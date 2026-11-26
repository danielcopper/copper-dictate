use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::Env;

/// What `toggle` sends the daemon: one JSON object per line.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub(crate) enum Request {
    Toggle,
}

/// The daemon's one-line answer to a request.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Reply {
    Recording,
    Text { text: String },
    Error { message: String },
}

#[derive(Debug, Error)]
#[error("XDG_RUNTIME_DIR is not set to an absolute path, so the daemon's socket has no place")]
pub(crate) struct NoRuntimeDir;

pub(crate) fn socket_path(env: Env<'_>) -> Result<PathBuf, NoRuntimeDir> {
    env("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("copper-dictate").join("daemon.sock"))
        .ok_or(NoRuntimeDir)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::{Reply, Request, socket_path};

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
                text: "hi".to_owned()
            })
            .unwrap(),
            r#"{"event":"text","text":"hi"}"#
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
