mod contents;
mod data_control;
mod kde;
mod keyboard;
mod remote_desktop;

use std::fmt::Display;
use std::io;
use std::time::Instant;

use rustix::event::{PollFd, Timespec, poll};

use crate::config::InsertionConfig;
use crate::log;
use crate::session::Insertion;
use crate::xdg::Env;

/// Where the desktop offers no way to insert: the final text reaches only `toggle`'s output.
pub(crate) struct NoInsertion;

impl Insertion for NoInsertion {
    fn insert(&self, _text: String) {}
}

/// The insertion this desktop allows; where there is none, it says why once and inserts nowhere.
/// Only the check for a Wayland session goes through `env`: the connections to the compositor
/// and the session bus read the process's environment themselves.
pub(crate) fn insertion(env: Env<'_>, config: &InsertionConfig) -> Box<dyn Insertion> {
    if env("WAYLAND_DISPLAY").is_none_or(|display| display.is_empty()) {
        cannot_insert("no Wayland session (WAYLAND_DISPLAY is not set)");
        return Box::new(NoInsertion);
    }
    match kde::start(config, remote_desktop::token_path(env)) {
        Ok(paste) => Box::new(paste),
        Err(error) => {
            cannot_insert(format_args!("the insertion does not start: {error}"));
            Box::new(NoInsertion)
        }
    }
}

fn cannot_insert(reason: impl Display) {
    log::eprint(format_args!(
        "cannot insert at the cursor: {reason}; the final text reaches only toggle's output"
    ));
}

/// Waits until one of the descriptors is ready or the deadline passes, and says whether one
/// is; a wait a signal interrupts ends with none ready.
fn poll_until(fds: &mut [PollFd<'_>], until: Option<Instant>) -> io::Result<bool> {
    let timeout = until
        .map(|until| Timespec::try_from(until.saturating_duration_since(Instant::now())))
        .transpose()
        .map_err(io::Error::other)?;
    match poll(fds, timeout.as_ref()) {
        Ok(ready) => Ok(ready > 0),
        Err(rustix::io::Errno::INTR) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::time::{Duration, Instant};

    use rustix::event::{PollFd, PollFlags};

    use super::poll_until;

    #[test]
    fn a_wait_ends_at_its_deadline_when_nothing_is_ready() {
        let (reader, _writer) = io::pipe().unwrap();
        let until = Instant::now() + Duration::from_millis(30);

        let ready = poll_until(&mut [PollFd::new(&reader, PollFlags::IN)], Some(until)).unwrap();

        assert!(!ready);
        assert!(Instant::now() >= until);
    }

    #[test]
    fn a_wait_ends_at_once_when_a_descriptor_is_ready() {
        let (reader, mut writer) = io::pipe().unwrap();
        writer.write_all(b"x").unwrap();
        let started = Instant::now();

        let ready = poll_until(&mut [PollFd::new(&reader, PollFlags::IN)], None).unwrap();

        assert!(ready);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
