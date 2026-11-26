use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::{Agent, Body};

use crate::config::FinalServerConfig;
use crate::session::{BoxError, FinalServer, Recording};
use crate::wav;

/// An OpenAI-compatible `/v1/audio/transcriptions` endpoint.
///
/// No `language` field: the API has no value for automatic detection, so leaving
/// it out asks for it; a server with a default language of its own applies that
/// instead.
pub(crate) struct HttpFinalServer {
    agent: Agent,
    config: FinalServerConfig,
    answer_minute: Duration,
}

#[derive(Debug, Error)]
enum FinalServerError {
    #[error("the final server answered {status}: {body}")]
    Status { status: u16, body: String },
    #[error("the final server's answer holds no transcription: {0}")]
    Answer(#[from] serde_json::Error),
    #[error("the final server did not answer within {minutes} minutes")]
    NoAnswer { minutes: u32 },
}

struct Timeouts {
    connect: Duration,
    write: Duration,
    body: Duration,
    /// The unit of the answer bound; tests shorten it.
    answer_minute: Duration,
}

// These end a connection that does not open, a server that stops reading the audio and
// an answer body that takes longer than a minute; the wait for the answer, which a slow
// server spends transcribing, grows with the recording instead (`answer_minutes`).
const TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(10),
    write: Duration::from_secs(60),
    body: Duration::from_secs(60),
    answer_minute: Duration::from_mins(1),
};

const LEAST_ANSWER_MINUTES: u32 = 10;

const ERROR_ANSWER_LIMIT: usize = 1000;

#[derive(Deserialize)]
struct Transcription {
    text: String,
}

impl HttpFinalServer {
    pub(crate) fn new(config: FinalServerConfig) -> Self {
        Self::with_timeouts(config, &TIMEOUTS)
    }

    fn with_timeouts(config: FinalServerConfig, timeouts: &Timeouts) -> Self {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::PlatformVerifier)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        // No proxy from the environment and no redirects: the client connects to the
        // configured endpoint only.
        let agent = Agent::config_builder()
            .tls_config(tls)
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(timeouts.connect))
            .timeout_per_write(Some(timeouts.write))
            .timeout_recv_body(Some(timeouts.body))
            .build()
            .new_agent();
        Self {
            agent,
            config,
            answer_minute: timeouts.answer_minute,
        }
    }
}

fn answer_minutes(samples: usize) -> u32 {
    let seconds = u64::try_from(samples)
        .unwrap_or(u64::MAX)
        .div_ceil(u64::from(Recording::SAMPLE_RATE));
    u32::try_from(seconds.saturating_mul(2).div_ceil(60))
        .unwrap_or(u32::MAX)
        .max(LEAST_ANSWER_MINUTES)
}

impl FinalServer for HttpFinalServer {
    fn transcribe(&self, recording: &Recording) -> Result<String, BoxError> {
        let wav = wav::encode(&recording.samples)?;
        let (content_type, body) = multipart(&self.config.model, &wav);
        let mut request = self
            .agent
            .post(self.config.endpoint.clone())
            .header("Content-Type", content_type);
        if let Some(key) = &self.config.api_key {
            request = request.header("Authorization", key.bearer());
        }
        let minutes = answer_minutes(recording.samples.len());
        let request = request
            .config()
            .timeout_recv_response(Some(self.answer_minute.saturating_mul(minutes)))
            .build();
        let mut response = match request.send(&body[..]) {
            Err(ureq::Error::Timeout(ureq::Timeout::RecvResponse)) => {
                return Err(FinalServerError::NoAnswer { minutes }.into());
            }
            result => result?,
        };
        let status = response.status();
        if !status.is_success() {
            return Err(FinalServerError::Status {
                status: status.as_u16(),
                body: error_answer(response.body_mut()),
            }
            .into());
        }
        let answer = response.body_mut().read_to_string()?;
        let transcription: Transcription =
            serde_json::from_str(&answer).map_err(FinalServerError::from)?;
        Ok(transcription.text)
    }
}

fn error_answer(body: &mut Body) -> String {
    let mut bytes = Vec::new();
    let mut reader = body.as_reader().take(ERROR_ANSWER_LIMIT as u64 + 1);
    if let Err(error) = reader.read_to_end(&mut bytes) {
        return format!("(the answer cannot be read: {error})");
    }
    let cut = bytes.len() > ERROR_ANSWER_LIMIT;
    bytes.truncate(ERROR_ANSWER_LIMIT);
    let mut answer = String::from_utf8_lossy(&bytes).into_owned();
    if cut {
        answer.push('…');
    }
    answer
}

