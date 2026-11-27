use std::fs::File;
use std::io::{self, PipeReader, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
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
use crate::platform::clipboard::{self, Snapshot};

const READ_TIMEOUT: Duration = Duration::from_millis(500);

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

const SELECTIONS: [Selection; 2] = [Selection::Clipboard, Selection::Primary];

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

    /// Dispatches what has arrived and returns the guard to wait on for more, or `None` when
    /// more had already arrived.
    pub(crate) fn prepare_read(&mut self) -> Result<Option<ReadEventsGuard>, ClipboardError> {
        self.queue.dispatch_pending(&mut self.state)?;
        self.queue.flush()?;
        Ok(self.queue.prepare_read())
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

    pub(crate) fn snapshot(&mut self, selection: Selection) -> Result<Snapshot, ClipboardError> {
        self.queue.roundtrip(&mut self.state)?;
        self.check()?;
        let index = selection as usize;
        if let Some(owned) = &self.state.owned[index] {
            return Ok(owned.contents.as_ref().clone());
        }
        let Some(offer) = self.state.offers[index].clone() else {
            return Ok(Snapshot::default());
        };
        let offered = offer
            .data::<OfferedTypes>()
            .map_or_else(Vec::new, |types| lock(&types.0).clone());
        Ok(Snapshot::take(
            &clipboard::kept_types(&offered),
            |mime, limit| self.receive(&offer, mime, limit),
        ))
    }

    /// Offers the contents as the selection, or clears it when there are none.
    pub(crate) fn set(
        &mut self,
        selection: Selection,
        contents: Arc<Snapshot>,
    ) -> Result<(), ClipboardError> {
        self.check()?;
        let index = selection as usize;
        let source = if contents.is_empty() {
            None
        } else {
            let source = self
                .state
                .manager
                .create_data_source(&self.queue.handle(), Arc::clone(&contents));
            for (mime, _) in &contents.contents {
                source.offer(mime.clone());
            }
            Some(source)
        };
        match selection {
            Selection::Clipboard => self.state.device.set_selection(source.as_ref()),
            Selection::Primary => self.state.device.set_primary_selection(source.as_ref()),
        }
        let replaced = std::mem::replace(
            &mut self.state.owned[index],
            source.map(|source| Owned { source, contents }),
        );
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
    ) -> Option<Vec<u8>> {
        let (reader, writer) = io::pipe().ok()?;
        offer.receive(mime.to_owned(), writer.as_fd());
        self.queue.flush().ok()?;
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

/// Reads to the end, or nothing when the data outgrows the limit or the deadline passes.
fn read_within(mut reader: PipeReader, limit: usize, deadline: Instant) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        let timeout = Timespec::try_from(left).ok()?;
        let mut fds = [PollFd::new(&reader, PollFlags::IN)];
        if poll(&mut fds, Some(&timeout)).ok()? == 0 {
            return None;
        }
        match reader.read(&mut chunk) {
            Ok(0) => return Some(data),
            Ok(read) => {
                if data.len() + read > limit {
                    return None;
                }
                data.extend_from_slice(&chunk[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
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
