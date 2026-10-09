//! The `wl_data_device` listener: binds a data device per seat on winit's connection and turns
//! drags of files over the app's window into [`Drops`].
//!
//! The thread serves a private event queue with libwayland's multi-reader protocol
//! (`prepare_read` + `poll` + `read`), alongside winit reading its own queue, and wakes every
//! [`TICK`] to see whether the listener was dropped. Protocol rules it keeps (a protocol error
//! would end winit's connection too): `set_actions` only on a drag offer before its drop, a
//! valid action pair, and `finish` only on a dropped offer (version 3+) with a copy action.

use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawWindowHandle};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, event_created_child};

use crate::Drops;
use crate::collect::{Collect, Step};
use crate::uri_list;

/// How often the thread checks whether to stop (and a drop's read whether it timed out).
const TICK: Duration = Duration::from_millis(100);
/// `wl_data_device_manager` 3 adds drag actions and `finish`.
const MANAGER_VERSION: u32 = 3;
/// MIME types an offer may list before the rest are ignored.
const MAX_MIME_TYPES: usize = 256;

/// Listens for file drags onto one Wayland window until dropped.
pub struct Listener {
    shared: Arc<Mutex<Drops>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    /// Listen for file drags onto `window` when it's a native Wayland window (`Ok(None)` on X11,
    /// Xwayland included). `wake` is called from the listener thread whenever [`Self::poll`] has
    /// something new (it should request a repaint).
    pub fn start(window: &(impl HasDisplayHandle + HasWindowHandle), wake: impl Fn() + Send + 'static) -> Result<Option<Self>, String> {
        let RawWindowHandle::Wayland(h) = window.window_handle().map_err(|e| e.to_string())?.as_raw() else { return Ok(None) };
        let surface = h.surface.as_ptr() as usize;
        let Some(conn) = crate::foreign::connection(window.display_handle().map_err(|e| e.to_string())?) else { return Ok(None) };
        let (globals, queue) = registry_queue_init::<State>(&conn).map_err(|e| format!("couldn't list the Wayland globals: {e}"))?;
        let qh = queue.handle();
        let manager =
            globals.bind::<WlDataDeviceManager, _, _>(&qh, 1..=MANAGER_VERSION, ()).map_err(|e| format!("no drag-and-drop on this compositor: {e}"))?;
        let shared = Arc::new(Mutex::new(Drops::default()));
        let mut state = State { surface, manager, devices: Vec::new(), drag: None, reading: None, shared: shared.clone(), wake: Box::new(wake) };
        let seats = globals.contents().with_list(|list| list.iter().filter(|g| g.interface == WlSeat::interface().name).map(|g| g.name).collect::<Vec<_>>());
        for name in seats {
            state.add_seat(globals.registry(), name, &qh);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("wayland-dnd".into())
            .spawn({
                let stop = stop.clone();
                move || run(conn, queue, state, &stop)
            })
            .map_err(|e| format!("couldn't start the drag-and-drop thread: {e}"))?;
        Ok(Some(Self { shared, stop, thread: Some(thread) }))
    }

    /// The drag state now; the dropped files are taken (each is returned once).
    pub fn poll(&self) -> Drops {
        let mut d = lock(&self.shared);
        Drops { hovering: d.hovering, position: d.position, dropped: std::mem::take(&mut d.dropped) }
    }

    /// The pointer during the last drag over the window, in surface-local logical pixels.
    pub fn position(&self) -> Option<(f64, f64)> {
        lock(&self.shared).position
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            log::warn!("the drag-and-drop thread panicked");
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Serve the queue until `stop` (or the connection fails).
fn run(_conn: Connection, mut queue: EventQueue<State>, mut state: State, stop: &AtomicBool) {
    let tick = Timespec { tv_sec: 0, tv_nsec: i64::try_from(TICK.as_nanos()).unwrap_or(100_000_000) };
    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = queue.dispatch_pending(&mut state) {
            log::warn!("Wayland drag-and-drop stopped: {e}");
            break;
        }
        if let Err(e) = queue.flush()
            && !would_block(&e)
        {
            log::warn!("Wayland drag-and-drop stopped: {e}");
            break;
        }
        // `None`: events are already queued; dispatch them first.
        let Some(guard) = queue.prepare_read() else { continue };
        let (wayland, pipe) = {
            let conn_fd = guard.connection_fd();
            let mut fds = vec![PollFd::new(&conn_fd, PollFlags::IN)];
            if let Some(r) = &state.reading {
                fds.push(PollFd::new(&r.pipe, PollFlags::IN));
            }
            if let Err(e) = poll(&mut fds, Some(&tick))
                && e != rustix::io::Errno::INTR
            {
                log::warn!("Wayland drag-and-drop stopped: poll failed: {e}");
                break;
            }
            let ready = |f: Option<&PollFd<'_>>| f.is_some_and(|f| !f.revents().is_empty());
            (ready(fds.first()), ready(fds.get(1)))
        };
        if wayland {
            if let Err(e) = guard.read()
                && !would_block(&e)
            {
                log::warn!("Wayland drag-and-drop stopped: {e}");
                break;
            }
        } else {
            drop(guard);
        }
        state.continue_reading(pipe);
    }
    state.shutdown();
    let _ = queue.flush();
}

fn would_block(e: &WaylandError) -> bool {
    matches!(e, WaylandError::Io(io) if io.kind() == ErrorKind::WouldBlock)
}

/// What an offer announced about itself.
#[derive(Default)]
struct OfferInfo {
    mime_types: Vec<String>,
    /// The action the compositor chose (version 3+).
    action: Option<DndAction>,
}

type OfferData = Mutex<OfferInfo>;

fn offer_info(offer: &WlDataOffer) -> Option<MutexGuard<'_, OfferInfo>> {
    offer.data::<OfferData>().map(lock)
}

/// A drop whose URI list is being read.
struct Reading {
    offer: WlDataOffer,
    pipe: std::io::PipeReader,
    collect: Collect,
}

struct State {
    /// winit's `wl_surface*` for the app's window: drags over other surfaces aren't ours.
    surface: usize,
    manager: WlDataDeviceManager,
    /// Per seat: its global name, our binding of it and its data device.
    devices: Vec<(u32, WlSeat, WlDataDevice)>,
    /// The offer of the drag over one of our surfaces, and whether it is taken (files, over the
    /// window).
    drag: Option<(WlDataOffer, bool)>,
    reading: Option<Reading>,
    shared: Arc<Mutex<Drops>>,
    wake: Box<dyn Fn() + Send>,
}

impl State {
    fn add_seat(&mut self, registry: &WlRegistry, name: u32, qh: &QueueHandle<Self>) {
        // Only the data device is needed from the seat: version 1 is enough.
        let seat = registry.bind::<WlSeat, _, _>(name, 1, qh, ());
        let device = self.manager.get_data_device(&seat, qh, ());
        self.devices.push((name, seat, device));
    }

