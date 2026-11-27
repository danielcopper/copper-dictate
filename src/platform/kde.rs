use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use thiserror::Error;

use crate::config::InsertionConfig;
use crate::log;
use crate::platform;
use crate::platform::contents::{self, Previous, RestoreClock, Snapshot};
use crate::platform::data_control::{Clipboard, ClipboardError, Incoming, SELECTIONS, Selection};
use crate::platform::keyboard::{Keyboard, KeyboardError};
use crate::platform::remote_desktop::{self, PortalError, Started};
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
/// permission.
pub(crate) fn start(config: &InsertionConfig, token_file: Option<PathBuf>) -> io::Result<Paste> {
    let restore_timeout = config.restore_timeout;
    let (woken, wake) = io::pipe()?;
    let (texts, received) = mpsc::channel();
    thread::Builder::new()
        .name("insertion".to_owned())
        .spawn(move || {
            let worker = Worker::new(Kde { woken }, restore_timeout, token_file);
            run(worker, &received);
        })?;
    Ok(Paste {
        texts,
        wake: Mutex::new(wake),
    })
}

/// Opens the clipboard and the remote control session at the start, where the portal asks
/// or notifies; a final text that ends before both are open is not inserted, as the focus
/// may have moved on by then.
fn run(mut worker: Worker<Kde>, texts: &Receiver<String>) {
    if let Err(error) = worker.open() {
        platform::cannot_insert(error);
        return;
    }
    let missed = texts.try_iter().count();
    if missed > 0 {
        log::eprint(format_args!(
            "final texts not inserted because they ended before the insertion was ready: {missed}"
        ));
    }
    loop {
        match worker.idle() {
            Ok(()) => {}
            Err(error @ PasteError::Wait(_)) => {
                platform::cannot_insert(error);
                return;
            }
            Err(error) => log::eprint(format_args!(
                "the insertion at the cursor stopped: {error}; the next dictation starts it again"
            )),
        }
        for text in texts.try_iter() {
            if let Err(error) = worker.insert(&text) {
                log::eprint(format_args!(
                    "cannot insert this final text at the cursor: {error}; it reaches only \
                     toggle's output, and the next dictation tries again"
                ));
            }
        }
    }
}

trait Selections {
    fn snapshot(&mut self, selection: Selection) -> Result<Previous, ClipboardError>;
    fn offer(
        &mut self,
        selection: Selection,
        contents: Arc<Snapshot>,
    ) -> Result<(), ClipboardError>;
    fn clear(&mut self, selection: Selection) -> Result<(), ClipboardError>;
    /// Waits until the compositor has handled every request sent so far.
    fn sync(&mut self) -> Result<(), ClipboardError>;
    fn offers(&self, selection: Selection, contents: &Arc<Snapshot>) -> bool;
    /// The reads of offered contents since the last call, by type and time.
    fn take_reads(&mut self) -> Vec<(String, Instant)>;
}

trait PasteKey {
    fn press(&mut self) -> Result<(), KeyboardError>;
}

trait Desktop {
    type Clipboard: Selections;
    type Keyboard: PasteKey;

    fn connect_clipboard(&mut self) -> Result<Self::Clipboard, ClipboardError>;
    /// Opens a remote control session, and returns the token that restores it next time.
    fn open_keyboard(
        &mut self,
        token: Option<&str>,
    ) -> Result<(Self::Keyboard, Option<String>), PasteError>;
    /// Waits for the compositor, the input emulation or a new final text, at most until the
    /// deadline, and handles what arrived.
    fn wait(
        &mut self,
        clipboard: Option<&mut Self::Clipboard>,
        keyboard: Option<&mut Self::Keyboard>,
        until: Option<Instant>,
    ) -> Result<(), PasteError>;
}

/// Keeps the clipboard connection and the remote control session between insertions, and
/// opens again whichever failed when the next final text comes.
struct Worker<D: Desktop> {
    desktop: D,
    restore_timeout: Duration,
    token_file: Option<PathBuf>,
    clipboard: Option<D::Clipboard>,
    keyboard: Option<D::Keyboard>,
}

