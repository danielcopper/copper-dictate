use std::fs::{self, DirBuilder};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use thiserror::Error;

use crate::protocol::{Reply, Request};
use crate::session::{Outcome, Session};

#[derive(Debug, Error)]
pub(crate) enum BindError {
    #[error("a daemon is already running at {0}")]
    AlreadyRunning(PathBuf),
    #[error("cannot listen at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

/// Listens at `socket`, taking over a socket file a daemon that ended left behind.
pub(crate) fn bind(socket: &Path) -> Result<UnixListener, BindError> {
    let io_error = |source| BindError::Io {
        path: socket.to_owned(),
        source,
    };
    if let Some(dir) = socket.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io_error)?;
    }
    match UnixListener::bind(socket) {
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(socket).is_ok() {
                return Err(BindError::AlreadyRunning(socket.to_owned()));
            }
            fs::remove_file(socket).map_err(io_error)?;
            UnixListener::bind(socket).map_err(io_error)
        }
        result => result.map_err(io_error),
    }
}

pub(crate) fn serve(listener: &UnixListener, session: &Arc<Session>) -> io::Result<()> {
    for connection in listener.incoming() {
        let connection = connection?;
        let session = Arc::clone(session);
        thread::spawn(move || {
            if let Err(error) = handle(&connection, &session) {
                eprintln!("copper-dictate: a toggle's connection failed: {error}");
            }
        });
    }
    Ok(())
}

