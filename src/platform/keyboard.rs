use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use reis::PendingRequestResult;
use reis::ei::{self, handshake::ContextType, keyboard::KeyState};
use reis::event::{self, DeviceCapability, EiEvent, EiEventConverter};
use reis::handshake::ei_handshake_blocking;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::time::{ClockId, clock_gettime};
use thiserror::Error;

const KEY_LEFTSHIFT: u32 = 42;
const KEY_INSERT: u32 = 110;
const DEVICE_TIMEOUT: Duration = Duration::from_secs(5);

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
    #[error("the compositor ended the input emulation ({})", .0.as_deref().unwrap_or("no reason given"))]
    Disconnected(Option<String>),
    #[error("the compositor offers no emulated keyboard")]
    NoKeyboard,
}

pub(crate) struct Keyboard {
    context: ei::Context,
    connection: event::Connection,
    converter: EiEventConverter,
    device: Option<event::Device>,
    resumed: bool,
    sequence: u32,
}

impl Keyboard {
    pub(crate) fn connect(eis: OwnedFd) -> Result<Self, KeyboardError> {
        let context = ei::Context::new(eis.into())?;
        let handshake = ei_handshake_blocking(&context, "copper-dictate", ContextType::Sender)?;
        let converter = EiEventConverter::new(&context, handshake);
        let mut keyboard = Self {
            connection: converter.connection().clone(),
            context,
            converter,
            device: None,
            resumed: false,
            sequence: 0,
        };
        let deadline = Instant::now() + DEVICE_TIMEOUT;
        while !keyboard.resumed {
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or(KeyboardError::NoKeyboard)?;
            let timeout = Timespec::try_from(left).map_err(io::Error::other)?;
            let mut fds = [PollFd::new(&keyboard.context, PollFlags::IN)];
            poll(&mut fds, Some(&timeout)).map_err(io::Error::from)?;
            keyboard.dispatch()?;
        }
        Ok(keyboard)
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.context.as_fd()
    }

    /// Handles what the compositor sent; call it when the socket is readable.
    pub(crate) fn dispatch(&mut self) -> Result<(), KeyboardError> {
        match self.context.read() {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(KeyboardError::Disconnected(None));
            }
            Err(error) => return Err(error.into()),
        }
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
        let device = self
            .device
            .as_ref()
            .filter(|_| self.resumed)
            .ok_or(KeyboardError::NoKeyboard)?;
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
            EiEvent::Disconnected(disconnected) => {
                return Err(KeyboardError::Disconnected(disconnected.explanation));
            }
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

fn now_in_microseconds() -> u64 {
    let now = clock_gettime(ClockId::Monotonic);
    let seconds = u64::try_from(now.tv_sec).unwrap_or_default();
    let micros = u64::try_from(now.tv_nsec / 1000).unwrap_or_default();
    seconds * 1_000_000 + micros
}
