use std::fs::{self, DirBuilder};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use thiserror::Error;

use crate::protocol::{Reply, Request};
use crate::session::{Outcome, Session};
use crate::xdg::{self, Env};

const ACCEPT_RETRY: Duration = Duration::from_millis(100);

#[derive(Debug, Error)]
#[error("XDG_RUNTIME_DIR is not set to an absolute path, so the daemon's socket has no place")]
pub(crate) struct NoRuntimeDir;

pub(crate) fn socket_path(env: Env<'_>) -> Result<PathBuf, NoRuntimeDir> {
    xdg::runtime_dir(env)
        .map(|dir| dir.join("copper-dictate").join("daemon.sock"))
        .ok_or(NoRuntimeDir)
}

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
                        eprintln!("copper-dictate: a toggle's connection failed: {error}");
                    }
                });
            }
            Err(error) => {
                eprintln!("copper-dictate: cannot accept a toggle's connection: {error}");
                // Errors such as running out of file descriptors last a while; the pause keeps
                // the loop from spinning through them.
                thread::sleep(ACCEPT_RETRY);
            }
        }
    }
}

fn handle(connection: &UnixStream, session: &Arc<Session>) -> io::Result<()> {
    let mut line = String::new();
    if BufReader::new(connection).read_line(&mut line)? == 0 {
        return Ok(());
    }
    let outcome = match serde_json::from_str::<Request>(&line) {
        Ok(Request::Toggle) => session.toggle(),
        Err(error) => {
            return send(
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
    let Err(error) = send(connection, &reply) else {
        return Ok(());
    };
    let Outcome::Done(transcript) = outcome else {
        return Err(error);
    };
    if session.keep(transcript) {
        eprintln!(
            "copper-dictate: the toggle went away before its final text or failure arrived; the next toggle prints it"
        );
    } else {
        eprintln!(
            "copper-dictate: the toggle went away before its final text or failure arrived, and a new dictation has begun; it is lost"
        );
    }
    Ok(())
}

fn send(mut connection: &UnixStream, reply: &Reply) -> io::Result<()> {
    let mut answer = serde_json::to_string(reply)?;
    answer.push('\n');
    connection.write_all(answer.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::Shutdown;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use std::path::{Path, PathBuf};

    use super::{BindError, bind, serve, serve_connections, socket_path};
    use crate::final_server::HttpFinalServer;
    use crate::session::{Outcome, Session};
    use crate::test_support::{
        BrokenMicrophone, FakeFinalServer, FakeHttpServer, FakeMicrophone, TempDir, decode,
        final_server_config, wait_until,
    };
    use crate::toggle;

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
    fn the_socket_lies_in_the_runtime_directory() {
        let env =
            |name: &str| (name == "XDG_RUNTIME_DIR").then(|| OsString::from("/run/user/1000"));

        assert_eq!(
            socket_path(&env).unwrap(),
            Path::new("/run/user/1000/copper-dictate/daemon.sock")
        );
    }

    #[test]
    fn without_a_runtime_directory_there_is_no_socket() {
        assert!(socket_path(&|_: &str| None).is_err());
        assert!(socket_path(&|_: &str| Some(OsString::from("relative"))).is_err());
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
