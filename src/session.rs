use std::error::Error;
use std::mem;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

pub(crate) type BoxError = Box<dyn Error + Send + Sync>;

/// The audio of one dictation, mono at [`Recording::SAMPLE_RATE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recording {
    pub(crate) samples: Vec<i16>,
}

impl Recording {
    pub(crate) const SAMPLE_RATE: u32 = 16_000;
}

pub(crate) trait AudioSource: Send + Sync {
    fn start(&self) -> Result<Box<dyn Capture>, BoxError>;
}

/// A stopped capture: its audio, and how often the audio stack recovered from an error
/// during it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stopped {
    pub(crate) recording: Recording,
    pub(crate) glitches: u32,
}

pub(crate) trait Capture: Send {
    fn stop(self: Box<Self>) -> Result<Stopped, BoxError>;
}

pub(crate) trait FinalServer: Send + Sync {
    fn transcribe(&self, recording: &Recording) -> Result<String, BoxError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalText {
    pub(crate) text: String,
    pub(crate) glitches: u32,
}

pub(crate) type Transcript = Result<FinalText, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Started,
    /// The toggle changed nothing: no recording started, or a transcription still runs.
    Refused(String),
    Text(FinalText),
    Failed(String),
}

impl Outcome {
    pub(crate) fn into_transcript(self) -> Option<Transcript> {
        match self {
            Self::Text(text) => Some(Ok(text)),
            Self::Failed(message) => Some(Err(message)),
            Self::Started | Self::Refused(_) => None,
        }
    }
}

impl From<Transcript> for Outcome {
    fn from(transcript: Transcript) -> Self {
        match transcript {
            Ok(text) => Self::Text(text),
            Err(message) => Self::Failed(message),
        }
    }
}

