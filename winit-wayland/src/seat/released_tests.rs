//! Events queued for a seat device that the seat has just released (rinch#2656).
//!
//! When a seat loses a capability, `remove_capability` releases that device's proxy. Events the
//! compositor wrote for the device before it saw the release can already sit in the client's
//! queue, read in the same batch as the `wl_seat.capabilities` event; wayland-client still
//! dispatches them, and a dead proxy answers no user data. winit used to `expect` that data and
//! panicked ("failed to get pointer data.").
//!
//! These tests run the real `WinitState` against a stand-in compositor (wayland-server, in
//! process, over a socket pair) that writes the capability change and then the device's last
//! events in one flush, and reads them into the queue before dispatching any of them.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use sctk::reexports::calloop::EventLoop;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::{Connection, EventQueue};
use wayland_protocols::xdg::shell::server::xdg_wm_base::{self, XdgWmBase};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use wayland_server::protocol::wl_buffer::WlBuffer;
use wayland_server::protocol::wl_callback::WlCallback;
use wayland_server::protocol::wl_compositor::{self, WlCompositor};
use wayland_server::protocol::wl_keyboard::WlKeyboard;
use wayland_server::protocol::wl_pointer::WlPointer;
use wayland_server::protocol::wl_region::WlRegion;
use wayland_server::protocol::wl_seat::{self, Capability, WlSeat};
use wayland_server::protocol::wl_shm::{self, WlShm};
use wayland_server::protocol::wl_shm_pool::{self, WlShmPool};
use wayland_server::protocol::wl_surface::{self, WlSurface};
use wayland_server::protocol::wl_touch::WlTouch;
use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New};

use crate::state::WinitState;

/// The stand-in compositor's state: the resources a test sends events through.
struct Server {
    capabilities: Capability,
    seat: Option<WlSeat>,
    surface: Option<WlSurface>,
    pointer: Option<WlPointer>,
    touch: Option<WlTouch>,
}

type Job = Box<dyn FnOnce(&mut Server) + Send>;

/// A job and the channel that says it was sent, or `None` to stop the compositor.
type Message = Option<(Job, Sender<()>)>;

/// A compositor on its own thread, so the client side can make blocking roundtrips.
struct StandIn {
    jobs: Sender<Message>,
    thread: Option<JoinHandle<()>>,
}