fn multipart(model: &str, wav: &[u8]) -> (String, Vec<u8>) {
    let boundary = boundary_absent_from(wav);
    let mut body = Vec::with_capacity(wav.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"model\"\r\n\r\n\
             {model}\r\n\
             --{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"recording.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn boundary_absent_from(data: &[u8]) -> String {
    let mut attempt = 0u64;
    loop {
        let boundary = format!("copper-dictate-{attempt:016x}");
        if !data
            .windows(boundary.len())
            .any(|window| window == boundary.as_bytes())
        {
            return boundary;
        }
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{HttpFinalServer, TIMEOUTS, Timeouts, answer_minutes, boundary_absent_from};
    use crate::config;
    use crate::session::{BoxError, FinalServer, Recording};
    use crate::test_support::{
        Decoded, FakeHttpServer, HttpAnswer, Part, TempDir, decode, final_server_config,
        read_request,
    };

    const SHORT: Duration = Duration::from_millis(300);

    fn server(url: &str, api_key: Option<&str>) -> HttpFinalServer {
        HttpFinalServer::new(final_server_config(url, api_key))
    }

    fn with_timeouts(url: &str, timeouts: &Timeouts) -> HttpFinalServer {
        HttpFinalServer::with_timeouts(final_server_config(url, None), timeouts)
    }

    fn short_silence() -> Recording {
        Recording {
            samples: vec![0; 160],
        }
    }

    /// Runs the request on its own thread, so a client that waits forever fails the test.
    fn transcribe_within(
        server: HttpFinalServer,
        recording: Recording,
        limit: Duration,
    ) -> Result<String, String> {
        let (sender, result) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(
                server
                    .transcribe(&recording)
                    .map_err(|error: BoxError| error.to_string()),
            );
        });
        result
            .recv_timeout(limit)
            .expect("the request ended within the limit")
    }

    fn server_that_never_answers() -> (String, mpsc::Receiver<Instant>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (sender, read) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            read_request(&mut BufReader::new(&stream));
            let _ = sender.send(Instant::now());
            thread::sleep(Duration::from_secs(30));
        });
        (url, read)
    }

    fn text_part(name: &str, value: &str) -> Part {
        Part {
            name: name.to_owned(),
            file_name: None,
            content_type: None,
            data: value.as_bytes().to_vec(),
        }
    }

    #[test]
    fn the_request_carries_the_audio_as_wav_the_model_and_the_key_and_no_language() {
        let fake = FakeHttpServer::start(vec![(200, r#"{"text":"Hallo Welt"}"#.to_owned())]);
        let samples: Vec<i16> = (-32..32).map(|n| n * 100).cycle().take(16_000).collect();

        let text = server(&fake.url, Some("sk-test"))
            .transcribe(&Recording {
                samples: samples.clone(),
            })
            .unwrap();

        assert_eq!(text, "Hallo Welt");
        let request = fake.requests.recv().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/audio/transcriptions");
        assert_eq!(request.header("authorization"), Some("Bearer sk-test"));
        let parts = request.multipart();
        let names: Vec<&str> = parts.iter().map(|part| part.name.as_str()).collect();
        assert_eq!(names, ["model", "file"], "no language field");
        assert_eq!(parts[0], text_part("model", "whisper-large-v3"));
        assert_eq!(parts[1].file_name.as_deref(), Some("recording.wav"));
        assert_eq!(parts[1].content_type.as_deref(), Some("audio/wav"));
        assert_eq!(
            decode(&parts[1].data),
            Decoded {
                format: 1,
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 16,
                samples,
            }
        );
    }

    #[test]
    fn a_key_from_the_environment_reaches_the_request_when_no_key_file_is_configured() {
        let fake = FakeHttpServer::start(vec![(200, r#"{"text":"ok"}"#.to_owned())]);
        let dir = TempDir::new();
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                "[final]\nendpoint = {:?}\nmodel = \"whisper-large-v3\"\n",
                fake.url
            ),
        )
        .unwrap();
        let env = |name: &str| {
            (name == "COPPER_DICTATE_FINAL_API_KEY").then(|| OsString::from("sk-from-env"))
        };
        let config = config::load(&config_path, &env).unwrap();

        HttpFinalServer::new(config.final_server)
            .transcribe(&Recording {
                samples: vec![0; 160],
            })
            .unwrap();

        assert_eq!(
            fake.requests.recv().unwrap().header("authorization"),
            Some("Bearer sk-from-env")
        );
    }

    #[test]
    fn without_a_key_the_request_has_no_authorization() {
        let fake = FakeHttpServer::start(vec![(200, r#"{"text":"ok"}"#.to_owned())]);

        server(&fake.url, None)
            .transcribe(&Recording {
                samples: vec![0; 160],
            })
            .unwrap();

        assert_eq!(fake.requests.recv().unwrap().header("authorization"), None);
    }

    #[test]
    fn an_error_status_is_reported_with_the_servers_answer() {
        let fake = FakeHttpServer::start(vec![(401, r#"{"error":"bad key"}"#.to_owned())]);

        let error = server(&fake.url, Some("wrong"))
            .transcribe(&Recording {
                samples: vec![0; 160],
            })
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            r#"the final server answered 401: {"error":"bad key"}"#
        );
    }

    #[test]
    fn an_answer_without_text_is_reported() {
        let fake = FakeHttpServer::start(vec![(200, r#"{"segments":[]}"#.to_owned())]);

        let error = server(&fake.url, None)
            .transcribe(&Recording {
                samples: vec![0; 160],
            })
            .unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with("the final server's answer holds no transcription"),
            "{error}"
        );
    }

    #[test]
    fn an_unreachable_server_is_reported() {
        let unused_port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();

        let result = server(
            &format!("http://{unused_port}/v1/audio/transcriptions"),
            None,
        )
        .transcribe(&Recording {
            samples: vec![0; 160],
        });

        assert!(result.is_err());
    }

    #[test]
    fn a_redirect_is_not_followed() {
        let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
        elsewhere.set_nonblocking(true).unwrap();
        let fake = FakeHttpServer::answering(vec![HttpAnswer {
            status: 302,
            headers: vec![(
                "Location",
                format!("http://{}/elsewhere", elsewhere.local_addr().unwrap()),
            )],
            body: b"moved".to_vec(),
        }]);

        let result = transcribe_within(
            server(&fake.url, None),
            short_silence(),
            Duration::from_secs(10),
        );

        assert_eq!(result.unwrap_err(), "the final server answered 302: moved");
        assert_eq!(
            elsewhere.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "nothing connected to the redirect's target"
        );
    }

    #[test]
    fn an_error_status_is_reported_even_when_its_answer_is_not_text() {
        let fake = FakeHttpServer::answering(vec![HttpAnswer {
            status: 502,
            headers: Vec::new(),
            body: vec![0xff, 0xfe, b'x'],
        }]);

        let error = server(&fake.url, None)
            .transcribe(&short_silence())
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "the final server answered 502: \u{fffd}\u{fffd}x"
        );
    }

    #[test]
    fn a_long_error_answer_is_cut_short() {
        let fake = FakeHttpServer::start(vec![(500, "x".repeat(100_000))]);

        let error = server(&fake.url, None)
            .transcribe(&short_silence())
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            format!("the final server answered 500: {}…", "x".repeat(1000))
        );
    }

    #[test]
    fn a_server_that_never_answers_times_out_and_names_the_bound() {
        let (url, _read) = server_that_never_answers();
        let timeouts = Timeouts {
            answer_minute: Duration::from_millis(30),
            ..TIMEOUTS
        };

        let result = transcribe_within(
            with_timeouts(&url, &timeouts),
            short_silence(),
            Duration::from_secs(10),
        );

        assert_eq!(
            result.unwrap_err(),
            "the final server did not answer within 10 minutes"
        );
    }

    #[test]
    fn a_long_recording_gives_the_server_longer_to_answer() {
        let (url, read) = server_that_never_answers();
        let timeouts = Timeouts {
            answer_minute: Duration::from_millis(100),
            ..TIMEOUTS
        };

        let result = transcribe_within(
            with_timeouts(&url, &timeouts),
            Recording {
                samples: vec![0; 10 * 60 * 16_000],
            },
            Duration::from_secs(30),
        );
        let waited = read.recv().unwrap().elapsed();

        assert_eq!(
            result.unwrap_err(),
            "the final server did not answer within 20 minutes"
        );
        assert!(
            waited > Duration::from_millis(1500),
            "waited {waited:?}: twenty shortened minutes, not ten"
        );
    }

    #[test]
    fn the_answer_bound_is_twice_the_recording_and_at_least_ten_minutes() {
        let minute = 60 * 16_000;

        assert_eq!(answer_minutes(0), 10);
        assert_eq!(answer_minutes(5 * minute), 10);
        assert_eq!(answer_minutes(6 * minute), 12);
        assert_eq!(answer_minutes(6 * minute + 1), 13, "rounded up");
        assert_eq!(answer_minutes(15 * minute), 30);
    }

    #[test]
    fn a_server_that_stops_reading_the_audio_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(30));
        });
        let timeouts = Timeouts {
            write: SHORT,
            ..TIMEOUTS
        };

        let result = transcribe_within(
            with_timeouts(&url, &timeouts),
            Recording {
                samples: vec![0; 600 * 16_000],
            },
            Duration::from_secs(10),
        );

        let error = result.unwrap_err();
        assert!(error.contains("timeout"), "{error}");
    }

    #[test]
    fn an_answer_that_stops_halfway_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut BufReader::new(&stream));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"text\"")
                .unwrap();
            thread::sleep(Duration::from_secs(30));
        });
        let timeouts = Timeouts {
            body: SHORT,
            ..TIMEOUTS
        };

        let result = transcribe_within(
            with_timeouts(&url, &timeouts),
            short_silence(),
            Duration::from_secs(10),
        );

        let error = result.unwrap_err();
        assert!(error.contains("timeout"), "{error}");
    }

    #[test]
    fn the_boundary_never_occurs_in_the_audio() {
        let data = b"xx copper-dictate-0000000000000000 copper-dictate-0000000000000001 yy";

        assert_eq!(
            boundary_absent_from(data),
            "copper-dictate-0000000000000002"
        );
    }
}
