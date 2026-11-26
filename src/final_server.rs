use std::sync::Arc;

use serde::Deserialize;
use thiserror::Error;
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use crate::config::FinalServerConfig;
use crate::session::{BoxError, FinalServer, Recording};
use crate::wav;

/// An OpenAI-compatible `/v1/audio/transcriptions` endpoint.
///
/// The request carries no `language` field: the API has no value for automatic
/// detection, and leaving the field out is what selects it.
pub(crate) struct HttpFinalServer {
    agent: Agent,
    config: FinalServerConfig,
}

#[derive(Debug, Error)]
enum FinalServerError {
    #[error("the final server answered {status}: {body}")]
    Status { status: u16, body: String },
    #[error("the final server's answer holds no transcription: {0}")]
    Answer(#[from] serde_json::Error),
}

#[derive(Deserialize)]
struct Transcription {
    text: String,
}

impl HttpFinalServer {
    pub(crate) fn new(config: FinalServerConfig) -> Self {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::PlatformVerifier)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        // No proxy from the environment: the client connects to the configured endpoint only.
        let agent = Agent::config_builder()
            .tls_config(tls)
            .proxy(None)
            .http_status_as_error(false)
            .build()
            .new_agent();
        Self { agent, config }
    }
}

impl FinalServer for HttpFinalServer {
    fn transcribe(&self, recording: &Recording) -> Result<String, BoxError> {
        let wav = wav::encode(&recording.samples)?;
        let (content_type, body) = multipart(&self.config.model, &wav);
        let mut request = self
            .agent
            .post(&self.config.endpoint)
            .header("Content-Type", content_type);
        if let Some(key) = &self.config.api_key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        let mut response = request.send(&body[..])?;
        let status = response.status();
        let answer = response.body_mut().read_to_string()?;
        if !status.is_success() {
            return Err(FinalServerError::Status {
                status: status.as_u16(),
                body: answer,
            }
            .into());
        }
        let transcription: Transcription =
            serde_json::from_str(&answer).map_err(FinalServerError::from)?;
        Ok(transcription.text)
    }
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

    use super::{HttpFinalServer, boundary_absent_from};
    use crate::config::{self, FinalServerConfig};
    use crate::session::{FinalServer, Recording};
    use crate::test_support::{FakeHttpServer, Part, TempDir};
    use crate::wav::tests::{Decoded, decode};

    fn server(url: &str, api_key: Option<&str>) -> HttpFinalServer {
        HttpFinalServer::new(FinalServerConfig {
            endpoint: url.to_owned(),
            model: "whisper-large-v3".to_owned(),
            api_key: api_key.map(str::to_owned),
        })
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
        let unused_port = std::net::TcpListener::bind("127.0.0.1:0")
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
    fn the_boundary_never_occurs_in_the_audio() {
        let data = b"xx copper-dictate-0000000000000000 copper-dictate-0000000000000001 yy";

        assert_eq!(
            boundary_absent_from(data),
            "copper-dictate-0000000000000002"
        );
    }
}