impl StandIn {
    fn start(capabilities: Capability, socket: UnixStream) -> Self {
        let (jobs, rx): (_, Receiver<Message>) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut display = Display::<Server>::new().unwrap();
            let dh = display.handle();
            dh.create_global::<Server, WlCompositor, ()>(6, ());
            dh.create_global::<Server, WlShm, ()>(1, ());
            dh.create_global::<Server, XdgWmBase, ()>(1, ());
            dh.create_global::<Server, WlSeat, ()>(7, ());
            display.handle().insert_client(socket, Arc::new(NoClientData)).unwrap();

            let mut server =
                Server { capabilities, seat: None, surface: None, pointer: None, touch: None };
            loop {
                display.dispatch_clients(&mut server).unwrap();
                display.flush_clients().unwrap();
                match rx.recv_timeout(Duration::from_millis(1)) {
                    Ok(Some((job, done))) => {
                        // Everything the job sends goes out in this one flush.
                        job(&mut server);
                        display.flush_clients().unwrap();
                        done.send(()).unwrap();
                    },
                    Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                }
            }
        });
        Self { jobs, thread: Some(thread) }
    }

    /// Run `job` on the compositor and wait until what it sent has been written.
    fn send(&self, job: impl FnOnce(&mut Server) + Send + 'static) {
        let (done, wait) = mpsc::channel();
        self.jobs.send(Some((Box::new(job), done))).unwrap();
        wait.recv().unwrap();
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        let _ = self.jobs.send(None);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct NoClientData;

impl ClientData for NoClientData {
    fn initialized(&self, _: ClientId) {}

    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

/// A `WinitState` connected to a fresh stand-in compositor whose seat has `capabilities`.
fn connect(capabilities: Capability) -> (StandIn, EventQueue<WinitState>, WinitState) {
    let (client_socket, server_socket) = UnixStream::pair().unwrap();
    let stand_in = StandIn::start(capabilities, server_socket);

    let connection = Connection::from_socket(client_socket).unwrap();
    let (globals, mut queue) = registry_queue_init::<WinitState>(&connection).unwrap();
    let event_loop: &'static EventLoop<'static, WinitState> =
        Box::leak(Box::new(EventLoop::try_new().unwrap()));
    let mut state = WinitState::new(&globals, &queue.handle(), event_loop.handle()).unwrap();

    // The seat's capabilities arrive, winit asks for the devices, and the compositor makes them.
    for _ in 0..3 {
        queue.roundtrip(&mut state).unwrap();
    }

    (stand_in, queue, state)
}

/// Read everything the compositor has written into the queue, then dispatch it: the
/// capability change and the device events behind it are in the queue together.
fn read_then_dispatch(queue: &mut EventQueue<WinitState>, state: &mut WinitState) {
    queue.flush().unwrap();
    let guard = queue.prepare_read().expect("the queue holds no events yet");
    guard.read().unwrap();
    queue.dispatch_pending(state).unwrap();
}

#[test]
fn pointer_events_queued_behind_the_pointers_removal_are_dropped() {
    let (stand_in, mut queue, mut state) = connect(Capability::Pointer);
    assert!(
        state.seats.values().any(|seat| seat.pointer.is_some()),
        "the seat's pointer was never made"
    );

    // The device goes away: the compositor takes the capability, then writes the pointer's
    // last events, which it sends until it sees the client's release.
    stand_in.send(|server| {
        let surface = server.surface.clone().expect("winit made no cursor surface");
        let pointer = server.pointer.clone().expect("winit asked for no pointer");
        server.seat.as_ref().unwrap().capabilities(Capability::empty());
        pointer.leave(1, &surface);
        pointer.frame();
    });

    // Before the fix this panicked in `pointer_frame`: "failed to get pointer data."
    read_then_dispatch(&mut queue, &mut state);

    assert!(state.seats.values().all(|seat| seat.pointer.is_none()), "the pointer was kept");
    assert!(state.pointer_surfaces.is_empty(), "the released pointer's cursor surface was kept");
}

#[test]
fn touch_events_queued_behind_the_touchs_removal_are_dropped() {
    let (stand_in, mut queue, mut state) = connect(Capability::Touch);
    assert!(
        state.seats.values().any(|seat| seat.touch.is_some()),
        "the seat's touch was never made"
    );

    stand_in.send(|server| {
        let touch = server.touch.clone().expect("winit asked for no touch");
        server.seat.as_ref().unwrap().capabilities(Capability::empty());
        // No `down`: the stand-in has no surface without a pointer. Each of these reaches its
        // handler, which read the seat from the dead proxy first.
        touch.motion(0, 0, 2.0, 2.0);
        touch.up(2, 0, 0);
        touch.cancel();
        touch.frame();
    });

    // Before the fix this panicked in the touch handlers: "failed to get touch data."
    read_then_dispatch(&mut queue, &mut state);

    assert!(state.seats.values().all(|seat| seat.touch.is_none()), "the touch was kept");
}

// The stand-in compositor's protocol: just enough for `WinitState::new` and a seat.

impl GlobalDispatch<WlCompositor, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlCompositor>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<WlCompositor, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlCompositor,
        request: wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                state.surface = Some(init.init(id, ()));
            },
            wl_compositor::Request::CreateRegion { id } => {
                init.init(id, ());
            },
            _ => (),
        }
    }
}

impl Dispatch<WlSurface, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlSurface,
        request: wl_surface::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_surface::Request::Frame { callback } = request {
            init.init(callback, ());
        }
    }
}

impl Dispatch<WlRegion, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlRegion,
        _: <WlRegion as wayland_server::Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<WlCallback, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlCallback,
        _: <WlCallback as wayland_server::Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<WlShm, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlShm>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let shm = init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl Dispatch<WlShm, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlShm,
        request: wl_shm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, .. } = request {
            init.init(id, ());
        }
    }
}

impl Dispatch<WlShmPool, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlShmPool,
        request: wl_shm_pool::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm_pool::Request::CreateBuffer { id, .. } = request {
            init.init(id, ());
        }
    }
}

impl Dispatch<WlBuffer, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlBuffer,
        _: <WlBuffer as wayland_server::Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<XdgWmBase, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<XdgWmBase>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<XdgWmBase, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &XdgWmBase,
        request: xdg_wm_base::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        assert!(
            matches!(request, xdg_wm_base::Request::Pong { .. } | xdg_wm_base::Request::Destroy),
            "the stand-in compositor makes no xdg surfaces"
        );
    }
}

impl GlobalDispatch<WlSeat, ()> for Server {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlSeat>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let seat = init.init(resource, ());
        seat.name("seat0".into());
        seat.capabilities(state.capabilities);
        state.seat = Some(seat);
    }
}

impl Dispatch<WlSeat, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlSeat,
        request: wl_seat::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetPointer { id } => state.pointer = Some(init.init(id, ())),
            wl_seat::Request::GetTouch { id } => state.touch = Some(init.init(id, ())),
            wl_seat::Request::GetKeyboard { id } => {
                init.init(id, ());
            },
            _ => (),
        }
    }
}

impl Dispatch<WlPointer, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlPointer,
        _: <WlPointer as wayland_server::Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<WlTouch, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlTouch,
        _: <WlTouch as wayland_server::Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<WlKeyboard, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlKeyboard,
        _: <WlKeyboard as wayland_server::Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
