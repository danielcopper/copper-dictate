use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::xdg::{self, Env};

#[derive(Debug, Error)]
#[error("XDG_RUNTIME_DIR is not set to an absolute path, so the daemon's socket has no place")]
pub(crate) struct NoRuntimeDir;

const SOCKET_FILE: &str = "daemon.sock";

pub(crate) fn socket_path(env: Env<'_>) -> Result<PathBuf, NoRuntimeDir> {
    xdg::runtime_dir(env)
        .map(|dir| dir.join("copper-dictate").join(SOCKET_FILE))
        .ok_or(NoRuntimeDir)
}

/// What `toggle` sends the daemon.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub(crate) enum Request {
    Toggle,
}

/// The daemon's answer to a request.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Reply {
    Recording,
    Text {
        text: String,
        #[serde(default)]
        glitches: u32,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Error)]
pub(crate) enum ReadError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Reads one message, or `None` when the other side closed the connection without one.
pub(crate) fn read_message<T: DeserializeOwned>(
    reader: &mut impl BufRead,
) -> Result<Option<T>, ReadError> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&line)?))
}

pub(crate) fn write_message(mut stream: impl Write, message: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    stream.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::{ReadError, Reply, Request, SOCKET_FILE, read_message, socket_path, write_message};
    use crate::test_support::production_code;

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
    fn a_message_is_written_as_one_line() {
        let mut stream = Vec::new();

        write_message(&mut stream, &Request::Toggle).unwrap();

        assert_eq!(stream, b"{\"command\":\"toggle\"}\n");
    }

    #[test]
    fn written_messages_read_back_one_at_a_time() {
        let mut stream = Vec::new();
        write_message(&mut stream, &Reply::Recording).unwrap();
        let next = Reply::Error {
            message: "the next line".to_owned(),
        };
        write_message(&mut stream, &next).unwrap();
        let mut reader = stream.as_slice();

        assert_eq!(read_message(&mut reader).unwrap(), Some(Reply::Recording));
        assert_eq!(read_message(&mut reader).unwrap(), Some(next));
        assert_eq!(read_message::<Reply>(&mut reader).unwrap(), None);
    }

    #[test]
    fn a_closed_connection_reads_as_no_message() {
        assert!(matches!(read_message::<Request>(&mut &b""[..]), Ok(None)));
    }

    #[test]
    fn a_line_that_is_no_message_reads_as_a_json_error() {
        assert!(matches!(
            read_message::<Request>(&mut &b"{\"command\":\"dance\"}\n"[..]),
            Err(ReadError::Json(_))
        ));
    }

    #[test]
    fn no_other_module_names_the_socket() {
        let naming: Vec<String> = production_code()
            .into_iter()
            .filter(|(name, code)| name != "daemon/protocol.rs" && code.contains(SOCKET_FILE))
            .map(|(name, _)| name)
            .collect();

        assert!(naming.is_empty(), "{naming:?}");
    }

    #[test]
    fn the_socket_clients_frame_messages_only_through_this_module() {
        let framing = [
            "read_line",
            "read_until",
            "write_all",
            "serde_json::to_",
            "serde_json::from_",
        ];
        let clients: Vec<(String, String)> = production_code()
            .into_iter()
            .filter(|(name, code)| name != "daemon/protocol.rs" && code.contains("UnixStream"))
            .collect();
        let framing_elsewhere: Vec<(&str, &str)> = clients
            .iter()
            .flat_map(|(name, code)| {
                framing
                    .iter()
                    .filter(|call| code.contains(**call))
                    .map(move |call| (name.as_str(), *call))
            })
            .collect();

        assert!(!clients.is_empty(), "no module uses a UnixStream");
        assert!(framing_elsewhere.is_empty(), "{framing_elsewhere:?}");
    }

    #[test]
    fn messages_are_tagged_json_objects() {
        assert_eq!(
            serde_json::to_string(&Request::Toggle).unwrap(),
            r#"{"command":"toggle"}"#
        );
        assert_eq!(
            serde_json::to_string(&Reply::Recording).unwrap(),
            r#"{"event":"recording"}"#
        );
        assert_eq!(
            serde_json::to_string(&Reply::Text {
                text: "hi".to_owned(),
                glitches: 2,
            })
            .unwrap(),
            r#"{"event":"text","text":"hi","glitches":2}"#
        );
        assert_eq!(
            serde_json::to_string(&Reply::Error {
                message: "no".to_owned()
            })
            .unwrap(),
            r#"{"event":"error","message":"no"}"#
        );
    }
}
