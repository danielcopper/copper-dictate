use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::protocol::{Reply, Request};

#[derive(Debug, Error)]
enum ToggleError {
    #[error(
        "the daemon is not running (no daemon listens at {0}); start it with `copper-dictate daemon`"
    )]
    NotRunning(PathBuf),
    #[error("talking to the daemon failed: {0}")]
    Io(#[from] io::Error),
    #[error("the daemon's answer is not understood: {0}")]
    Answer(#[from] serde_json::Error),
}

pub(crate) fn run(socket: &Path, out: &mut dyn Write, err: &mut dyn Write) -> bool {
    match send(socket) {
        Ok(Reply::Text { text }) => writeln!(out, "{text}").is_ok(),
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

fn send(socket: &Path) -> Result<Reply, ToggleError> {
    let mut stream = UnixStream::connect(socket).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            ToggleError::NotRunning(socket.to_owned())
        }
        _ => ToggleError::Io(error),
    })?;
    let mut request = serde_json::to_string(&Request::Toggle)?;
    request.push('\n');
    stream.write_all(request.as_bytes())?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::run;
    use crate::test_support::TempDir;

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
