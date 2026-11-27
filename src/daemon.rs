pub(crate) mod protocol;
pub(crate) mod toggle;

use std::fs::{self, DirBuilder};
use std::io::{self, BufReader};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use thiserror::Error;

use crate::daemon::protocol::{ReadError, Reply, Request, read_message, write_message};
use crate::log;
use crate::session::{Outcome, Session};

const ACCEPT_RETRY: Duration = Duration::from_millis(100);

#[derive(Debug, Error)]
pub(crate) enum BindError {
    #[error("a daemon is already running at {0}")]
    AlreadyRunning(PathBuf),
    #[error("cannot listen at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

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

pub(crate) fn serve(listener: &UnixListener, session: &Arc<Session>) {
    serve_connections(listener.incoming(), session);
}

fn serve_connections(
    connections: impl IntoIterator<Item = io::Result<UnixStream>>,
    session: &Arc<Session>,
) {
    for connection in connections {
        match connection {
            Ok(connection) => {
                let session = Arc::clone(session);
                thread::spawn(move || {
                    if let Err(error) = handle(&connection, &session) {
                        log::eprint(format_args!("a toggle's connection failed: {error}"));
                    }
                });
            }
            Err(error) => {
                log::eprint(format_args!("cannot accept a toggle's connection: {error}"));
                // Errors such as running out of file descriptors last a while; the pause keeps
                // the loop from spinning through them.
                thread::sleep(ACCEPT_RETRY);
            }
        }
    }
}

fn handle(connection: &UnixStream, session: &Arc<Session>) -> io::Result<()> {
    let outcome = match read_message::<Request>(&mut BufReader::new(connection)) {
        Ok(None) => return Ok(()),
        Ok(Some(Request::Toggle)) => session.toggle(),
        Err(ReadError::Io(error)) => return Err(error),
        Err(ReadError::Json(error)) => {
            return write_message(
                connection,
                &Reply::Error {
                    message: format!("not a request: {error}"),
                },
            );
        }
    };
    let reply = match &outcome {
        Outcome::Started => Reply::Recording,
        Outcome::Done(Ok(final_text)) => Reply::Text {
            text: final_text.text.clone(),
            glitches: final_text.glitches,
        },
        Outcome::Refused(message) | Outcome::Done(Err(message)) => Reply::Error {
            message: message.clone(),
        },
    };
    let Err(error) = write_message(connection, &reply) else {
        return Ok(());
    };
    let Outcome::Done(transcript) = outcome else {
        return Err(error);
    };
    if session.keep(transcript) {
        log::eprint(
            "the toggle went away before its final text or failure arrived; the next toggle prints it",
        );
    } else {
        log::eprint(
            "the toggle went away before its final text or failure arrived, and a new dictation has begun; it is lost",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::Shutdown;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use super::{BindError, bind, serve, serve_connections};
    use crate::daemon::protocol::socket_path;
    use crate::daemon::toggle;
    use crate::servers::final_server::HttpFinalServer;
    use crate::session::{Outcome, Session};
    use crate::test_support::{
        BrokenMicrophone, FakeFinalServer, FakeHttpServer, FakeMicrophone, TempDir, decode,
        final_server_config, wait_until,
    };

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

    fn socket_in(runtime_dir: &TempDir) -> PathBuf {
        let runtime_dir = runtime_dir.path().as_os_str().to_owned();
        socket_path(&|name: &str| (name == "XDG_RUNTIME_DIR").then(|| runtime_dir.clone())).unwrap()
    }

    fn http_final_server(url: &str) -> HttpFinalServer {
        HttpFinalServer::new(final_server_config(url, Some("sk-test")))
    }

    fn toggle_and_go_away(session: &Arc<Session>) {
        let (client, accepted) = UnixStream::pair().unwrap();
        (&client).write_all(b"{\"command\":\"toggle\"}\n").unwrap();
        drop(client);
        let _ = super::handle(&accepted, session);
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
    fn a_text_whose_toggle_went_away_is_printed_by_the_next_toggle() {
        let (microphone, events) = FakeMicrophone::new(vec![1]);
        let (final_server, release) = FakeFinalServer::answering(&[Ok("kept for later")]).held();
        let received = final_server.received.clone();
        let session = Session::new(Box::new(microphone), Box::new(final_server), NO_LIMIT);
        let daemon = Daemon::start(Arc::clone(&session));
        daemon.toggle();
        let mut gone = UnixStream::connect(socket_in(&daemon.runtime_dir)).unwrap();
        gone.write_all(b"{\"command\":\"toggle\"}\n").unwrap();
        wait_until(|| !received.lock().unwrap().is_empty());

        drop(gone);
        release.send(()).unwrap();
        wait_until(|| session.holds_a_transcript());

        assert_eq!(
            daemon.toggle(),
            (true, "kept for later\n".to_owned(), String::new())
        );
        assert_eq!(events.try_iter().collect::<Vec<_>>(), ["start", "stop"]);
    }

    #[test]
    fn a_failure_whose_toggle_went_away_is_printed_by_the_next_toggle() {
        let (microphone, events) = FakeMicrophone::new(vec![1]);
        let session = Session::new(
            Box::new(microphone),
            Box::new(FakeFinalServer::answering(&[Err("timed out")])),
            NO_LIMIT,
        );
        let daemon = Daemon::start(Arc::clone(&session));
        daemon.toggle();

        toggle_and_go_away(&session);

        assert_eq!(
            daemon.toggle(),
            (
                false,
                String::new(),
                "copper-dictate: the final server failed: timed out\n".to_owned()
            )
        );
        assert_eq!(events.try_iter().collect::<Vec<_>>(), ["start", "stop"]);
    }

    #[test]
    fn a_refusal_whose_toggle_went_away_is_never_kept() {
        let session = Session::new(
            Box::new(BrokenMicrophone),
            Box::new(FakeFinalServer::answering(&[])),
            NO_LIMIT,
        );

        toggle_and_go_away(&session);

        assert_eq!(
            session.toggle(),
            Outcome::Refused("cannot start the recording: no input device".to_owned()),
            "the next toggle tries to start a recording again"
        );
    }

    #[test]
    fn a_dictation_the_microphone_glitched_in_prints_its_text_and_warns_how_often() {
        let (microphone, _events) = FakeMicrophone::glitching(vec![1], 3);
        let daemon = Daemon::start(Session::new(
            Box::new(microphone),
            Box::new(FakeFinalServer::answering(&[Ok("despite the glitches")])),
            NO_LIMIT,
        ));

        daemon.toggle();

        assert_eq!(
            daemon.toggle(),
            (
                true,
                "despite the glitches\n".to_owned(),
                "copper-dictate: warning: the microphone glitched 3 times during the recording; parts of the audio may be missing\n".to_owned()
            )
        );
    }

    #[test]
    fn the_glitch_warning_of_a_recording_stopped_at_the_limit_reaches_the_next_toggle() {
        let (microphone, _events) = FakeMicrophone::glitching(vec![1], 1);
        let session = Session::new(
            Box::new(microphone),
            Box::new(FakeFinalServer::answering(&[Ok("cut at the limit")])),
            Duration::from_millis(50),
        );
        let daemon = Daemon::start(Arc::clone(&session));
        daemon.toggle();
        wait_until(|| session.holds_a_transcript());

        assert_eq!(
            daemon.toggle(),
            (
                true,
                "cut at the limit\n".to_owned(),
                "copper-dictate: warning: the microphone glitched once during the recording; parts of the audio may be missing\n".to_owned()
            )
        );
    }

    #[test]
    fn the_glitch_warning_of_a_text_whose_toggle_went_away_reaches_the_next_toggle() {
        let (microphone, _events) = FakeMicrophone::glitching(vec![1], 2);
        let (final_server, release) = FakeFinalServer::answering(&[Ok("kept for later")]).held();
        let received = final_server.received.clone();
        let session = Session::new(Box::new(microphone), Box::new(final_server), NO_LIMIT);
        let daemon = Daemon::start(Arc::clone(&session));
        daemon.toggle();
        let mut gone = UnixStream::connect(socket_in(&daemon.runtime_dir)).unwrap();
        gone.write_all(b"{\"command\":\"toggle\"}\n").unwrap();
        wait_until(|| !received.lock().unwrap().is_empty());

        drop(gone);
        release.send(()).unwrap();
        wait_until(|| session.holds_a_transcript());

        let (succeeded, out, err) = daemon.toggle();
        assert!(succeeded);
        assert_eq!(out, "kept for later\n");
        assert!(err.contains("the microphone glitched 2 times"), "{err}");
    }

    #[test]
    fn an_accept_error_does_not_end_the_daemon() {
        let (microphone, _events) = FakeMicrophone::new(vec![1]);
        let session = Session::new(
            Box::new(microphone),
            Box::new(FakeFinalServer::answering(&[])),
            NO_LIMIT,
        );
        let (client, accepted) = UnixStream::pair().unwrap();
        thread::spawn(move || {
            serve_connections(
                [
                    Err(io::Error::from(io::ErrorKind::ConnectionAborted)),
                    Ok(accepted),
                ],
                &session,
            );
        });

        (&client).write_all(b"{\"command\":\"toggle\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).unwrap();

        assert_eq!(line, "{\"event\":\"recording\"}\n");
    }

    #[test]
    fn a_connection_closed_without_a_request_gets_no_answer() {
        let (microphone, _events) = FakeMicrophone::new(vec![1]);
        let daemon = Daemon::start(Session::new(
            Box::new(microphone),
            Box::new(FakeFinalServer::answering(&[])),
            NO_LIMIT,
        ));
        let stream = UnixStream::connect(socket_in(&daemon.runtime_dir)).unwrap();

        stream.shutdown(Shutdown::Write).unwrap();
        let mut answer = String::new();
        (&stream).read_to_string(&mut answer).unwrap();

        assert_eq!(answer, "");
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
