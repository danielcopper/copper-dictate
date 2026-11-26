use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::daemon::protocol::{ReadError, Reply, Request, read_message, write_message};

#[derive(Debug, Error)]
enum ToggleError {
    #[error(
        "the daemon is not running (no daemon listens at {0}); start it with `copper-dictate daemon`"
    )]
    NotRunning(PathBuf),
    #[error("the daemon closed the connection without an answer; is it still running?")]
    NoAnswer,
    #[error("talking to the daemon failed: {0}")]
    Io(#[from] io::Error),
    #[error("the daemon's answer is not understood: {0}")]
    Answer(#[from] serde_json::Error),
}

pub(crate) fn run(socket: &Path, out: &mut dyn Write, err: &mut dyn Write) -> bool {
    match send(socket) {
        Ok(Reply::Text { text, glitches }) => match writeln!(out, "{text}") {
            Ok(()) => {
                warn_of_glitches(glitches, err);
                true
            }
            Err(error) => {
                let _ = writeln!(err, "copper-dictate: cannot print the final text: {error}");
                false
            }
        },
        Ok(Reply::Recording) => {
            let _ = writeln!(err, "copper-dictate: recording; toggle again to stop");
            true
        }
        Ok(Reply::Error { message }) => {
            let _ = writeln!(err, "copper-dictate: {message}");
            false
        }
        Err(error) => {
            let _ = writeln!(err, "copper-dictate: {error}");
            false
        }
    }
}

fn warn_of_glitches(glitches: u32, err: &mut dyn Write) {
    let times = match glitches {
        0 => return,
        1 => "once".to_owned(),
        n => format!("{n} times"),
    };
    let _ = writeln!(
        err,
        "copper-dictate: warning: the microphone glitched {times} during the recording; parts of the audio may be missing"
    );
}

fn send(socket: &Path) -> Result<Reply, ToggleError> {
    let stream = UnixStream::connect(socket).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            ToggleError::NotRunning(socket.to_owned())
        }
        _ => ToggleError::Io(error),
    })?;
    write_message(&stream, &Request::Toggle)?;
    match read_message(&stream) {
        Ok(Some(reply)) => Ok(reply),
        Ok(None) => Err(ToggleError::NoAnswer),
        Err(ReadError::Io(error)) => Err(ToggleError::Io(error)),
        Err(ReadError::Json(error)) => Err(ToggleError::Answer(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::thread;

    use super::run;
    use crate::test_support::TempDir;

    fn daemon_answering(answer: &'static [u8]) -> (TempDir, PathBuf) {
        let runtime_dir = TempDir::new();
        let socket = runtime_dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            BufReader::new(&stream)
                .read_line(&mut String::new())
                .unwrap();
            stream.write_all(answer).unwrap();
        });
        (runtime_dir, socket)
    }

    struct BrokenOutput;

    impl Write for BrokenOutput {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_daemon_that_closes_without_an_answer_is_named() {
        let (_runtime_dir, socket) = daemon_answering(b"");
        let mut err = Vec::new();

        assert!(!run(&socket, &mut Vec::new(), &mut err));

        assert_eq!(
            String::from_utf8(err).unwrap(),
            "copper-dictate: the daemon closed the connection without an answer; is it still running?\n"
        );
    }

    #[test]
    fn a_final_text_that_cannot_be_printed_is_reported() {
        let (_runtime_dir, socket) = daemon_answering(b"{\"event\":\"text\",\"text\":\"hi\"}\n");
        let mut err = Vec::new();

        assert!(!run(&socket, &mut BrokenOutput, &mut err));

        let err = String::from_utf8(err).unwrap();
        assert!(
            err.starts_with("copper-dictate: cannot print the final text: "),
            "{err}"
        );
    }

    #[test]
    fn without_a_daemon_it_fails_and_says_the_daemon_is_not_running() {
        let runtime_dir = TempDir::new();
        let socket = runtime_dir.path().join("daemon.sock");
        let mut out = Vec::new();
        let mut err = Vec::new();

        let succeeded = run(&socket, &mut out, &mut err);

        assert!(!succeeded);
        assert_eq!(out, b"");
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("the daemon is not running"), "{err}");
    }

    #[test]
    fn a_socket_nobody_listens_at_counts_as_no_daemon() {
        let runtime_dir = TempDir::new();
        let socket = runtime_dir.path().join("daemon.sock");
        drop(UnixListener::bind(&socket).unwrap());
        let mut err = Vec::new();

        assert!(!run(&socket, &mut Vec::new(), &mut err));
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("the daemon is not running")
        );
    }
}
