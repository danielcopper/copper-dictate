use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
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
use crate::platform::remote_desktop::{self, PortalError, RemoteDesktop, Started};
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

impl PasteError {
    /// Whether the session was ended on purpose or the permission denied, so that the next
    /// session asks the user instead of restoring the permission.
    fn asks_again(&self) -> bool {
        matches!(
            self,
            Self::Keyboard(KeyboardError::EndedOnPurpose(_)) | Self::Portal(PortalError::Denied)
        )
    }
}

#[derive(Debug, Error)]
enum Trouble {
    #[error(
        "cannot insert this final text at the cursor: {0}; it reaches only toggle's output, and \
         the next dictation tries again"
    )]
    NotPasted(PasteError),
    #[error(
        "the paste key was sent, but then {0}; if the final text is not at the cursor, toggle's \
         output has it"
    )]
    AfterTheKey(PasteError),
    #[error("the {0}'s previous contents do not come back after the insertion: {1}")]
    NotRestored(Selection, ClipboardError),
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
    let desktop = Kde {
        woken,
        wake: wake.try_clone()?,
    };
    let (texts, received) = mpsc::channel();
    thread::Builder::new()
        .name("insertion".to_owned())
        .spawn(move || {
            let worker = Worker::new(desktop, restore_timeout, token_file);
            run(worker, &received);
        })?;
    Ok(Paste {
        texts,
        wake: Mutex::new(wake),
    })
}

