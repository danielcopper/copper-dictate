use serde::{Deserialize, Serialize};

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
    Text {
        text: String,
        #[serde(default)]
        glitches: u32,
    },
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{Reply, Request};

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