impl<D: Desktop> Worker<D> {
    fn new(desktop: D, restore_timeout: Duration, token_file: Option<PathBuf>) -> Self {
        Self {
            desktop,
            restore_timeout,
            token_file,
            clipboard: None,
            keyboard: None,
        }
    }

    fn open(&mut self) -> Result<(&mut D, &mut D::Clipboard, &mut D::Keyboard), PasteError> {
        let clipboard = match self.clipboard.take() {
            Some(clipboard) => clipboard,
            None => self.desktop.connect_clipboard()?,
        };
        let clipboard = self.clipboard.insert(clipboard);
        let keyboard = if let Some(keyboard) = self.keyboard.take() {
            keyboard
        } else {
            let token = self
                .token_file
                .as_deref()
                .and_then(remote_desktop::read_token);
            let (keyboard, new_token) = self.desktop.open_keyboard(token.as_deref())?;
            keep_token(self.token_file.as_deref(), new_token);
            keyboard
        };
        let keyboard = self.keyboard.insert(keyboard);
        Ok((&mut self.desktop, clipboard, keyboard))
    }

    fn insert(&mut self, text: &str) -> Result<(), PasteError> {
        let Some(text) = contents::pasteable(text) else {
            return Ok(());
        };
        let timeout = self.restore_timeout;
        let (desktop, clipboard, keyboard) = self.open()?;
        let errors = paste(desktop, clipboard, keyboard, &text, timeout);
        for error in &errors {
            self.forget_what_failed(error);
        }
        errors.into_iter().next().map_or(Ok(()), Err)
    }

    fn idle(&mut self) -> Result<(), PasteError> {
        let waited = self
            .desktop
            .wait(self.clipboard.as_mut(), self.keyboard.as_mut(), None);
        if let Err(error) = &waited {
            self.forget_what_failed(error);
        }
        waited
    }

    fn forget_what_failed(&mut self, error: &PasteError) {
        match error {
            PasteError::Clipboard(_) => self.clipboard = None,
            PasteError::Keyboard(error) if error.ends_the_session() => self.keyboard = None,
            PasteError::Keyboard(_) | PasteError::Portal(_) | PasteError::Wait(_) => {}
        }
    }
}

fn keep_token(path: Option<&Path>, token: Option<String>) {
    if let (Some(path), Some(token)) = (path, token)
        && let Err(error) = remote_desktop::write_token(path, &token)
    {
        log::eprint(format_args!(
            "cannot keep the remote control permission in {}: {error}; the next start asks again",
            path.display()
        ));
    }
}

/// Pastes the text and puts back what each selection held, wherever the final text still
/// stands; the errors come in the order they happened.
fn paste<D: Desktop>(
    desktop: &mut D,
    clipboard: &mut D::Clipboard,
    keyboard: &mut D::Keyboard,
    text: &str,
    timeout: Duration,
) -> Vec<PasteError> {
    let previous = match snapshots(clipboard) {
        Ok(previous) => previous,
        Err(error) => return vec![error.into()],
    };
    for (selection, previous) in SELECTIONS.iter().zip(&previous) {
        if let Previous::Kept { lost, .. } = previous {
            for (mime, why) in lost {
                log::eprint(format_args!(
                    "the {selection}'s {mime} does not come back after the insertion: {why}"
                ));
            }
        }
    }
    let offered = Arc::new(contents::final_text(text));
    let mut errors: Vec<PasteError> =
        offer_and_press(desktop, clipboard, keyboard, &offered, timeout)
            .err()
            .into_iter()
            .collect();
    for (selection, previous) in SELECTIONS.into_iter().zip(previous) {
        if clipboard.offers(selection, &offered)
            && let Err(error) = restore(clipboard, selection, previous)
        {
            errors.push(error.into());
        }
    }
    errors
}

fn snapshots(clipboard: &mut impl Selections) -> Result<[Previous; 2], ClipboardError> {
    Ok([
        clipboard.snapshot(Selection::Clipboard)?,
        clipboard.snapshot(Selection::Primary)?,
    ])
}

