use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use reis::PendingRequestResult;
use reis::ei::{self, connection::DisconnectReason, handshake::ContextType, keyboard::KeyState};
use reis::event::{self, DeviceCapability, EiEvent, EiEventConverter};
use reis::handshake::{EiHandshaker, HandshakeError, HandshakeResp};
use rustix::event::{PollFd, PollFlags};
use rustix::time::{ClockId, clock_gettime};
use thiserror::Error;

use crate::platform;
use crate::platform::remote_desktop::RemoteDesktop;

const KEY_LEFTSHIFT: u32 = 42;
const KEY_INSERT: u32 = 110;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub(crate) enum KeyboardError {
    #[error("the input emulation socket failed: {0}")]
    Io(#[from] io::Error),
    #[error("the input emulation failed: {0}")]
    Protocol(#[from] reis::Error),
    #[error("the input emulation failed: {0}")]
    Event(#[from] event::EventError),
    #[error("the input emulation's message is not understood: {0}")]
    Parse(String),
    #[error("the desktop ended the remote control session ({})", .0.as_deref().unwrap_or("no reason given"))]
    Ended(Option<String>),
    #[error("the compositor ended the input emulation ({})", .0.as_deref().unwrap_or("no reason given"))]
    Disconnected(Option<String>),
    #[error("the compositor did not set the input emulation up within {} s", CONNECT_TIMEOUT.as_secs())]
    NoAnswer,
    #[error("the compositor offers no emulated keyboard")]
    NoKeyboard,
    #[error("the compositor has paused the emulated keyboard")]
    Paused,
}

impl KeyboardError {
    /// Whether the input emulation is gone, so that only a new session brings it back.
    pub(crate) fn ends_the_session(&self) -> bool {
        !matches!(self, Self::NoKeyboard | Self::Paused)
    }
}

pub(crate) struct Keyboard {
    context: ei::Context,
    connection: event::Connection,
    converter: EiEventConverter,
    device: Option<event::Device>,
    resumed: bool,
    sequence: u32,
    _portal: zbus::Connection,
}

impl Keyboard {
    /// Sets the input emulation up and waits for its keyboard, both within a few seconds.
    pub(crate) fn connect(session: RemoteDesktop) -> Result<Self, KeyboardError> {
        let (portal, eis) = session.into_parts();
        let context = ei::Context::new(UnixStream::from(eis))?;
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let handshake = handshake(&context, deadline)?;
        let converter = EiEventConverter::new(&context, handshake);
        let mut keyboard = Self {
            connection: converter.connection().clone(),
            context,
            converter,
            device: None,
            resumed: false,
            sequence: 0,
            _portal: portal,
        };
        while !keyboard.resumed {
            if Instant::now() >= deadline {
                return Err(if keyboard.device.is_some() {
                    KeyboardError::Paused
                } else {
                    KeyboardError::NoKeyboard
                });
            }
            if platform::poll_until(
                &mut [PollFd::new(&keyboard.context, PollFlags::IN)],
                Some(deadline),
            )? {
                keyboard.dispatch()?;
            }
        }
        Ok(keyboard)
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.context.as_fd()
    }

    /// Handles what the compositor sent; call it when the socket is readable.
    pub(crate) fn dispatch(&mut self) -> Result<(), KeyboardError> {
        read(&self.context)?;
        while let Some(result) = self.context.pending_event() {
            match result {
                PendingRequestResult::Request(event) => self.converter.handle_event(event)?,
                PendingRequestResult::ParseError(error) => {
                    return Err(KeyboardError::Parse(error.to_string()));
                }
                PendingRequestResult::InvalidObject(_) => {}
            }
        }
        while let Some(event) = self.converter.next_event() {
            self.handle(event)?;
        }
        self.flush()
    }

    /// Presses and releases Shift+Insert, which pastes whatever the keyboard layout.
    pub(crate) fn paste(&mut self) -> Result<(), KeyboardError> {
        let device = self.device.as_ref().ok_or(KeyboardError::NoKeyboard)?;
        if !self.resumed {
            return Err(KeyboardError::Paused);
        }
        let keyboard: ei::Keyboard = device.interface().ok_or(KeyboardError::NoKeyboard)?;
        let serial = self.connection.serial();
        device.device().start_emulating(serial, self.sequence);
        self.sequence = self.sequence.wrapping_add(1);
        for (key, state) in [
            (KEY_LEFTSHIFT, KeyState::Press),
            (KEY_INSERT, KeyState::Press),
            (KEY_INSERT, KeyState::Released),
            (KEY_LEFTSHIFT, KeyState::Released),
        ] {
            keyboard.key(key, state);
            device.device().frame(serial, now_in_microseconds());
        }
        device.device().stop_emulating(serial);
        self.flush()
    }

    fn handle(&mut self, event: EiEvent) -> Result<(), KeyboardError> {
        match event {
            EiEvent::SeatAdded(added) => {
                added
                    .seat
                    .bind_capabilities(DeviceCapability::Keyboard.into());
            }
            EiEvent::DeviceAdded(added)
                if added.device.has_capability(DeviceCapability::Keyboard) =>
            {
                self.device = Some(added.device);
                self.resumed = false;
            }
            EiEvent::DeviceResumed(resumed) if self.device.as_ref() == Some(&resumed.device) => {
                self.resumed = true;
            }
            EiEvent::DevicePaused(paused) if self.device.as_ref() == Some(&paused.device) => {
                self.resumed = false;
            }
            EiEvent::DeviceRemoved(removed) if self.device.as_ref() == Some(&removed.device) => {
                self.device = None;
                self.resumed = false;
            }
            EiEvent::Disconnected(disconnected) => return Err(lost(disconnected)),
            _ => {}
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), KeyboardError> {
        match self.connection.flush() {
            Err(error) if error != rustix::io::Errno::AGAIN => Err(io::Error::from(error).into()),
            _ => Ok(()),
        }
    }
}

/// The handshake `reis` does blocking, with a deadline, so a silent compositor cannot hold
/// the insertion up.
fn handshake(context: &ei::Context, deadline: Instant) -> Result<HandshakeResp, KeyboardError> {
    let mut handshaker = EiHandshaker::new("copper-dictate", ContextType::Sender);
    loop {
        if Instant::now() >= deadline {
            return Err(KeyboardError::NoAnswer);
        }
        if !platform::poll_until(&mut [PollFd::new(context, PollFlags::IN)], Some(deadline))? {
            continue;
        }
        read(context)?;
        while let Some(result) = context.pending_event() {
            let event = match result {
                PendingRequestResult::Request(event) => event,
                PendingRequestResult::ParseError(error) => {
                    return Err(KeyboardError::Parse(error.to_string()));
                }
                PendingRequestResult::InvalidObject(id) => {
                    return Err(reis::Error::from(HandshakeError::InvalidObject(id)).into());
                }
            };
            if let Some(response) = handshaker.handle_event(event).map_err(reis::Error::from)? {
                return Ok(response);
            }
        }
    }
}

/// A disconnect on purpose is how the compositor ends the input emulation when the portal
/// closes the session; any other reason is a failure.
fn lost(disconnected: event::Disconnected) -> KeyboardError {
    match disconnected.reason {
        DisconnectReason::Disconnected => KeyboardError::Ended(disconnected.explanation),
        _ => KeyboardError::Disconnected(disconnected.explanation),
    }
}

fn read(context: &ei::Context) -> Result<(), KeyboardError> {
    match context.read() {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            Err(KeyboardError::Disconnected(None))
        }
        Err(error) => Err(error.into()),
    }
}

fn now_in_microseconds() -> u64 {
    let now = clock_gettime(ClockId::Monotonic);
    let seconds = u64::try_from(now.tv_sec).unwrap_or_default();
    let micros = u64::try_from(now.tv_nsec / 1000).unwrap_or_default();
    seconds * 1_000_000 + micros
}

#[cfg(test)]
mod tests {
    use reis::ei::connection::DisconnectReason;
    use reis::event::Disconnected;

    use super::{KeyboardError, lost};

    fn disconnected(reason: DisconnectReason) -> Disconnected {
        Disconnected {
            last_serial: 0,
            reason,
            explanation: Some("why".to_owned()),
        }
    }

    #[test]
    fn a_disconnect_on_purpose_ends_the_session_and_any_other_is_a_failure() {
        assert!(matches!(
            lost(disconnected(DisconnectReason::Disconnected)),
            KeyboardError::Ended(Some(why)) if why == "why"
        ));
        for reason in [
            DisconnectReason::Error,
            DisconnectReason::Mode,
            DisconnectReason::Protocol,
            DisconnectReason::Value,
            DisconnectReason::Transport,
        ] {
            assert!(
                matches!(
                    lost(disconnected(reason)),
                    KeyboardError::Disconnected(Some(_))
                ),
                "{reason:?}"
            );
        }
    }
}