enum State {
    Idle,
    Recording { id: u64, capture: Box<dyn Capture> },
    Stopping,
    AtLimit { waiter: bool },
    Finished(Transcript),
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
                shared.state = State::Stopping;
                drop(shared);
                self.changed.notify_all();
                let transcript = self.finish(capture);
                self.set(State::Idle);
                transcript.into()
            }
            State::AtLimit { waiter: false } => {
                shared.state = State::AtLimit { waiter: true };
                self.wait_for_transcript(shared).into()
            }
            busy @ (State::Stopping | State::AtLimit { waiter: true }) => {
                shared.state = busy;
                Outcome::Refused("a transcription is already in progress".to_owned())
            }
            State::Finished(transcript) => transcript.into(),
        }
    }

    /// Holds a transcript whose toggle went away for the next toggle; false when a new
    /// dictation has already begun.
    pub(crate) fn keep(&self, transcript: Transcript) -> bool {
        let mut shared = self.lock();
        if !matches!(shared.state, State::Idle) {
            return false;
        }
        shared.state = State::Finished(transcript);
        true
    }

    #[cfg(test)]
    pub(crate) fn holds_a_transcript(&self) -> bool {
        matches!(self.lock().state, State::Finished(_))
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
            Err(error) => Outcome::Refused(format!("cannot start the recording: {error}")),
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
        let capture = match mem::replace(&mut shared.state, State::AtLimit { waiter: false }) {
            State::Recording {
                id: current,
                capture,
            } if current == id => capture,
            other => {
                shared.state = other;
                return;
            }
        };
        drop(shared);
        let transcript = self.finish(capture);
        self.set(State::Finished(transcript));
    }

    fn wait_for_transcript(&self, mut shared: MutexGuard<'_, Shared>) -> Transcript {
        loop {
            match mem::replace(&mut shared.state, State::Idle) {
                State::Finished(transcript) => return transcript,
                other => {
                    shared.state = other;
                    shared = self
                        .changed
                        .wait(shared)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }

    fn finish(&self, capture: Box<dyn Capture>) -> Transcript {
        let Stopped {
            recording,
            glitches,
        } = capture
            .stop()
            .map_err(|error| format!("the recording failed: {error}"))?;
        if recording.samples.is_empty() {
            return Err("the microphone delivered no audio, so nothing was sent".to_owned());
        }
        let text = self
            .final_server
            .transcribe(&recording)
            .map_err(|error| format!("the final server failed: {error}"))?;
        Ok(FinalText { text, glitches })
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
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::{FinalText, Outcome, Recording, Session};
    use crate::test_support::{
        BrokenMicrophone, FakeFinalServer, FakeMicrophone, MicrophoneThatStops, wait_until,
    };

    const NO_LIMIT: Duration = Duration::from_secs(3600);
    const WAIT: Duration = Duration::from_secs(10);

    fn final_text(text: &str) -> FinalText {
        FinalText {
            text: text.to_owned(),
            glitches: 0,
        }
    }

    fn text(text: &str) -> Outcome {
        Outcome::Text(final_text(text))
    }

    #[test]
    fn a_toggle_starts_a_recording_and_the_next_returns_its_final_text() {
        let (microphone, events) = FakeMicrophone::new(vec![1, 2, 3]);
        let final_server = FakeFinalServer::answering(&[Ok("hello")]);
        let received = final_server.received.clone();
        let session = Session::new(Box::new(microphone), Box::new(final_server), NO_LIMIT);

        assert_eq!(session.toggle(), Outcome::Started);
        assert_eq!(events.try_recv(), Ok("start"));
        assert_eq!(session.toggle(), text("hello"));
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
        assert_eq!(session.toggle(), text("two"));
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

        assert_eq!(session.toggle(), text("at the limit"));
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

        assert_eq!(toggle.join().unwrap(), text("waited for"));
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
            Outcome::Refused("a transcription is already in progress".to_owned())
        );
        release.send(()).unwrap();
        assert_eq!(stopping.join().unwrap(), text("first"));
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
        assert_eq!(session.toggle(), text("again"));
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
            Outcome::Refused("cannot start the recording: no input device".to_owned())
        );
    }

    #[test]
    fn the_limit_timer_of_a_stopped_recording_does_not_touch_the_next_one() {
        let (microphone, events) = FakeMicrophone::new(vec![1]);
        let final_server = FakeFinalServer::answering(&[Ok("first"), Ok("second")]);
        let session = Session::new(
            Box::new(microphone),
            Box::new(final_server),
            Duration::from_millis(2000),
        );
        session.toggle();
        thread::sleep(Duration::from_millis(1200));
        session.toggle();

        session.toggle();
        thread::sleep(Duration::from_millis(1200));

        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            ["start", "stop", "start"]
        );
        assert_eq!(session.toggle(), text("second"));
    }

    #[test]
    fn a_recording_without_audio_is_reported_and_not_sent() {
        let (microphone, _events) = FakeMicrophone::new(Vec::new());
        let final_server = FakeFinalServer::answering(&[]);
        let received = final_server.received.clone();
        let session = Session::new(Box::new(microphone), Box::new(final_server), NO_LIMIT);

        session.toggle();

        assert_eq!(
            session.toggle(),
            Outcome::Failed("the microphone delivered no audio, so nothing was sent".to_owned())
        );
        assert!(received.lock().unwrap().is_empty());
    }

    #[test]
    fn a_microphone_that_stops_during_the_recording_fails_the_dictation() {
        let session = Session::new(
            Box::new(MicrophoneThatStops),
            Box::new(FakeFinalServer::answering(&[])),
            NO_LIMIT,
        );

        session.toggle();

        assert_eq!(
            session.toggle(),
            Outcome::Failed("the recording failed: the device is gone".to_owned())
        );
    }

    #[test]
    fn the_glitches_of_a_recording_reach_its_final_text() {
        let (microphone, _events) = FakeMicrophone::glitching(vec![1], 5);
        let final_server = FakeFinalServer::answering(&[Ok("glitchy")]);
        let session = Session::new(Box::new(microphone), Box::new(final_server), NO_LIMIT);

        session.toggle();

        assert_eq!(
            session.toggle(),
            Outcome::Text(FinalText {
                text: "glitchy".to_owned(),
                glitches: 5,
            })
        );
    }

    #[test]
    fn a_kept_transcript_is_returned_by_the_next_toggle_instead_of_a_new_recording() {
        let (microphone, events) = FakeMicrophone::new(vec![1]);
        let final_server = FakeFinalServer::answering(&[Ok("undelivered")]);
        let session = Session::new(Box::new(microphone), Box::new(final_server), NO_LIMIT);
        session.toggle();
        let outcome = session.toggle();

        assert!(session.keep(outcome.into_transcript().unwrap()));

        assert_eq!(session.toggle(), text("undelivered"));
        assert_eq!(events.try_iter().collect::<Vec<_>>(), ["start", "stop"]);
        assert_eq!(
            session.toggle(),
            Outcome::Started,
            "the session is ready again"
        );
    }

    #[test]
    fn a_transcript_is_not_kept_once_a_new_recording_runs() {
        let (microphone, _events) = FakeMicrophone::new(vec![1]);
        let final_server = FakeFinalServer::answering(&[Ok("next")]);
        let session = Session::new(Box::new(microphone), Box::new(final_server), NO_LIMIT);
        session.toggle();

        assert!(!session.keep(Ok(final_text("late"))));

        assert_eq!(session.toggle(), text("next"));
    }
}