fn handle(connection: &UnixStream, session: &Arc<Session>) -> io::Result<()> {
    let mut line = String::new();
    BufReader::new(connection).read_line(&mut line)?;
    let reply = match serde_json::from_str::<Request>(&line) {
        Ok(Request::Toggle) => match session.toggle() {
            Outcome::Started => Reply::Recording,
            Outcome::Text(text) => Reply::Text { text },
            Outcome::Failed(message) => Reply::Error { message },
        },
        Err(error) => Reply::Error {
            message: format!("not a request: {error}"),
        },
    };
    let mut answer = serde_json::to_string(&reply)?;
    answer.push('\n');
    let mut connection = connection;
    connection.write_all(answer.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use super::{BindError, bind, serve};
    use crate::config::FinalServerConfig;
    use crate::final_server::HttpFinalServer;
    use crate::session::Session;
    use crate::session::fakes::{FakeFinalServer, FakeMicrophone};
    use crate::test_support::{FakeHttpServer, TempDir};
    use crate::toggle;
    use crate::wav::tests::decode;

    const NO_LIMIT: Duration = Duration::from_secs(3600);

    struct Daemon {
        runtime_dir: TempDir,
    }

    impl Daemon {
        fn start(session: Arc<Session>) -> Self {
            let runtime_dir = TempDir::new();
            let listener = bind(&socket_in(&runtime_dir)).unwrap();
            thread::spawn(move || serve(&listener, &session));
            Self { runtime_dir }
        }

        /// Runs `copper-dictate toggle` against this daemon: (succeeded, stdout, stderr).
        fn toggle(&self) -> (bool, String, String) {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let succeeded = toggle::run(&socket_in(&self.runtime_dir), &mut out, &mut err);
            (
                succeeded,
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        }
    }

    fn socket_in(runtime_dir: &TempDir) -> std::path::PathBuf {
        let runtime_dir = runtime_dir.path().as_os_str().to_owned();
        crate::protocol::socket_path(&|name: &str| {
            (name == "XDG_RUNTIME_DIR").then(|| runtime_dir.clone())
        })
        .unwrap()
    }

    fn http_final_server(url: &str) -> HttpFinalServer {
        HttpFinalServer::new(FinalServerConfig {
            endpoint: url.to_owned(),
            model: "whisper-large-v3".to_owned(),
            api_key: Some("sk-test".to_owned()),
        })
    }

    fn tone(seconds: usize) -> Vec<i16> {
        (0..seconds * 16_000)
            .map(|n| if n % 40 < 20 { 4000 } else { -4000 })
            .collect()
    }

    #[test]
    fn two_toggles_print_the_text_the_final_server_returned_for_the_audio() {
        let fake = FakeHttpServer::start(vec![(200, r#"{"text":"Hallo Welt"}"#.to_owned())]);
        let (microphone, events) = FakeMicrophone::new(tone(3));
        let daemon = Daemon::start(Session::new(
            Box::new(microphone),
            Box::new(http_final_server(&fake.url)),
            NO_LIMIT,
        ));

        let (started, out, _) = daemon.toggle();
        assert!(started);
        assert_eq!(out, "");
        assert_eq!(events.try_recv(), Ok("start"));

        assert_eq!(
            daemon.toggle(),
            (true, "Hallo Welt\n".to_owned(), String::new())
        );
        let request = fake.requests.recv().unwrap();
        let file = request.multipart().pop().unwrap();
        assert_eq!(decode(&file.data).samples, tone(3));
    }

    #[test]
    fn a_ten_minute_recording_is_sent_whole_and_its_text_printed() {
        let fake = FakeHttpServer::start(vec![(200, r#"{"text":"ten minutes"}"#.to_owned())]);
        let (microphone, _events) = FakeMicrophone::new(tone(600));
        let daemon = Daemon::start(Session::new(
            Box::new(microphone),
            Box::new(http_final_server(&fake.url)),
            Duration::from_mins(15),
        ));

        daemon.toggle();
        assert_eq!(
            daemon.toggle(),
            (true, "ten minutes\n".to_owned(), String::new())
        );

        let request = fake.requests.recv().unwrap();
        let samples = decode(&request.multipart().pop().unwrap().data).samples;
        assert_eq!(samples.len(), 600 * 16_000);
        assert!(samples == tone(600), "the samples arrive unchanged");
    }

    #[test]
    fn a_recording_that_reaches_the_limit_stops_by_itself_and_its_text_is_printed() {
        let fake = FakeHttpServer::start(vec![(200, r#"{"text":"cut at the limit"}"#.to_owned())]);
        let (microphone, events) = FakeMicrophone::new(tone(1));
        let daemon = Daemon::start(Session::new(
            Box::new(microphone),
            Box::new(http_final_server(&fake.url)),
            Duration::from_millis(200),
        ));

        daemon.toggle();
        assert_eq!(events.recv_timeout(Duration::from_secs(10)), Ok("start"));
        assert_eq!(events.recv_timeout(Duration::from_secs(10)), Ok("stop"));
        let request = fake.requests.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            decode(&request.multipart().pop().unwrap().data).samples,
            tone(1)
        );

        assert_eq!(
            daemon.toggle(),
            (true, "cut at the limit\n".to_owned(), String::new())
        );
        assert!(
            events.try_recv().is_err(),
            "the toggle started no new recording"
        );
    }

    #[test]
    fn a_failed_dictation_is_reported_on_stderr_and_fails() {
        let (microphone, _events) = FakeMicrophone::new(vec![1]);
        let daemon = Daemon::start(Session::new(
            Box::new(microphone),
            Box::new(FakeFinalServer::answering(&[Err("timed out")])),
            NO_LIMIT,
        ));

        daemon.toggle();
        let (succeeded, out, err) = daemon.toggle();

        assert!(!succeeded);
        assert_eq!(out, "");
        assert_eq!(err, "copper-dictate: the final server failed: timed out\n");
    }

    #[test]
    fn a_line_that_is_no_request_gets_an_error() {
        let (microphone, _events) = FakeMicrophone::new(vec![1]);
        let daemon = Daemon::start(Session::new(
            Box::new(microphone),
            Box::new(FakeFinalServer::answering(&[])),
            NO_LIMIT,
        ));
        let mut stream = UnixStream::connect(socket_in(&daemon.runtime_dir)).unwrap();

        stream.write_all(b"{\"command\":\"dance\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();

        assert!(
            line.starts_with(r#"{"event":"error","message":"not a request"#),
            "{line}"
        );
    }

    #[test]
    fn the_socket_directory_is_private() {
        let runtime_dir = TempDir::new();
        let socket = socket_in(&runtime_dir);

        let _listener = bind(&socket).unwrap();

        let mode = socket
            .parent()
            .unwrap()
            .metadata()
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_second_daemon_does_not_take_over_a_running_ones_socket() {
        let runtime_dir = TempDir::new();
        let socket = socket_in(&runtime_dir);
        let _running = bind(&socket).unwrap();

        assert!(matches!(bind(&socket), Err(BindError::AlreadyRunning(path)) if path == socket));
    }

    #[test]
    fn a_socket_left_behind_by_an_ended_daemon_is_taken_over() {
        let runtime_dir = TempDir::new();
        let socket = socket_in(&runtime_dir);
        drop(bind(&socket).unwrap());
        assert!(socket.exists());

        let _listener = bind(&socket).unwrap();

        assert!(UnixStream::connect(&socket).is_ok());
    }
}