    fn remove_seat(&mut self, name: u32) {
        self.devices.retain(|(n, _, device)| {
            if *n == name {
                release(device);
            }
            *n != name
        });
    }

    fn update(&self, f: impl FnOnce(&mut Drops)) {
        f(&mut lock(&self.shared));
        (self.wake)();
    }

    fn enter(&mut self, serial: u32, surface: &WlSurface, x: f64, y: f64, offer: Option<WlDataOffer>) {
        self.end_drag();
        let Some(offer) = offer else { return };
        let ours = surface.id().as_ptr() as usize == self.surface;
        let files = ours && offer_info(&offer).is_some_and(|i| i.mime_types.iter().any(|m| m == uri_list::MIME));
        if files {
            offer.accept(serial, Some(uri_list::MIME.into()));
            if offer.version() >= 3 {
                offer.set_actions(DndAction::Copy, DndAction::Copy);
            }
            self.update(|d| {
                d.hovering = true;
                d.position = Some((x, y));
            });
        } else {
            offer.accept(serial, None);
        }
        self.drag = Some((offer, files));
    }

    fn motion(&mut self, x: f64, y: f64) {
        if matches!(self.drag, Some((_, true))) {
            self.update(|d| d.position = Some((x, y)));
        }
    }

    /// The drag left (or ended without a drop on us).
    fn end_drag(&mut self) {
        if let Some((offer, taken)) = self.drag.take() {
            offer.destroy();
            if taken {
                self.update(|d| d.hovering = false);
            }
        }
    }