/// Opens the clipboard and the remote control session at the start, where the portal asks
/// or notifies; a final text that ends meanwhile is not inserted, as the focus may have moved
/// on by then.
fn run<D: Desktop>(mut worker: Worker<D>, texts: &Receiver<String>) {
    if let Err(error) = worker.open() {
        if !error.asks_again() {
            platform::cannot_insert(error);
            return;
        }
        worker.forget_what_failed(&error);
        log::eprint(format_args!(
            "the insertion at the cursor did not start: {error}; the next dictation asks again"
        ));
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
            for trouble in worker.insert(&text) {
                log::eprint(format_args!("{trouble}"));
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
    fn sync(&mut self) -> Result<(), ClipboardError>;
    fn offers(&self, selection: Selection, contents: &Arc<Snapshot>) -> bool;
    fn take_reads(&mut self) -> Vec<(String, Instant)>;
}

trait PasteKey {
    fn press(&mut self) -> Result<(), KeyboardError>;
}

type Answer<S> = Receiver<Result<Started<S>, PortalError>>;

trait Desktop {
    type Clipboard: Selections;
    type Session;
    type Keyboard: PasteKey;

    fn connect_clipboard(&mut self) -> Result<Self::Clipboard, ClipboardError>;
    /// Asks the portal for a remote control session; the answer comes on the receiver and
    /// ends a `wait`, maybe only once the user gave it.
    fn start_session(&mut self, token: Option<&str>) -> io::Result<Answer<Self::Session>>;
    fn connect_keyboard(&mut self, session: Self::Session)
    -> Result<Self::Keyboard, KeyboardError>;
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
    token: SavedToken,
    clipboard: Option<D::Clipboard>,
    keyboard: Option<D::Keyboard>,
}

impl<D: Desktop> Worker<D> {
    fn new(desktop: D, restore_timeout: Duration, token_file: Option<PathBuf>) -> Self {
        Self {
            desktop,
            restore_timeout,
            token: SavedToken::read(token_file),
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
            let answer = self.desktop.start_session(self.token.get())?;
            let Started { session, new_token } =
                wait_for_the_session(&mut self.desktop, clipboard, &answer)?;
            self.token.renew(new_token);
            self.desktop.connect_keyboard(session)?
        };
        let keyboard = self.keyboard.insert(keyboard);
        Ok((&mut self.desktop, clipboard, keyboard))
    }

    fn insert(&mut self, text: &str) -> Vec<Trouble> {
        let Some(text) = contents::pasteable(text) else {
            return Vec::new();
        };
        let timeout = self.restore_timeout;
        let troubles = match self.open() {
            Ok((desktop, clipboard, keyboard)) => {
                paste(desktop, clipboard, keyboard, &text, timeout)
            }
            Err(error) => vec![Trouble::NotPasted(error)],
        };
        for trouble in &troubles {
            match trouble {
                Trouble::NotPasted(error) | Trouble::AfterTheKey(error) => {
                    self.forget_what_failed(error);
                }
                Trouble::NotRestored(..) => self.clipboard = None,
            }
        }
        troubles
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
        if error.asks_again() {
            self.token.forget();
        }
        match error {
            PasteError::Clipboard(_) => self.clipboard = None,
            PasteError::Keyboard(error) if error.ends_the_session() => self.keyboard = None,
            PasteError::Keyboard(_) | PasteError::Portal(_) | PasteError::Wait(_) => {}
        }
    }
}

/// The token that restores the remote control permission, kept in a file as well, so that
/// the daemon's next start restores it too; without one, the portal asks the user.
struct SavedToken {
    file: Option<PathBuf>,
    token: Option<String>,
}

impl SavedToken {
    fn read(file: Option<PathBuf>) -> Self {
        let token = file.as_deref().and_then(remote_desktop::read_token);
        Self { file, token }
    }

    fn get(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Keeps the token an opened session brought; a session opened without one that brought
    /// none leaves no token behind.
    fn renew(&mut self, new_token: Option<String>) {
        match new_token {
            Some(token) => {
                if let Some(file) = &self.file
                    && let Err(error) = remote_desktop::write_token(file, &token)
                {
                    log::eprint(format_args!(
                        "cannot keep the remote control permission in {}: {error}; the next \
                         start asks again",
                        file.display()
                    ));
                }
                self.token = Some(token);
            }
            None if self.token.is_none() => self.forget(),
            None => {}
        }
    }

    fn forget(&mut self) {
        self.token = None;
        if let Some(file) = &self.file
            && let Err(error) = remote_desktop::forget_token(file)
        {
            log::eprint(format_args!(
                "cannot delete the remote control permission in {}: {error}; the next start \
                 restores it without asking",
                file.display()
            ));
        }
    }
}

/// Pastes the text and puts back what each selection held, wherever the final text still
/// stands; the troubles come in the order they happened.
fn paste<D: Desktop>(
    desktop: &mut D,
    clipboard: &mut D::Clipboard,
    keyboard: &mut D::Keyboard,
    text: &str,
    timeout: Duration,
) -> Vec<Trouble> {
    let previous = match snapshots(clipboard) {
        Ok(previous) => previous,
        Err(error) => return vec![Trouble::NotPasted(error.into())],
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
    let mut troubles = match offer_and_press(clipboard, keyboard, &offered) {
        Ok(()) => wait_for_the_read(desktop, clipboard, keyboard, timeout)
            .into_iter()
            .map(Trouble::AfterTheKey)
            .collect(),
        Err(error) => vec![Trouble::NotPasted(error)],
    };
    for (selection, previous) in SELECTIONS.into_iter().zip(previous) {
        if clipboard.offers(selection, &offered)
            && let Err(error) = restore(clipboard, selection, previous)
        {
            troubles.push(Trouble::NotRestored(selection, error));
        }
    }
    troubles
}

fn snapshots(clipboard: &mut impl Selections) -> Result<[Previous; 2], ClipboardError> {
    Ok([
        clipboard.snapshot(Selection::Clipboard)?,
        clipboard.snapshot(Selection::Primary)?,
    ])
}

fn offer_and_press(
    clipboard: &mut impl Selections,
    keyboard: &mut impl PasteKey,
    offered: &Arc<Snapshot>,
) -> Result<(), PasteError> {
    for selection in SELECTIONS {
        clipboard.offer(selection, Arc::clone(offered))?;
    }
    clipboard.sync()?;
    clipboard.take_reads();
    keyboard.press()?;
    Ok(())
}

/// Serves the clipboard until the portal answers; once the clipboard fails, there is nothing
/// left to serve, and the answer is waited for alone.
fn wait_for_the_session<D: Desktop>(
    desktop: &mut D,
    clipboard: &mut D::Clipboard,
    answer: &Answer<D::Session>,
) -> Result<Started<D::Session>, PasteError> {
    loop {
        match answer.try_recv() {
            Ok(started) => return started.map_err(PasteError::from),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => break,
        }
        // A clipboard that fails here fails again at its next use, where that is said.
        if desktop.wait(Some(clipboard), None, None).is_err() {
            break;
        }
    }
    answer
        .recv()
        .map_err(|_| io::Error::other("the remote control session's start gave no answer"))?
        .map_err(PasteError::from)
}

/// Serves the final text until the restore is due; once the key is sent, a failed input
/// emulation does not cut this short, as the window may still be reading the text.
fn wait_for_the_read<D: Desktop>(
    desktop: &mut D,
    clipboard: &mut D::Clipboard,
    keyboard: &mut D::Keyboard,
    timeout: Duration,
) -> Vec<PasteError> {
    let mut errors = Vec::new();
    let mut keyboard = Some(keyboard);
    let mut clock = RestoreClock::started(Instant::now(), timeout);
    loop {
        for (mime, at) in clipboard.take_reads() {
            clock.read(&mime, at);
        }
        let restore_at = clock.restore_at();
        if Instant::now() >= restore_at {
            return errors;
        }
        if let Err(error) = desktop.wait(
            Some(&mut *clipboard),
            keyboard.as_deref_mut(),
            Some(restore_at),
        ) {
            let clipboard_failed = matches!(error, PasteError::Clipboard(_));
            errors.push(error);
            if clipboard_failed || keyboard.take().is_none() {
                return errors;
            }
        }
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
    wake: PipeWriter,
}

impl Desktop for Kde {
    type Clipboard = Clipboard;
    type Session = RemoteDesktop;
    type Keyboard = Keyboard;

    fn connect_clipboard(&mut self) -> Result<Clipboard, ClipboardError> {
        Clipboard::connect()
    }

    fn start_session(&mut self, token: Option<&str>) -> io::Result<Answer<RemoteDesktop>> {
        let (sender, answer) = mpsc::channel();
        let token = token.map(str::to_owned);
        let mut wake = self.wake.try_clone()?;
        thread::Builder::new()
            .name("remote-desktop".to_owned())
            .spawn(move || {
                let _ = sender.send(remote_desktop::start(token.as_deref()));
                let _ = wake.write(&[1]);
            })?;
        Ok(answer)
    }

    fn connect_keyboard(&mut self, session: RemoteDesktop) -> Result<Keyboard, KeyboardError> {
        Keyboard::connect(session)
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
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::mpsc::{self, Sender};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{Answer, Desktop, PasteError, PasteKey, Selections, Trouble, Worker, run};
    use crate::platform::contents::{Lost, Previous, Snapshot};
    use crate::platform::data_control::{ClipboardError, Selection};
    use crate::platform::keyboard::KeyboardError;
    use crate::platform::remote_desktop::{self, PortalError, Started};
    use crate::test_support::TempDir;

    #[derive(Debug, Clone, Copy)]
    enum Failure {
        Clipboard,
        SessionEnded,
        EndedOnPurpose,
        Paused,
        Denied,
        NoPortal,
        Poll,
    }

    impl Failure {
        fn keyboard(self) -> KeyboardError {
            match self {
                Self::Paused => KeyboardError::Paused,
                Self::EndedOnPurpose => KeyboardError::EndedOnPurpose(None),
                _ => KeyboardError::Disconnected(None),
            }
        }

        fn portal(self) -> PortalError {
            match self {
                Self::Denied => PortalError::Denied,
                Self::NoPortal => PortalError::Missing(zbus::Error::InterfaceNotFound),
                _ => PortalError::Failed("Start"),
            }
        }

        fn paste(self) -> PasteError {
            match self {
                Self::Clipboard => ClipboardError::Finished.into(),
                Self::Denied | Self::NoPortal => self.portal().into(),
                Self::Poll => io::Error::other("poll failed").into(),
                Self::SessionEnded | Self::EndedOnPurpose | Self::Paused => self.keyboard().into(),
            }
        }
    }

    /// When the fake portal answers a session's start.
    #[derive(Clone, Copy)]
    enum PortalAnswers {
        AtOnce,
        AtTheNextWait,
        /// From another thread, a while after the start, whatever the waits do.
        Late,
    }

    type Answered = Result<Started<()>, PortalError>;

    struct Script {
        happened: Vec<String>,
        previous: [Previous; 2],
        failing: Vec<(String, Failure)>,
        taken_during_the_wait: Option<Selection>,
        tokens: Vec<&'static str>,
        portal_answers: PortalAnswers,
        /// Set from a session's start until its keyboard connects.
        portal_waits: bool,
        pending_answer: Option<(Sender<Answered>, Answered)>,
        /// Under `run`: the final text each wait between dictations brings, until none is left.
        dictations: Option<(Sender<String>, Vec<&'static str>)>,
    }

    impl Script {
        fn failure(&mut self, step: &str) -> Option<Failure> {
            self.failing
                .iter()
                .position(|(failing, _)| failing == step)
                .map(|at| self.failing.remove(at).1)
        }

        fn step(&mut self, step: String) -> Result<(), Failure> {
            let failure = self.failure(&step);
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
            portal_answers: PortalAnswers::AtOnce,
            portal_waits: false,
            pending_answer: None,
            dictations: None,
        }))
    }

    fn failing(script: &Shared, step: &str, failure: Failure) {
        script.borrow_mut().failing.push((step.to_owned(), failure));
    }

    fn happened(script: &Shared) -> Vec<String> {
        script.borrow().happened.clone()
    }

    fn opened(script: &Shared) -> Vec<String> {
        happened(script)
            .into_iter()
            .filter(|step| step.starts_with("connect") || step.starts_with("open"))
            .collect()
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

    fn saved_token(dir: &TempDir, token: &str) -> PathBuf {
        let token_file = dir.path().join("state/copper-dictate/remote-desktop-token");
        remote_desktop::write_token(&token_file, token).unwrap();
        token_file
    }

    #[track_caller]
    fn inserted(worker: &mut Worker<FakeDesktop>, text: &str) {
        let troubles = worker.insert(text);
        assert!(troubles.is_empty(), "{troubles:?}");
    }

    /// Runs the insertion's loop over these dictations, with `early` queued before it starts.
    fn run_dictations(
        script: &Shared,
        token_file: Option<PathBuf>,
        early: &[&str],
        dictations: Vec<&'static str>,
    ) {
        let (texts, received) = mpsc::channel();
        for text in early {
            texts.send((*text).to_owned()).unwrap();
        }
        script.borrow_mut().dictations = Some((texts, dictations));
        run(worker(script, token_file), &received);
    }

    struct FakeDesktop(Shared);

    struct FakeClipboard {
        script: Shared,
        owned: [Option<Arc<Snapshot>>; 2],
    }

    struct FakeKeyboard(Shared);

    impl Desktop for FakeDesktop {
        type Clipboard = FakeClipboard;
        type Session = ();
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

        fn start_session(&mut self, token: Option<&str>) -> io::Result<Answer<()>> {
            let script = &mut *self.0.borrow_mut();
            let answered = script
                .step(token.map_or_else(
                    || "open a session without a token".to_owned(),
                    |token| format!("open a session with the token {token}"),
                ))
                .map_err(Failure::portal)
                .map(|()| Started {
                    session: (),
                    new_token: (!script.tokens.is_empty())
                        .then(|| script.tokens.remove(0).to_owned()),
                });
            let (sender, answer) = mpsc::channel();
            match script.portal_answers {
                PortalAnswers::AtOnce => sender.send(answered).unwrap(),
                PortalAnswers::AtTheNextWait => {
                    script.portal_waits = true;
                    script.pending_answer = Some((sender, answered));
                }
                PortalAnswers::Late => {
                    script.portal_waits = true;
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(50));
                        let _ = sender.send(answered);
                    });
                }
            }
            Ok(answer)
        }

        fn connect_keyboard(&mut self, (): ()) -> Result<FakeKeyboard, KeyboardError> {
            let mut script = self.0.borrow_mut();
            script.portal_waits = false;
            script
                .failure("connect the keyboard")
                .map_or(Ok(()), |failure| Err(failure.keyboard()))?;
            Ok(FakeKeyboard(Rc::clone(&self.0)))
        }

        fn wait(
            &mut self,
            clipboard: Option<&mut FakeClipboard>,
            keyboard: Option<&mut FakeKeyboard>,
            until: Option<Instant>,
        ) -> Result<(), PasteError> {
            let script = &mut *self.0.borrow_mut();
            if script.portal_waits {
                if let Some((sender, answered)) = script.pending_answer.take() {
                    sender.send(answered).unwrap();
                }
                return script
                    .step(
                        if clipboard.is_some() {
                            "wait for the portal"
                        } else {
                            "wait for the portal without the clipboard"
                        }
                        .to_owned(),
                    )
                    .map_err(Failure::paste);
            }
            let Some(until) = until else {
                if let Some((texts, dictations)) = &mut script.dictations {
                    if dictations.is_empty() {
                        return Err(io::Error::other("no dictation left").into());
                    }
                    texts.send(dictations.remove(0).to_owned()).unwrap();
                }
                return script
                    .step("wait between dictations".to_owned())
                    .map_err(Failure::paste);
            };
            let step = if keyboard.is_some() {
                "wait"
            } else {
                "wait for the clipboard alone"
            };
            script.step(step.to_owned()).map_err(Failure::paste)?;
            if let (Some(selection), Some(clipboard)) =
                (script.taken_during_the_wait.take(), clipboard)
            {
                clipboard.owned[selection as usize] = None;
                script
                    .happened
                    .push(format!("someone else sets the {selection}"));
            }
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

        inserted(&mut worker(&script, None), "Grüße");

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

        inserted(&mut worker(&script, None), "eins\nzwei\u{1b}[201~\r\n");

        assert!(
            happened(&script).contains(&"offer the clipboard: eins zwei[201~ ".to_owned()),
            "{:?}",
            happened(&script)
        );
    }

    #[test]
    fn a_final_text_of_whitespace_alone_is_not_pasted() {
        let script = script([text("copied"), text("selected")]);

        inserted(&mut worker(&script, None), " \n\t ");

        assert_eq!(happened(&script), Vec::<String>::new());
    }

    #[test]
    fn when_the_key_fails_the_previous_contents_come_back_at_once() {
        let script = script([text("copied"), text("selected")]);
        failing(&script, "press Shift+Insert", Failure::Paused);

        let troubles = worker(&script, None).insert("lost");

        assert!(
            matches!(
                troubles[..],
                [Trouble::NotPasted(PasteError::Keyboard(
                    KeyboardError::Paused
                ))]
            ),
            "{troubles:?}"
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
    fn a_session_lost_after_the_key_leaves_the_final_text_on_offer_until_the_restore() {
        let script = script([text("copied"), text("selected")]);
        failing(&script, "wait", Failure::SessionEnded);

        let troubles = worker(&script, None).insert("pasted");

        assert!(
            matches!(
                troubles[..],
                [Trouble::AfterTheKey(PasteError::Keyboard(
                    KeyboardError::Disconnected(None)
                ))]
            ),
            "{troubles:?}"
        );
        assert_eq!(
            happened(&script)[8..],
            [
                "wait",
                "wait for the clipboard alone",
                "offer the clipboard: copied",
                "offer the primary selection: selected",
            ]
        );
    }

    #[test]
    fn a_clipboard_that_fails_after_the_key_gets_the_previous_contents_back_at_once() {
        let script = script([text("copied"), text("selected")]);
        failing(&script, "wait", Failure::Clipboard);

        let troubles = worker(&script, None).insert("pasted");

        assert!(
            matches!(
                troubles[..],
                [Trouble::AfterTheKey(PasteError::Clipboard(_))]
            ),
            "{troubles:?}"
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

        let troubles = worker(&script, None).insert("lost");

        assert!(
            matches!(troubles[..], [Trouble::NotPasted(PasteError::Clipboard(_))]),
            "{troubles:?}"
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
    fn every_failure_of_one_insertion_is_told_and_only_the_failed_parts_are_opened_again() {
        let script = script([text("copied"), text("selected")]);
        failing(&script, "press Shift+Insert", Failure::Paused);
        failing(&script, "offer the clipboard: copied", Failure::Clipboard);
        let mut worker = worker(&script, None);

        let troubles = worker.insert("lost");
        let after_the_first = happened(&script);
        inserted(&mut worker, "pasted");

        assert!(
            matches!(
                troubles[..],
                [
                    Trouble::NotPasted(PasteError::Keyboard(KeyboardError::Paused)),
                    Trouble::NotRestored(Selection::Clipboard, _),
                ]
            ),
            "{troubles:?}"
        );
        assert_eq!(
            after_the_first[8..],
            [
                "offer the clipboard: copied",
                "offer the primary selection: selected",
            ]
        );
        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session without a token",
                "connect the clipboard",
            ]
        );
    }

    #[test]
    fn each_failure_says_whether_the_paste_key_was_sent() {
        let failed = || PasteError::from(KeyboardError::Paused);

        let not_pasted = Trouble::NotPasted(failed()).to_string();
        let after_the_key = Trouble::AfterTheKey(failed()).to_string();
        let not_restored =
            Trouble::NotRestored(Selection::Primary, ClipboardError::Finished).to_string();

        assert!(not_pasted.starts_with("cannot insert"), "{not_pasted}");
        assert!(
            after_the_key.starts_with("the paste key was sent"),
            "{after_the_key}"
        );
        assert!(
            not_restored.starts_with("the primary selection's previous contents"),
            "{not_restored}"
        );
    }

    #[test]
    fn a_selection_someone_else_sets_during_the_wait_is_left_alone() {
        let script = script([text("copied"), text("selected")]);
        script.borrow_mut().taken_during_the_wait = Some(Selection::Clipboard);

        inserted(&mut worker(&script, None), "pasted");

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

        inserted(&mut worker(&script, None), "pasted");

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

        inserted(&mut worker(&script, None), "pasted");

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

        assert!(!worker.insert("first").is_empty());
        inserted(&mut worker, "second");

        assert_eq!(opened(&script), OPENED);
        let happened = happened(&script);
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

        assert!(!worker.insert("lost").is_empty());
        inserted(&mut worker, "pasted");

        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session without a token",
                "open a session with the token first",
            ]
        );
        assert!(
            happened(&script).contains(&"offer the clipboard: pasted".to_owned()),
            "{:?}",
            happened(&script)
        );
        assert_eq!(
            remote_desktop::read_token(&token_file).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn after_a_session_ended_on_purpose_the_next_dictation_asks_again_and_later_ones_restore_it() {
        let dir = TempDir::new();
        let token_file = saved_token(&dir, "saved");
        let script = script([Previous::Empty, Previous::Empty]);
        script.borrow_mut().tokens = vec!["first", "second"];
        failing(&script, "wait between dictations", Failure::EndedOnPurpose);
        failing(&script, "wait between dictations", Failure::SessionEnded);
        let mut worker = worker(&script, Some(token_file));
        worker.open().unwrap();

        assert!(worker.idle().is_err());
        inserted(&mut worker, "asked");
        assert!(worker.idle().is_err());
        inserted(&mut worker, "restored");

        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session with the token saved",
                "open a session without a token",
                "open a session with the token second",
            ]
        );
    }

    #[test]
    fn after_a_denied_permission_the_next_dictation_asks_again() {
        let dir = TempDir::new();
        let token_file = saved_token(&dir, "saved");
        let script = script([Previous::Empty, Previous::Empty]);
        failing(
            &script,
            "open a session with the token saved",
            Failure::Denied,
        );
        let mut worker = worker(&script, Some(token_file));

        let troubles = worker.insert("lost");
        inserted(&mut worker, "pasted");

        assert!(
            matches!(
                troubles[..],
                [Trouble::NotPasted(PasteError::Portal(PortalError::Denied))]
            ),
            "{troubles:?}"
        );
        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session with the token saved",
                "open a session without a token",
            ]
        );
    }

    #[test]
    fn a_session_that_ends_between_dictations_is_opened_again_by_the_next_one() {
        let script = script([Previous::Empty, Previous::Empty]);
        failing(&script, "wait between dictations", Failure::SessionEnded);
        let mut worker = worker(&script, None);
        worker.open().unwrap();

        assert!(worker.idle().is_err());
        inserted(&mut worker, "pasted");

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

        assert!(!worker.insert("lost").is_empty());
        inserted(&mut worker, "pasted");

        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session without a token",
                "connect the clipboard",
            ]
        );
        assert!(
            happened(&script).contains(&"offer the clipboard: pasted".to_owned()),
            "{:?}",
            happened(&script)
        );
    }

    #[test]
    fn a_session_that_cannot_be_opened_fails_only_that_dictation() {
        let script = script([Previous::Empty, Previous::Empty]);
        failing(&script, "open a session without a token", Failure::Denied);
        let mut worker = worker(&script, None);

        let first = worker.insert("lost");
        let after_the_first = happened(&script);
        inserted(&mut worker, "pasted");

        assert!(
            matches!(
                first[..],
                [Trouble::NotPasted(PasteError::Portal(PortalError::Denied))]
            ),
            "{first:?}"
        );
        assert_eq!(after_the_first, OPENED);
        assert!(
            happened(&script).contains(&"offer the clipboard: pasted".to_owned()),
            "{:?}",
            happened(&script)
        );
    }

    #[test]
    fn a_failed_insertion_leaves_the_next_dictation_to_be_inserted() {
        let script = script([Previous::Empty, Previous::Empty]);
        failing(&script, "press Shift+Insert", Failure::Paused);

        run_dictations(&script, None, &[], vec!["lost", "pasted"]);

        assert!(
            happened(&script).contains(&"offer the clipboard: pasted".to_owned()),
            "{:?}",
            happened(&script)
        );
    }

    #[test]
    fn a_session_lost_between_dictations_is_opened_again_for_the_next_one() {
        let script = script([Previous::Empty, Previous::Empty]);
        failing(&script, "wait between dictations", Failure::SessionEnded);

        run_dictations(&script, None, &[], vec!["pasted"]);

        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session without a token",
                "open a session without a token",
            ]
        );
        assert!(
            happened(&script).contains(&"offer the clipboard: pasted".to_owned()),
            "{:?}",
            happened(&script)
        );
    }

    #[test]
    fn a_final_text_that_ends_before_the_insertion_is_ready_is_not_inserted() {
        let script = script([Previous::Empty, Previous::Empty]);

        run_dictations(&script, None, &["early"], vec!["pasted"]);

        let happened = happened(&script);
        assert!(
            !happened.contains(&"offer the clipboard: early".to_owned()),
            "{happened:?}"
        );
        assert!(
            happened.contains(&"offer the clipboard: pasted".to_owned()),
            "{happened:?}"
        );
    }

    #[test]
    fn an_insertion_that_cannot_start_inserts_no_dictation() {
        let script = script([Previous::Empty, Previous::Empty]);
        failing(&script, "open a session without a token", Failure::NoPortal);

        run_dictations(&script, None, &[], vec!["lost"]);

        assert_eq!(happened(&script), OPENED);
    }

    #[test]
    fn a_session_ended_on_purpose_after_the_key_is_asked_for_again_by_the_next_dictation() {
        let dir = TempDir::new();
        let token_file = saved_token(&dir, "saved");
        let script = script([Previous::Empty, Previous::Empty]);
        failing(&script, "wait", Failure::EndedOnPurpose);
        let mut worker = worker(&script, Some(token_file.clone()));

        assert!(!worker.insert("pasted").is_empty());
        inserted(&mut worker, "next");

        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session with the token saved",
                "open a session without a token",
            ]
        );
        assert!(!token_file.exists());
    }

    #[test]
    fn a_session_lost_after_the_key_is_opened_again_with_the_saved_token_by_the_next_dictation() {
        let dir = TempDir::new();
        let token_file = saved_token(&dir, "saved");
        let script = script([Previous::Empty, Previous::Empty]);
        script.borrow_mut().tokens = vec!["renewed"];
        failing(&script, "wait", Failure::SessionEnded);
        let mut worker = worker(&script, Some(token_file));

        assert!(!worker.insert("pasted").is_empty());
        inserted(&mut worker, "next");

        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session with the token saved",
                "open a session with the token renewed",
            ]
        );
    }

    #[test]
    fn a_wait_that_fails_again_once_the_session_is_lost_restores_at_once() {
        let script = script([text("copied"), text("selected")]);
        failing(&script, "wait", Failure::SessionEnded);
        failing(&script, "wait for the clipboard alone", Failure::Poll);

        let troubles = worker(&script, None).insert("pasted");

        assert!(
            matches!(
                troubles[..],
                [
                    Trouble::AfterTheKey(PasteError::Keyboard(KeyboardError::Disconnected(None))),
                    Trouble::AfterTheKey(PasteError::Wait(_)),
                ]
            ),
            "{troubles:?}"
        );
        assert_eq!(
            happened(&script)[8..],
            [
                "wait",
                "wait for the clipboard alone",
                "offer the clipboard: copied",
                "offer the primary selection: selected",
            ]
        );
    }

    #[test]
    fn a_daemon_started_again_after_a_session_ended_on_purpose_asks_again_too() {
        let dir = TempDir::new();
        let token_file = saved_token(&dir, "saved");
        let script = script([Previous::Empty, Previous::Empty]);
        failing(&script, "wait between dictations", Failure::EndedOnPurpose);
        let mut before = worker(&script, Some(token_file.clone()));
        before.open().unwrap();
        assert!(before.idle().is_err());
        drop(before);

        inserted(&mut worker(&script, Some(token_file)), "pasted");

        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session with the token saved",
                "connect the clipboard",
                "open a session without a token",
            ]
        );
    }

    #[test]
    fn a_daemon_started_again_after_a_denied_permission_asks_again_too() {
        let dir = TempDir::new();
        let token_file = saved_token(&dir, "saved");
        let script = script([Previous::Empty, Previous::Empty]);
        failing(
            &script,
            "open a session with the token saved",
            Failure::Denied,
        );
        assert!(
            !worker(&script, Some(token_file.clone()))
                .insert("lost")
                .is_empty()
        );

        inserted(&mut worker(&script, Some(token_file)), "pasted");

        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session with the token saved",
                "connect the clipboard",
                "open a session without a token",
            ]
        );
    }

    #[test]
    fn a_token_left_behind_is_deleted_when_the_session_that_asked_again_brings_none() {
        let dir = TempDir::new();
        let token_file = saved_token(&dir, "saved");
        let token_dir = token_file.parent().unwrap().to_owned();
        let script = script([Previous::Empty, Previous::Empty]);
        failing(&script, "wait between dictations", Failure::EndedOnPurpose);
        let mut worker = worker(&script, Some(token_file.clone()));
        worker.open().unwrap();

        fs::set_permissions(&token_dir, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(worker.idle().is_err());
        fs::set_permissions(&token_dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(token_file.exists());
        inserted(&mut worker, "asked");

        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session with the token saved",
                "open a session without a token",
            ]
        );
        assert!(!token_file.exists());
    }

    #[test]
    fn the_token_of_a_session_whose_keyboard_does_not_connect_is_kept() {
        let dir = TempDir::new();
        let token_file = saved_token(&dir, "saved");
        let script = script([Previous::Empty, Previous::Empty]);
        script.borrow_mut().tokens = vec!["renewed", "later"];
        failing(&script, "connect the keyboard", Failure::SessionEnded);
        let mut worker = worker(&script, Some(token_file.clone()));

        assert!(!worker.insert("lost").is_empty());
        assert_eq!(
            remote_desktop::read_token(&token_file).as_deref(),
            Some("renewed")
        );
        inserted(&mut worker, "pasted");

        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session with the token saved",
                "open a session with the token renewed",
            ]
        );
    }

    #[test]
    fn the_clipboard_is_served_while_the_portal_waits_for_the_user() {
        let script = script([Previous::Empty, Previous::Empty]);
        script.borrow_mut().portal_answers = PortalAnswers::AtTheNextWait;

        inserted(&mut worker(&script, None), "pasted");

        assert_eq!(
            happened(&script)[..4],
            [
                "connect the clipboard",
                "open a session without a token",
                "wait for the portal",
                "take the clipboard",
            ]
        );
    }

    #[test]
    fn a_clipboard_that_fails_while_the_portal_waits_is_not_served_again_and_the_session_opens() {
        let script = script([Previous::Empty, Previous::Empty]);
        script.borrow_mut().portal_answers = PortalAnswers::Late;
        failing(&script, "wait for the portal", Failure::Clipboard);

        let troubles = worker(&script, None).insert("pasted");

        assert!(troubles.is_empty(), "{troubles:?}");
        assert_eq!(
            happened(&script)[..4],
            [
                "connect the clipboard",
                "open a session without a token",
                "wait for the portal",
                "take the clipboard",
            ]
        );
    }

    #[test]
    fn a_permission_denied_at_the_start_is_asked_for_again_by_the_next_dictation() {
        let dir = TempDir::new();
        let token_file = saved_token(&dir, "saved");
        let script = script([Previous::Empty, Previous::Empty]);
        failing(
            &script,
            "open a session with the token saved",
            Failure::Denied,
        );

        run_dictations(&script, Some(token_file.clone()), &[], vec!["pasted"]);

        assert_eq!(
            opened(&script),
            [
                "connect the clipboard",
                "open a session with the token saved",
                "open a session without a token",
            ]
        );
        assert!(
            happened(&script).contains(&"offer the clipboard: pasted".to_owned()),
            "{:?}",
            happened(&script)
        );
        assert!(!token_file.exists());
    }
}
