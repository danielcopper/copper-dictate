use std::error::Error;
use std::mem;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

pub(crate) type BoxError = Box<dyn Error + Send + Sync>;

/// The audio of one dictation, 16 kHz mono.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recording {
    pub(crate) samples: Vec<i16>,
}

pub(crate) trait AudioSource: Send + Sync {
    fn start(&self) -> Result<Box<dyn Capture>, BoxError>;
}

/// A recording in progress.
pub(crate) trait Capture: Send {
    fn stop(self: Box<Self>) -> Result<Recording, BoxError>;
}

pub(crate) trait FinalServer: Send + Sync {
    fn transcribe(&self, recording: &Recording) -> Result<String, BoxError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Started,
    Text(String),
    Failed(String),
}

enum State {
    Idle,
    Recording {
        id: u64,
        capture: Box<dyn Capture>,
    },
    /// `claimed` once a toggle waits for this transcription's outcome.
    Transcribing {
        claimed: bool,
    },
    /// The outcome of a recording that stopped at the limit, kept for the next toggle.
    Finished(Outcome),
}

struct Shared {
    state: State,
    next_id: u64,
}

impl Shared {
    fn recording_id(&self) -> Option<u64> {
        match self.state {
            State::Recording { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// One dictation at a time: a toggle starts a recording, the next stops it and
/// returns the final text.
///
/// A recording that reaches the limit stops by itself and is transcribed; the
/// next toggle returns that text instead of starting a new recording.
pub(crate) struct Session {
    audio: Box<dyn AudioSource>,
    final_server: Box<dyn FinalServer>,
    max_recording: Duration,
    shared: Mutex<Shared>,
    changed: Condvar,
}

impl Session {
    pub(crate) fn new(
        audio: Box<dyn AudioSource>,
        final_server: Box<dyn FinalServer>,
        max_recording: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            audio,
            final_server,
            max_recording,
            shared: Mutex::new(Shared {
                state: State::Idle,
                next_id: 0,
            }),
            changed: Condvar::new(),
        })
    }

    pub(crate) fn toggle(self: &Arc<Self>) -> Outcome {
        let mut shared = self.lock();
        match mem::replace(&mut shared.state, State::Idle) {
            State::Idle => self.start(shared),
            State::Recording { capture, .. } => {
                shared.state = State::Transcribing { claimed: true };
                drop(shared);
                self.changed.notify_all();
                let outcome = self.finish(capture);
                self.set(State::Idle);
                outcome
            }
            State::Transcribing { claimed: false } => {
                shared.state = State::Transcribing { claimed: true };
                let mut shared = self
                    .changed
                    .wait_while(shared, |shared| {
                        matches!(shared.state, State::Transcribing { .. })
                    })
                    .unwrap_or_else(PoisonError::into_inner);
                match mem::replace(&mut shared.state, State::Idle) {
                    State::Finished(outcome) => outcome,
                    other => {
                        shared.state = other;
                        Outcome::Failed("the transcription ended without a result".to_owned())
                    }
                }
            }
            State::Transcribing { claimed: true } => {
                shared.state = State::Transcribing { claimed: true };
                Outcome::Failed("a transcription is already in progress".to_owned())
            }
            State::Finished(outcome) => outcome,
        }
    }

    fn start(self: &Arc<Self>, mut shared: MutexGuard<'_, Shared>) -> Outcome {
        match self.audio.start() {
            Ok(capture) => {
                let id = shared.next_id;
                shared.next_id += 1;
                shared.state = State::Recording { id, capture };
                drop(shared);
                let session = Arc::clone(self);
                thread::spawn(move || session.stop_at_limit(id));
                Outcome::Started
            }
            Err(error) => Outcome::Failed(format!("cannot start the recording: {error}")),
        }
    }

    fn stop_at_limit(&self, id: u64) {
        let shared = self.lock();
        let (mut shared, _) = self
            .changed
            .wait_timeout_while(shared, self.max_recording, |shared| {
                shared.recording_id() == Some(id)
            })
            .unwrap_or_else(PoisonError::into_inner);
        if shared.recording_id() != Some(id) {
            return;
        }
        let State::Recording { capture, .. } =
            mem::replace(&mut shared.state, State::Transcribing { claimed: false })
        else {
            return;
        };
        drop(shared);
        let outcome = self.finish(capture);
        self.set(State::Finished(outcome));
    }

    fn finish(&self, capture: Box<dyn Capture>) -> Outcome {
        let recording = match capture.stop() {
            Ok(recording) => recording,
            Err(error) => return Outcome::Failed(format!("the recording failed: {error}")),
        };
        match self.final_server.transcribe(&recording) {
            Ok(text) => Outcome::Text(text),
            Err(error) => Outcome::Failed(format!("the final server failed: {error}")),
        }
    }

    fn set(&self, state: State) {
        self.lock().state = state;
        self.changed.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
pub(crate) mod fakes {
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex};

    use super::{AudioSource, BoxError, Capture, FinalServer, Recording};

    /// Hands out recordings of the given samples, and reports each start and stop.
    pub(crate) struct FakeMicrophone {
        samples: Vec<i16>,
        events: Sender<&'static str>,
    }

    impl FakeMicrophone {
        pub(crate) fn new(samples: Vec<i16>) -> (Self, Receiver<&'static str>) {
            let (events, received) = mpsc::channel();
            (Self { samples, events }, received)
        }
    }

    impl AudioSource for FakeMicrophone {
        fn start(&self) -> Result<Box<dyn Capture>, BoxError> {
            self.events.send("start")?;
            Ok(Box::new(FakeCapture {
                samples: self.samples.clone(),
                events: self.events.clone(),
            }))
        }
    }

    struct FakeCapture {
        samples: Vec<i16>,
        events: Sender<&'static str>,
    }

    impl Capture for FakeCapture {
        fn stop(self: Box<Self>) -> Result<Recording, BoxError> {
            self.events.send("stop")?;
            Ok(Recording {
                samples: self.samples,
            })
        }
    }

    pub(crate) struct BrokenMicrophone;

    impl AudioSource for BrokenMicrophone {
        fn start(&self) -> Result<Box<dyn Capture>, BoxError> {
            Err("no input device".into())
        }
    }

    /// Answers each recording with the next of the given results, and keeps the recordings.
    pub(crate) struct FakeFinalServer {
        answers: Mutex<Vec<Result<String, String>>>,
        pub(crate) received: Arc<Mutex<Vec<Recording>>>,
        release: Option<Mutex<Receiver<()>>>,
    }

    impl FakeFinalServer {
        pub(crate) fn answering(answers: &[Result<&str, &str>]) -> Self {
            Self {
                answers: Mutex::new(
                    answers
                        .iter()
                        .rev()
                        .map(|answer| answer.map(str::to_owned).map_err(str::to_owned))
                        .collect(),
                ),
                received: Arc::default(),
                release: None,
            }
        }

        /// Holds each answer back until the returned sender sends.
        pub(crate) fn held(mut self) -> (Self, Sender<()>) {
            let (release, wait) = mpsc::channel();
            self.release = Some(Mutex::new(wait));
            (self, release)
        }
    }

    impl FinalServer for FakeFinalServer {
        fn transcribe(&self, recording: &Recording) -> Result<String, BoxError> {
            self.received.lock().unwrap().push(recording.clone());
            if let Some(release) = &self.release {
                release.lock().unwrap().recv()?;
            }
            let answer = self.answers.lock().unwrap().pop().expect("an answer left");
            answer.map_err(Into::into)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::fakes::{BrokenMicrophone, FakeFinalServer, FakeMicrophone};
    use super::{Outcome, Recording, Session};

    const NO_LIMIT: Duration = Duration::from_secs(3600);
    const WAIT: Duration = Duration::from_secs(10);

    #[test]
    fn a_toggle_starts_a_recording_and_the_next_returns_its_final_text() {
        let (microphone, events) = FakeMicrophone::new(vec![1, 2, 3]);
        let final_server = FakeFinalServer::answering(&[Ok("hello")]);
        let received = final_server.received.clone();
        let session = Session::new(Box::new(microphone), Box::new(final_server), NO_LIMIT);

        assert_eq!(session.toggle(), Outcome::Started);
        assert_eq!(events.try_recv(), Ok("start"));
        assert_eq!(session.toggle(), Outcome::Text("hello".to_owned()));
        assert_eq!(events.try_recv(), Ok("stop"));
        assert_eq!(
            *received.lock().unwrap(),
            [Recording {
                samples: vec![1, 2, 3]
            }]
        );
    }

    #[test]
    fn the_toggle_after_a_dictation_starts_the_next_one() {
        let (microphone, events) = FakeMicrophone::new(vec![1]);
        let final_server = FakeFinalServer::answering(&[Ok("one"), Ok("two")]);
        let session = Session::new(Box::new(microphone), Box::new(final_server), NO_LIMIT);

        session.toggle();
        session.toggle();

        assert_eq!(session.toggle(), Outcome::Started);
        assert_eq!(session.toggle(), Outcome::Text("two".to_owned()));
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            ["start", "stop", "start", "stop"]
        );
    }

    #[test]
    fn a_recording_that_reaches_the_limit_stops_and_is_transcribed_by_itself() {
        let (microphone, events) = FakeMicrophone::new(vec![7; 100]);
        let final_server = FakeFinalServer::answering(&[Ok("at the limit")]);
        let received = final_server.received.clone();
        let session = Session::new(
            Box::new(microphone),
            Box::new(final_server),
            Duration::from_millis(50),
        );

        assert_eq!(session.toggle(), Outcome::Started);
        assert_eq!(events.recv_timeout(WAIT), Ok("start"));
        assert_eq!(events.recv_timeout(WAIT), Ok("stop"));
        wait_until(|| !received.lock().unwrap().is_empty());

        assert_eq!(session.toggle(), Outcome::Text("at the limit".to_owned()));
        assert_eq!(
            *received.lock().unwrap(),
            [Recording {
                samples: vec![7; 100]
            }]
        );
        assert_eq!(events.try_recv().ok(), None, "no new recording started");
    }

    #[test]
    fn a_toggle_during_the_transcription_at_the_limit_waits_for_its_text() {
        let (microphone, events) = FakeMicrophone::new(vec![1]);
        let (final_server, release) = FakeFinalServer::answering(&[Ok("waited for")]).held();
        let received = final_server.received.clone();
        let session = Session::new(
            Box::new(microphone),
            Box::new(final_server),
            Duration::from_millis(50),
        );
        session.toggle();
        wait_until(|| !received.lock().unwrap().is_empty());

        let toggle = thread::spawn({
            let session = session.clone();
            move || session.toggle()
        });
        thread::sleep(Duration::from_millis(50));
        release.send(()).unwrap();

        assert_eq!(
            toggle.join().unwrap(),
            Outcome::Text("waited for".to_owned())
        );
        assert_eq!(events.try_iter().collect::<Vec<_>>(), ["start", "stop"]);
    }

    #[test]
    fn a_toggle_during_a_stopping_transcription_is_refused() {
        let (microphone, _events) = FakeMicrophone::new(vec![1]);
        let (final_server, release) = FakeFinalServer::answering(&[Ok("first")]).held();
        let received = final_server.received.clone();
        let session = Session::new(Box::new(microphone), Box::new(final_server), NO_LIMIT);
        session.toggle();
        let stopping = thread::spawn({
            let session = session.clone();
            move || session.toggle()
        });
        wait_until(|| !received.lock().unwrap().is_empty());

        assert_eq!(
            session.toggle(),
            Outcome::Failed("a transcription is already in progress".to_owned())
        );
        release.send(()).unwrap();
        assert_eq!(stopping.join().unwrap(), Outcome::Text("first".to_owned()));
    }

    #[test]
    fn a_failed_final_request_is_reported_and_the_session_is_ready_again() {
        let (microphone, _events) = FakeMicrophone::new(vec![1]);
        let final_server = FakeFinalServer::answering(&[Err("connection refused"), Ok("again")]);
        let session = Session::new(Box::new(microphone), Box::new(final_server), NO_LIMIT);

        session.toggle();
        assert_eq!(
            session.toggle(),
            Outcome::Failed("the final server failed: connection refused".to_owned())
        );
        assert_eq!(session.toggle(), Outcome::Started);
        assert_eq!(session.toggle(), Outcome::Text("again".to_owned()));
    }

    #[test]
    fn a_microphone_that_does_not_start_is_reported() {
        let session = Session::new(
            Box::new(BrokenMicrophone),
            Box::new(FakeFinalServer::answering(&[])),
            NO_LIMIT,
        );

        assert_eq!(
            session.toggle(),
            Outcome::Failed("cannot start the recording: no input device".to_owned())
        );
    }

    #[test]
    fn the_limit_timer_of_a_stopped_recording_does_not_touch_the_next_one() {
        let (microphone, events) = FakeMicrophone::new(vec![1]);
        let final_server = FakeFinalServer::answering(&[Ok("first"), Ok("second")]);
        let session = Session::new(
            Box::new(microphone),
            Box::new(final_server),
            Duration::from_millis(1000),
        );
        session.toggle();
        thread::sleep(Duration::from_millis(600));
        session.toggle();

        session.toggle();
        thread::sleep(Duration::from_millis(600));

        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            ["start", "stop", "start"]
        );
        assert_eq!(session.toggle(), Outcome::Text("second".to_owned()));
    }

    fn wait_until(condition: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + WAIT;
        while !condition() {
            assert!(std::time::Instant::now() < deadline, "timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }
}