fn offer_and_press<D: Desktop>(
    desktop: &mut D,
    clipboard: &mut D::Clipboard,
    keyboard: &mut D::Keyboard,
    offered: &Arc<Snapshot>,
    timeout: Duration,
) -> Result<(), PasteError> {
    for selection in SELECTIONS {
        clipboard.offer(selection, Arc::clone(offered))?;
    }
    clipboard.sync()?;
    clipboard.take_reads();
    keyboard.press()?;
    let mut clock = RestoreClock::started(Instant::now(), timeout);
    loop {
        for (mime, at) in clipboard.take_reads() {
            clock.read(&mime, at);
        }
        let restore_at = clock.restore_at();
        if Instant::now() >= restore_at {
            return Ok(());
        }
        desktop.wait(
            Some(&mut *clipboard),
            Some(&mut *keyboard),
            Some(restore_at),
        )?;
    }
}

fn restore(
    clipboard: &mut impl Selections,
    selection: Selection,
    previous: Previous,
) -> Result<(), ClipboardError> {
    match previous {
        Previous::Kept { snapshot, .. } if !snapshot.is_empty() => {
            clipboard.offer(selection, Arc::new(snapshot))
        }
        Previous::Empty | Previous::Secret | Previous::Kept { .. } => clipboard.clear(selection),
    }
}

impl Selections for Clipboard {
    fn snapshot(&mut self, selection: Selection) -> Result<Previous, ClipboardError> {
        Clipboard::snapshot(self, selection)
    }

    fn offer(
        &mut self,
        selection: Selection,
        contents: Arc<Snapshot>,
    ) -> Result<(), ClipboardError> {
        Clipboard::offer(self, selection, contents)
    }

    fn clear(&mut self, selection: Selection) -> Result<(), ClipboardError> {
        Clipboard::clear(self, selection)
    }

    fn sync(&mut self) -> Result<(), ClipboardError> {
        Clipboard::sync(self)
    }

    fn offers(&self, selection: Selection, contents: &Arc<Snapshot>) -> bool {
        Clipboard::offers(self, selection, contents)
    }

    fn take_reads(&mut self) -> Vec<(String, Instant)> {
        Clipboard::take_reads(self)
    }
}

impl PasteKey for Keyboard {
    fn press(&mut self) -> Result<(), KeyboardError> {
        self.paste()
    }
}

struct Kde {
    woken: PipeReader,
}

impl Desktop for Kde {
    type Clipboard = Clipboard;
    type Keyboard = Keyboard;

    fn connect_clipboard(&mut self) -> Result<Clipboard, ClipboardError> {
        Clipboard::connect()
    }

    fn open_keyboard(
        &mut self,
        token: Option<&str>,
    ) -> Result<(Keyboard, Option<String>), PasteError> {
        let Started { session, new_token } = remote_desktop::start(token)?;
        Ok((Keyboard::connect(session)?, new_token))
    }