    fn dropped(&mut self) {
        let Some((offer, true)) = self.drag.take() else { return self.end_drag() };
        self.update(|d| d.hovering = false);
        let (pipe, writer) = match std::io::pipe() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("couldn't receive the dropped files: {e}");
                return offer.destroy();
            }
        };
        // libwayland duplicates the fd into the request; ours closes now, so the read sees the
        // end once the source closes its copy.
        offer.receive(uri_list::MIME.into(), writer.as_fd());
        drop(writer);
        if let Some(old) = self.reading.replace(Reading { offer, pipe, collect: Collect::new(Instant::now()) }) {
            old.offer.destroy();
        }
    }

    /// Read more of the dropped URI list (`readable`: poll says the pipe has data or closed).
    fn continue_reading(&mut self, readable: bool) {
        let Some(r) = self.reading.as_mut() else { return };
        let step = if readable { r.collect.read_from(&mut r.pipe) } else { r.collect.check_deadline(Instant::now()) };
        let data = match step {
            Step::Pending => return,
            Step::Done(data) => data,
            Step::Failed(e) => {
                log::warn!("{e}");
                Vec::new()
            }
        };
        let Some(r) = self.reading.take() else { return };
        let paths = uri_list::file_paths(&data);
        if !paths.is_empty() {
            let copy = offer_info(&r.offer).is_some_and(|i| i.action.is_some_and(|a| a.contains(DndAction::Copy)));
            if r.offer.version() >= 3 && copy {
                r.offer.finish();
            }
            self.update(|d| d.dropped.extend(paths));
        }
        r.offer.destroy();
    }

    fn shutdown(&mut self) {
        self.end_drag();
        if let Some(r) = self.reading.take() {
            r.offer.destroy();
        }
        for (_, _, device) in self.devices.drain(..) {
            release(&device);
        }
    }
}

/// Destroy our data device (`release` is version 2+; version 1 has no destructor).
fn release(device: &WlDataDevice) {
    if device.version() >= 2 {
        device.release();
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(state: &mut Self, registry: &WlRegistry, event: wl_registry::Event, _: &GlobalListContents, _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            wl_registry::Event::Global { name, interface, .. } if interface == WlSeat::interface().name => state.add_seat(registry, name, qh),
            wl_registry::Event::GlobalRemove { name } => state.remove_seat(name),
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(_: &mut Self, _: &WlDataDeviceManager, _: <WlDataDeviceManager as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(state: &mut Self, _: &WlDataDevice, event: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_device::Event::Enter { serial, surface, x, y, id } => state.enter(serial, &surface, x, y, id),
            wl_data_device::Event::Motion { x, y, .. } => state.motion(x, y),
            wl_data_device::Event::Leave => state.end_drag(),
            wl_data_device::Event::Drop => state.dropped(),
            // The clipboard isn't ours to read (egui's clipboard has its own device).
            wl_data_device::Event::Selection { id: Some(offer) } => offer.destroy(),
            _ => {}
        }
    }

    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, OfferData::default()),
    ]);
}

impl Dispatch<WlDataOffer, OfferData> for State {
    fn event(_: &mut Self, _: &WlDataOffer, event: wl_data_offer::Event, data: &OfferData, _: &Connection, _: &QueueHandle<Self>) {
        let mut info = lock(data);
        match event {
            wl_data_offer::Event::Offer { mime_type } if info.mime_types.len() < MAX_MIME_TYPES => info.mime_types.push(mime_type),
            wl_data_offer::Event::Action { dnd_action: WEnum::Value(a) } => info.action = Some(a),
            _ => {}
        }
    }
}
