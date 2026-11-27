use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Instant;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use thiserror::Error;

use crate::config::InsertionConfig;
use crate::log;
use crate::platform;
use crate::platform::clipboard::{self, RestoreClock};
use crate::platform::data_control::{Clipboard, ClipboardError, Selection};
use crate::platform::keyboard::{Keyboard, KeyboardError};
use crate::platform::remote_desktop::{self, PortalError};
use crate::session::Insertion;

#[derive(Debug, Error)]
enum PasteError {
    #[error(transparent)]
    Clipboard(#[from] ClipboardError),
    #[error(transparent)]
    Portal(#[from] PortalError),
    #[error(transparent)]
    Keyboard(#[from] KeyboardError),
    #[error("waiting for the desktop failed: {0}")]
    Wait(#[from] io::Error),
}

/// Inserts by pasting: the final text goes to the clipboard and the primary selection, the
/// paste key is sent, and the previous contents come back.
pub(crate) struct Paste {
    texts: Sender<String>,
    wake: Mutex<PipeWriter>,
}

impl Insertion for Paste {
    fn insert(&self, text: String) {
        if self.texts.send(text).is_ok() {
            let _ = self
                .wake
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .write(&[1]);
        }
    }
}

/// Sets the insertion up on a thread of its own, since the portal may wait for the user's
/// permission; a dictation that ends before it is ready is not inserted.
pub(crate) fn start(config: InsertionConfig, token_file: Option<PathBuf>) -> io::Result<Paste> {
    let (woken, wake) = io::pipe()?;
    let (texts, received) = mpsc::channel();
    thread::Builder::new()
        .name("insertion".to_owned())
        .spawn(move || {
            if let Err(error) = run(&config, token_file.as_deref(), &woken, &received) {
                platform::cannot_insert(error);
            }
        })?;
    Ok(Paste {
        texts,
        wake: Mutex::new(wake),
    })
}

fn run(
    config: &InsertionConfig,
    token_file: Option<&Path>,
    woken: &PipeReader,
    texts: &Receiver<String>,
) -> Result<(), PasteError> {
    let clipboard = Clipboard::connect()?;
    let token = token_file.and_then(remote_desktop::read_token);
    let (_session, eis, new_token) = remote_desktop::start(token.as_deref())?;
    if let (Some(path), Some(token)) = (token_file, new_token)
        && let Err(error) = remote_desktop::write_token(path, &token)
    {
        log::eprint(format_args!(
            "cannot keep the remote control permission in {}: {error}; the next start asks again",
            path.display()
        ));
    }
    let mut desktop = Desktop {
        clipboard,
        keyboard: Keyboard::connect(eis)?,
    };
    while texts.try_recv().is_ok() {}
    loop {
        if desktop.wait(Some(woken), None)? {
            let _ = (&*woken).read(&mut [0; 64]);
        }
        while let Ok(text) = texts.try_recv() {
            desktop.paste(&text, config)?;
        }
    }
}

struct Desktop {
    clipboard: Clipboard,
    keyboard: Keyboard,
}

impl Desktop {
    fn paste(&mut self, text: &str, config: &InsertionConfig) -> Result<(), PasteError> {
        let previous = [
            self.clipboard.snapshot(Selection::Clipboard)?,
            self.clipboard.snapshot(Selection::Primary)?,
        ];
        let offered = Arc::new(clipboard::final_text(text));
        self.clipboard
            .set(Selection::Clipboard, Arc::clone(&offered))?;
        self.clipboard.set(Selection::Primary, offered)?;
        self.clipboard.sync()?;
        self.clipboard.take_reads();
        let pasted = self.keyboard.paste();
        if pasted.is_ok() {
            self.wait_for_the_read(config)?;
        }
        let [kept_clipboard, kept_primary] = previous;
        self.clipboard
            .set(Selection::Clipboard, Arc::new(kept_clipboard))?;
        self.clipboard
            .set(Selection::Primary, Arc::new(kept_primary))?;
        Ok(pasted?)
    }

    fn wait_for_the_read(&mut self, config: &InsertionConfig) -> Result<(), PasteError> {
        let mut clock = RestoreClock::started(Instant::now(), config.restore_timeout);
        loop {
            for (mime, at) in self.clipboard.take_reads() {
                clock.read(&mime, at);
            }
            let restore_at = clock.restore_at();
            if Instant::now() >= restore_at {
                return Ok(());
            }
            self.wait(None, Some(restore_at))?;
        }
    }

    /// Waits for the compositor, the input emulation or the wake pipe, at most until the
    /// deadline; true when the wake pipe is readable.
    fn wait(
        &mut self,
        wake: Option<&PipeReader>,
        until: Option<Instant>,
    ) -> Result<bool, PasteError> {
        let Some(guard) = self.clipboard.prepare_read()? else {
            self.clipboard.dispatch()?;
            return Ok(false);
        };
        let timeout = until
            .map(|until| Timespec::try_from(until.saturating_duration_since(Instant::now())))
            .transpose()
            .map_err(io::Error::other)?;
        let wanted = PollFlags::IN;
        let compositor = guard.connection_fd();
        let emulation = self.keyboard.fd();
        let mut fds = vec![
            PollFd::new(&compositor, wanted),
            PollFd::new(&emulation, wanted),
        ];
        if let Some(wake) = wake {
            fds.push(PollFd::new(wake, wanted));
        }
        match poll(&mut fds, timeout.as_ref()) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(io::Error::from(error).into()),
        }
        let ready: Vec<bool> = fds.iter().map(|fd| !fd.revents().is_empty()).collect();
        drop(fds);
        if ready[0] {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(error))
                    if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(ClipboardError::from(error).into()),
            }
        }
        self.clipboard.dispatch()?;
        if ready[1] {
            self.keyboard.dispatch()?;
        }
        Ok(ready.get(2).copied().unwrap_or(false))
    }
}
