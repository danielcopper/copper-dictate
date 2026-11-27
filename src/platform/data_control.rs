use std::fmt::{self, Display};
use std::fs::File;
use std::io::{self, PipeReader, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Instant;

use rustix::event::{PollFd, PollFlags};
use thiserror::Error;
use wayland_client::backend::{ReadEventsGuard, WaylandError};
use wayland_client::globals::{BindError, GlobalError, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{
    ConnectError, Connection, Dispatch, DispatchError, EventQueue, Proxy, QueueHandle,
    event_created_child,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::{
    self, ExtDataControlDeviceV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::{
    self, ExtDataControlManagerV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::{
    self, ExtDataControlOfferV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::{
    self, ExtDataControlSourceV1,
};

use crate::log;
use crate::platform;
use crate::platform::contents::{self, Lost, Previous, READ_TIMEOUT, Snapshot};

#[derive(Debug, Error)]
pub(crate) enum ClipboardError {
    #[error("no Wayland compositor answers: {0}")]
    Connect(#[from] ConnectError),
    #[error("the compositor offers no ext-data-control, so the clipboard cannot be set")]
    NoDataControl,
    #[error("the compositor offers no seat")]
    NoSeat,
    #[error("the compositor ended the clipboard device")]
    Finished,
    #[error("the Wayland connection failed: {0}")]
    Wayland(#[from] WaylandError),
    #[error("the Wayland connection failed: {0}")]
    Dispatch(#[from] DispatchError),
    #[error("the Wayland connection failed: {0}")]
    Globals(#[from] GlobalError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Selection {
    Clipboard,
    Primary,
}

pub(crate) const SELECTIONS: [Selection; 2] = [Selection::Clipboard, Selection::Primary];

impl Display for Selection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Clipboard => "clipboard",
            Self::Primary => "primary selection",
        })
    }
}

/// The clipboard and the primary selection through `ext-data-control`, which needs no window.
pub(crate) struct Clipboard {
    queue: EventQueue<State>,
    state: State,
}

struct State {
    manager: ExtDataControlManagerV1,
    device: ExtDataControlDeviceV1,
    offers: [Option<ExtDataControlOfferV1>; 2],
    owned: [Option<Owned>; 2],
    reads: Vec<(String, Instant)>,
    finished: bool,
}

struct Owned {
    source: ExtDataControlSourceV1,
    contents: Arc<Snapshot>,
}

#[derive(Default)]
struct OfferedTypes(Mutex<Vec<String>>);

impl Clipboard {
    pub(crate) fn connect() -> Result<Self, ClipboardError> {
        let connection = Connection::connect_to_env()?;
        let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
        let handle = queue.handle();
        let manager: ExtDataControlManagerV1 =
            globals
                .bind(&handle, 1..=1, ())
                .map_err(|error| match error {
                    BindError::NotPresent | BindError::UnsupportedVersion => {
                        ClipboardError::NoDataControl
                    }
                })?;
        let seat: WlSeat = globals
            .bind(&handle, 1..=1, ())
            .map_err(|_| ClipboardError::NoSeat)?;
        let device = manager.get_data_device(&seat, &handle, ());
        let mut state = State {
            manager,
            device,
            offers: [None, None],
            owned: [None, None],
            reads: Vec::new(),
            finished: false,
        };
        queue.roundtrip(&mut state)?;
        Ok(Self { queue, state })
    }

    /// Dispatches what has arrived and returns what to wait on for more, or `None` when more
    /// had already arrived.
    pub(crate) fn prepare_read(&mut self) -> Result<Option<Incoming>, ClipboardError> {
        self.queue.dispatch_pending(&mut self.state)?;
        self.queue.flush()?;
        Ok(self.queue.prepare_read().map(Incoming))
    }

    pub(crate) fn dispatch(&mut self) -> Result<(), ClipboardError> {
        self.queue.dispatch_pending(&mut self.state)?;
        self.check()
    }

    /// Waits until the compositor has handled every request sent so far.
    pub(crate) fn sync(&mut self) -> Result<(), ClipboardError> {
        self.queue.roundtrip(&mut self.state)?;
        self.check()
    }

    /// The reads of offered contents since the last call, by type and time.
    pub(crate) fn take_reads(&mut self) -> Vec<(String, Instant)> {
        std::mem::take(&mut self.state.reads)
    }

    pub(crate) fn snapshot(&mut self, selection: Selection) -> Result<Previous, ClipboardError> {
        self.queue.roundtrip(&mut self.state)?;
        self.check()?;
        let index = selection as usize;
        if let Some(owned) = &self.state.owned[index] {
            return Ok(Previous::Kept {
                snapshot: owned.contents.as_ref().clone(),
                lost: Vec::new(),
            });
        }
        let Some(offer) = self.state.offers[index].clone() else {
            return Ok(Previous::Empty);
        };
        let offered = offer
            .data::<OfferedTypes>()
            .map_or_else(Vec::new, |types| lock(&types.0).clone());
        Ok(contents::keep(&offered, |mime, limit| {
            self.receive(&offer, mime, limit)
        }))
    }

    pub(crate) fn offer(
        &mut self,
        selection: Selection,
        contents: Arc<Snapshot>,
    ) -> Result<(), ClipboardError> {
        self.set(selection, Some(contents))
    }

    pub(crate) fn clear(&mut self, selection: Selection) -> Result<(), ClipboardError> {
        self.set(selection, None)
    }

    /// Whether the selection still holds these contents, which it no longer does once another
    /// client has set it.
    pub(crate) fn offers(&self, selection: Selection, contents: &Arc<Snapshot>) -> bool {
        self.state.owned[selection as usize]
            .as_ref()
            .is_some_and(|owned| Arc::ptr_eq(&owned.contents, contents))
    }

    fn set(
        &mut self,
        selection: Selection,
        contents: Option<Arc<Snapshot>>,
    ) -> Result<(), ClipboardError> {
        self.check()?;
        let index = selection as usize;
        let owned = contents.map(|contents| {
            let source = self
                .state
                .manager
                .create_data_source(&self.queue.handle(), Arc::clone(&contents));
            for (mime, _) in &contents.contents {
                source.offer(mime.clone());
            }
            Owned { source, contents }
        });
        let source = owned.as_ref().map(|owned| &owned.source);
        match selection {
            Selection::Clipboard => self.state.device.set_selection(source),
            Selection::Primary => self.state.device.set_primary_selection(source),
        }
        let replaced = std::mem::replace(&mut self.state.owned[index], owned);
        if let Some(replaced) = replaced {
            replaced.source.destroy();
        }
        self.queue.flush()?;
        Ok(())
    }

    fn receive(
        &mut self,
        offer: &ExtDataControlOfferV1,
        mime: &str,
        limit: usize,
    ) -> Result<Vec<u8>, Lost> {
        let (reader, writer) = io::pipe().map_err(unreadable)?;
        offer.receive(mime.to_owned(), writer.as_fd());
        self.queue.flush().map_err(unreadable)?;
        drop(writer);
        read_within(reader, limit, Instant::now() + READ_TIMEOUT)
    }

    fn check(&self) -> Result<(), ClipboardError> {
        if self.state.finished {
            return Err(ClipboardError::Finished);
        }
        Ok(())
    }
}

/// What the compositor sends next, to wait for beside other descriptors.
pub(crate) struct Incoming(ReadEventsGuard);

impl Incoming {
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.0.connection_fd()
    }

    /// Reads what has arrived; call [`Clipboard::dispatch`] after it.
    pub(crate) fn read(self) -> Result<(), ClipboardError> {
        match self.0.read() {
            Ok(_) => Ok(()),
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// Reads to the end, unless the data outgrows the limit or the deadline passes first.
fn read_within(mut reader: PipeReader, limit: usize, deadline: Instant) -> Result<Vec<u8>, Lost> {
    let mut data = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        if Instant::now() >= deadline {
            return Err(Lost::NoAnswer);
        }
        if !platform::poll_until(&mut [PollFd::new(&reader, PollFlags::IN)], Some(deadline))
            .map_err(unreadable)?
        {
            continue;
        }
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(data),
            Ok(read) if data.len() + read > limit => return Err(Lost::TooBig),
            Ok(read) => data.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(unreadable(error)),
        }
    }
}

fn unreadable(error: impl Display) -> Lost {
    Lost::Unreadable(error.to_string())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn serve(contents: Arc<Snapshot>, mime: String, fd: OwnedFd) {
    thread::spawn(move || {
        let Some(data) = contents.data(&mime) else {
            return;
        };
        if let Err(error) = File::from(fd).write_all(data)
            && error.kind() != io::ErrorKind::BrokenPipe
        {
            log::eprint(format_args!(
                "cannot hand the clipboard's {mime} over: {error}"
            ));
        }
    });
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wl_seat::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: ext_data_control_manager_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let (selection, offer) = match event {
            ext_data_control_device_v1::Event::Selection { id } => (Selection::Clipboard, id),
            ext_data_control_device_v1::Event::PrimarySelection { id } => (Selection::Primary, id),
            ext_data_control_device_v1::Event::Finished => {
                state.finished = true;
                return;
            }
            _ => return,
        };
        if let Some(previous) = std::mem::replace(&mut state.offers[selection as usize], offer)
            && !SELECTIONS
                .iter()
                .any(|other| state.offers[*other as usize].as_ref() == Some(&previous))
        {
            previous.destroy();
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, OfferedTypes::default()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, OfferedTypes> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        types: &OfferedTypes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            lock(&types.0).push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, Arc<Snapshot>> for State {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        contents: &Arc<Snapshot>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.reads.push((mime_type.clone(), Instant::now()));
                serve(Arc::clone(contents), mime_type, fd);
            }
            ext_data_control_source_v1::Event::Cancelled => {
                for owned in &mut state.owned {
                    if owned.as_ref().is_some_and(|owned| owned.source == *source) {
                        *owned = None;
                    }
                }
                source.destroy();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::read_within;
    use crate::platform::contents::Lost;

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    #[test]
    fn data_of_exactly_the_limit_is_read_to_the_end() {
        let (reader, mut writer) = io::pipe().unwrap();
        let writing = thread::spawn(move || {
            for _ in 0..2 {
                writer.write_all(&[b'x'; 5]).unwrap();
                thread::sleep(Duration::from_millis(10));
            }
        });

        assert_eq!(read_within(reader, 10, soon()), Ok(vec![b'x'; 10]));
        writing.join().unwrap();
    }

    #[test]
    fn data_over_the_limit_is_too_big() {
        let (reader, mut writer) = io::pipe().unwrap();
        writer.write_all(&[b'x'; 11]).unwrap();
        drop(writer);

        assert_eq!(read_within(reader, 10, soon()), Err(Lost::TooBig));
    }

    #[test]
    fn a_writer_that_sends_nothing_goes_unanswered_at_the_deadline() {
        let (reader, _writer) = io::pipe().unwrap();
        let started = Instant::now();

        assert_eq!(
            read_within(reader, 10, started + Duration::from_millis(50)),
            Err(Lost::NoAnswer)
        );
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(50) && waited < Duration::from_secs(2),
            "{waited:?}"
        );
    }
}