    fn wait(
        &mut self,
        mut clipboard: Option<&mut Clipboard>,
        keyboard: Option<&mut Keyboard>,
        until: Option<Instant>,
    ) -> Result<(), PasteError> {
        let incoming = match clipboard.as_deref_mut() {
            Some(clipboard) => match clipboard.prepare_read()? {
                Some(incoming) => Some(incoming),
                None => return Ok(clipboard.dispatch()?),
            },
            None => None,
        };
        let (woken, compositor_ready, emulation_ready) = {
            let compositor = incoming.as_ref().map(Incoming::fd);
            let emulation = keyboard.as_deref().map(Keyboard::fd);
            let mut fds = vec![PollFd::new(&self.woken, PollFlags::IN)];
            fds.extend(
                compositor
                    .iter()
                    .chain(&emulation)
                    .map(|fd| PollFd::new(fd, PollFlags::IN)),
            );
            platform::poll_until(&mut fds, until)?;
            let mut ready = fds.iter().map(|fd| !fd.revents().is_empty());
            (
                ready.next().unwrap_or(false),
                compositor.is_some() && ready.next().unwrap_or(false),
                emulation.is_some() && ready.next().unwrap_or(false),
            )
        };
        if woken {
            let _ = (&self.woken).read(&mut [0; 64]);
        }
        if let Some(incoming) = incoming
            && compositor_ready
        {
            incoming.read()?;
        }
        if let Some(clipboard) = clipboard {
            clipboard.dispatch()?;
        }
        if emulation_ready && let Some(keyboard) = keyboard {
            keyboard.dispatch()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{Desktop, PasteError, PasteKey, Selections, Worker};
    use crate::platform::contents::{Lost, Previous, Snapshot};
    use crate::platform::data_control::{ClipboardError, Selection};
    use crate::platform::keyboard::KeyboardError;
    use crate::platform::remote_desktop::{self, PortalError};
    use crate::test_support::TempDir;

    #[derive(Debug, Clone, Copy)]
    enum Failure {
        Clipboard,
        SessionEnded,
        Paused,
        Denied,
    }

    impl Failure {
        fn keyboard(self) -> KeyboardError {
            match self {
                Self::Paused => KeyboardError::Paused,
                _ => KeyboardError::Disconnected(None),
            }
        }

        fn paste(self) -> PasteError {
            match self {
                Self::Clipboard => ClipboardError::Finished.into(),
                Self::Denied => PortalError::Denied.into(),
                Self::SessionEnded | Self::Paused => self.keyboard().into(),
            }
        }
    }

    struct Script {
        happened: Vec<String>,
        previous: [Previous; 2],
        failing: Vec<(String, Failure)>,
        taken_during_the_wait: Option<Selection>,
        tokens: Vec<&'static str>,
    }

    impl Script {
        fn step(&mut self, step: String) -> Result<(), Failure> {
            let failure = self
                .failing
                .iter()
                .position(|(failing, _)| *failing == step)
                .map(|at| self.failing.remove(at).1);
            self.happened.push(step);
            failure.map_or(Ok(()), Err)
        }
    }

    type Shared = Rc<RefCell<Script>>;

    fn script(previous: [Previous; 2]) -> Shared {
        Rc::new(RefCell::new(Script {
            happened: Vec::new(),
            previous,
            failing: Vec::new(),
            taken_during_the_wait: None,
            tokens: Vec::new(),
        }))
    }

    fn failing(script: &Shared, step: &str, failure: Failure) {
        script.borrow_mut().failing.push((step.to_owned(), failure));
    }

    fn happened(script: &Shared) -> Vec<String> {
        script.borrow().happened.clone()
    }

    fn text(text: &str) -> Previous {
        Previous::Kept {
            snapshot: Snapshot {
                contents: vec![("text/plain".to_owned(), text.as_bytes().to_vec())],
            },
            lost: Vec::new(),
        }
    }

    fn worker(script: &Shared, token_file: Option<PathBuf>) -> Worker<FakeDesktop> {
        Worker::new(
            FakeDesktop(Rc::clone(script)),
            Duration::from_millis(5),
            token_file,
        )
    }

    struct FakeDesktop(Shared);

    struct FakeClipboard {
        script: Shared,
        owned: [Option<Arc<Snapshot>>; 2],
    }

    struct FakeKeyboard(Shared);

    impl Desktop for FakeDesktop {
        type Clipboard = FakeClipboard;
        type Keyboard = FakeKeyboard;

        fn connect_clipboard(&mut self) -> Result<FakeClipboard, ClipboardError> {
            self.0
                .borrow_mut()
                .step("connect the clipboard".to_owned())
                .map_err(|_| ClipboardError::NoSeat)?;
            Ok(FakeClipboard {
                script: Rc::clone(&self.0),
                owned: [None, None],
            })
        }

        fn open_keyboard(
            &mut self,
            token: Option<&str>,
        ) -> Result<(FakeKeyboard, Option<String>), PasteError> {
            let mut script = self.0.borrow_mut();
            script
                .step(token.map_or_else(
                    || "open a session without a token".to_owned(),
                    |token| format!("open a session with the token {token}"),
                ))
                .map_err(Failure::paste)?;
            let new_token = (!script.tokens.is_empty()).then(|| script.tokens.remove(0).to_owned());
            Ok((FakeKeyboard(Rc::clone(&self.0)), new_token))
        }

        fn wait(
            &mut self,
            clipboard: Option<&mut FakeClipboard>,
            _keyboard: Option<&mut FakeKeyboard>,
            until: Option<Instant>,
        ) -> Result<(), PasteError> {
            let Some(until) = until else {
                return self
                    .0
                    .borrow_mut()
                    .step("wait between dictations".to_owned())
                    .map_err(Failure::paste);
            };
            let mut script = self.0.borrow_mut();
            if script.happened.last().is_none_or(|last| last != "wait") {
                script.step("wait".to_owned()).map_err(Failure::paste)?;
            }
            if let (Some(selection), Some(clipboard)) =
                (script.taken_during_the_wait.take(), clipboard)
            {
                clipboard.owned[selection as usize] = None;
                script
                    .happened
                    .push(format!("someone else sets the {selection}"));
            }
            drop(script);
            thread::sleep(until.saturating_duration_since(Instant::now()));
            Ok(())
        }
    }

    impl FakeClipboard {
        fn step(&self, step: String) -> Result<(), ClipboardError> {
            self.script
                .borrow_mut()
                .step(step)
                .map_err(|_| ClipboardError::Finished)
        }
    }

    impl Selections for FakeClipboard {
        fn snapshot(&mut self, selection: Selection) -> Result<Previous, ClipboardError> {
            self.step(format!("take the {selection}"))?;
            Ok(self.owned[selection as usize].as_ref().map_or_else(
                || self.script.borrow().previous[selection as usize].clone(),
                |owned| Previous::Kept {
                    snapshot: owned.as_ref().clone(),
                    lost: Vec::new(),
                },
            ))
        }

        fn offer(
            &mut self,
            selection: Selection,
            contents: Arc<Snapshot>,
        ) -> Result<(), ClipboardError> {
            let text = String::from_utf8_lossy(contents.data("text/plain").unwrap_or_default());
            self.step(format!("offer the {selection}: {text}"))?;
            self.owned[selection as usize] = Some(contents);
            Ok(())
        }

        fn clear(&mut self, selection: Selection) -> Result<(), ClipboardError> {
            self.step(format!("clear the {selection}"))?;
            self.owned[selection as usize] = None;
            Ok(())
        }

        fn sync(&mut self) -> Result<(), ClipboardError> {
            self.step("sync".to_owned())
        }

        fn offers(&self, selection: Selection, contents: &Arc<Snapshot>) -> bool {
            self.owned[selection as usize]
                .as_ref()
                .is_some_and(|owned| Arc::ptr_eq(owned, contents))
        }

        fn take_reads(&mut self) -> Vec<(String, Instant)> {
            Vec::new()
        }
    }

    impl PasteKey for FakeKeyboard {
        fn press(&mut self) -> Result<(), KeyboardError> {
            self.0
                .borrow_mut()
                .step("press Shift+Insert".to_owned())
                .map_err(Failure::keyboard)
        }
    }

    const OPENED: [&str; 2] = ["connect the clipboard", "open a session without a token"];
    const TAKEN: [&str; 2] = ["take the clipboard", "take the primary selection"];

    fn steps(groups: &[&[&str]]) -> Vec<String> {
        groups
            .iter()
            .flat_map(|group| group.iter().map(|step| (*step).to_owned()))
            .collect()
    }

    #[test]
    fn the_final_text_is_offered_on_both_selections_pasted_and_the_previous_contents_come_back() {
        let script = script([text("copied"), text("selected")]);

        worker(&script, None).insert("Grüße").unwrap();

        assert_eq!(
            happened(&script),
            steps(&[
                &OPENED,
                &TAKEN,
                &[
                    "offer the clipboard: Grüße",
                    "offer the primary selection: Grüße",
                    "sync",
                    "press Shift+Insert",
                    "wait",
                    "offer the clipboard: copied",
                    "offer the primary selection: selected",
                ],
            ])
        );
    }

    #[test]
    fn the_final_text_is_offered_without_line_breaks_and_escape_sequences() {
        let script = script([Previous::Empty, Previous::Empty]);

        worker(&script, None)
            .insert("eins\nzwei\u{1b}[201~\r\n")
            .unwrap();

        assert!(
            happened(&script).contains(&"offer the clipboard: eins zwei[201~ ".to_owned()),
            "{:?}",
            happened(&script)
        );
    }

    #[test]
    fn a_final_text_of_whitespace_alone_is_not_pasted() {
        let script = script([text("copied"), text("selected")]);

        worker(&script, None).insert(" \n\t ").unwrap();

        assert_eq!(happened(&script), Vec::<String>::new());
    }

    #[test]
    fn when_the_key_fails_the_previous_contents_still_come_back() {
        let script = script([text("copied"), text("selected")]);
        failing(&script, "press Shift+Insert", Failure::Paused);

        let inserted = worker(&script, None).insert("lost");

        assert!(
            matches!(inserted, Err(PasteError::Keyboard(KeyboardError::Paused))),
            "{inserted:?}"
        );
        assert_eq!(
            happened(&script)[7..],
            [
                "press Shift+Insert",
                "offer the clipboard: copied",
                "offer the primary selection: selected",
            ]
        );
    }

    #[test]
    fn when_the_wait_fails_the_previous_contents_still_come_back() {
        let script = script([text("copied"), text("selected")]);
        failing(&script, "wait", Failure::SessionEnded);

        let inserted = worker(&script, None).insert("lost");

        assert!(
            matches!(
                inserted,
                Err(PasteError::Keyboard(KeyboardError::Disconnected(None)))
            ),
            "{inserted:?}"
        );
        assert_eq!(
            happened(&script)[8..],
            [
                "wait",
                "offer the clipboard: copied",
                "offer the primary selection: selected",
            ]
        );
    }

    #[test]
    fn when_the_primary_selection_cannot_be_set_the_clipboard_still_gets_its_contents_back() {
        let script = script([text("copied"), text("selected")]);
        failing(
            &script,
            "offer the primary selection: lost",
            Failure::Clipboard,
        );

        let inserted = worker(&script, None).insert("lost");

        assert!(
            matches!(inserted, Err(PasteError::Clipboard(_))),
            "{inserted:?}"
        );
        assert_eq!(
            happened(&script)[4..],
            [
                "offer the clipboard: lost",
                "offer the primary selection: lost",
                "offer the clipboard: copied",
            ]
        );
    }

    #[test]
    fn a_selection_someone_else_sets_during_the_wait_is_left_alone() {
        let script = script([text("copied"), text("selected")]);
        script.borrow_mut().taken_during_the_wait = Some(Selection::Clipboard);

        worker(&script, None).insert("pasted").unwrap();

        assert_eq!(
            happened(&script)[8..],
            [
                "wait",
                "someone else sets the clipboard",
                "offer the primary selection: selected",
            ]
        );
    }

    #[test]
    fn contents_marked_as_a_password_are_not_restored_and_leave_the_selection_empty() {
        let script = script([Previous::Secret, Previous::Empty]);

        worker(&script, None).insert("pasted").unwrap();

        assert_eq!(
            happened(&script)[8..],
            ["wait", "clear the clipboard", "clear the primary selection"]
        );
    }

    #[test]
    fn contents_none_of_whose_types_could_be_kept_leave_the_selection_empty() {
        let script = script([
            Previous::Kept {
                snapshot: Snapshot {
                    contents: Vec::new(),
                },
                lost: vec![("image/png".to_owned(), Lost::TooBig)],
            },
            text("selected"),
        ]);

        worker(&script, None).insert("pasted").unwrap();

        assert_eq!(
            happened(&script)[8..],
            [
                "wait",
                "clear the clipboard",
                "offer the primary selection: selected"
            ]
        );
    }

    #[test]
    fn a_paused_keyboard_fails_only_that_insertion_and_the_next_one_keeps_the_session() {
        let script = script([text("copied"), text("selected")]);
        failing(&script, "press Shift+Insert", Failure::Paused);
        let mut worker = worker(&script, None);

        assert!(worker.insert("first").is_err());
        worker.insert("second").unwrap();

        let happened = happened(&script);
        assert_eq!(
            happened
                .iter()
                .filter(|step| step.starts_with("connect") || step.starts_with("open"))
                .collect::<Vec<_>>(),
            OPENED
        );
        assert!(
            happened.contains(&"offer the clipboard: second".to_owned()),
            "{happened:?}"
        );
        assert_eq!(
            happened.last().unwrap(),
            "offer the primary selection: selected"
        );
    }

    #[test]
    fn after_the_session_ended_the_next_dictation_opens_one_with_the_saved_token() {
        let dir = TempDir::new();
        let token_file = dir.path().join("state/copper-dictate/remote-desktop-token");
        let script = script([text("copied"), text("selected")]);
        script.borrow_mut().tokens = vec!["first", "second"];
        failing(&script, "press Shift+Insert", Failure::SessionEnded);
        let mut worker = worker(&script, Some(token_file.clone()));

        assert!(worker.insert("lost").is_err());
        worker.insert("pasted").unwrap();

        let happened = happened(&script);
        assert_eq!(
            happened
                .iter()
                .filter(|step| step.starts_with("connect") || step.starts_with("open"))
                .collect::<Vec<_>>(),
            [
                "connect the clipboard",
                "open a session without a token",
                "open a session with the token first",
            ]
        );
        assert!(
            happened.contains(&"offer the clipboard: pasted".to_owned()),
            "{happened:?}"
        );
        assert_eq!(
            remote_desktop::read_token(&token_file).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn a_session_that_ends_between_dictations_is_opened_again_by_the_next_one() {
        let script = script([Previous::Empty, Previous::Empty]);
        failing(&script, "wait between dictations", Failure::SessionEnded);
        let mut worker = worker(&script, None);
        worker.open().unwrap();

        assert!(worker.idle().is_err());
        worker.insert("pasted").unwrap();

        assert_eq!(
            happened(&script)[..5],
            [
                "connect the clipboard",
                "open a session without a token",
                "wait between dictations",
                "open a session without a token",
                "take the clipboard",
            ]
        );
    }

    #[test]
    fn a_failed_clipboard_is_connected_again_by_the_next_dictation() {
        let script = script([Previous::Empty, Previous::Empty]);
        failing(&script, "sync", Failure::Clipboard);
        let mut worker = worker(&script, None);

        assert!(worker.insert("lost").is_err());
        worker.insert("pasted").unwrap();

        let happened = happened(&script);
        assert_eq!(
            happened
                .iter()
                .filter(|step| step.starts_with("connect") || step.starts_with("open"))
                .collect::<Vec<_>>(),
            [
                "connect the clipboard",
                "open a session without a token",
                "connect the clipboard",
            ]
        );
        assert!(
            happened.contains(&"offer the clipboard: pasted".to_owned()),
            "{happened:?}"
        );
    }

    #[test]
    fn a_session_that_cannot_be_opened_fails_only_that_dictation() {
        let script = script([Previous::Empty, Previous::Empty]);
        failing(&script, "open a session without a token", Failure::Denied);
        let mut worker = worker(&script, None);

        let first = worker.insert("lost");
        let after_the_first = happened(&script);
        worker.insert("pasted").unwrap();

        assert!(
            matches!(first, Err(PasteError::Portal(PortalError::Denied))),
            "{first:?}"
        );
        assert_eq!(after_the_first, OPENED);
        assert!(
            happened(&script).contains(&"offer the clipboard: pasted".to_owned()),
            "{:?}",
            happened(&script)
        );
    }
}
