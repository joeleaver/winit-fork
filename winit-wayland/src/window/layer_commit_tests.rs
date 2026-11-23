//! `Window::update_layer_shell_and_size` (rinch patch, rinch#2464, rinch#2829): a layer
//! surface's changes and its size request reach the compositor in **one** commit.
//!
//! A real layer-shell window runs against a stand-in compositor (wayland-server, in process,
//! over a socket pair, on its own thread) that records the layer surface's requests and its
//! surface's commits. Each case reads what the compositor got for one call:
//!
//! - changes with a size request that goes out: the changes and the `set_size` in one commit;
//! - changes with no size: the changes and the surface's current size in one commit;
//! - changes with a size request held behind one in flight: the changes go out with the held
//!   (newest) size, in one commit, and the size is not applied.
//!
//! An update re-sends the surface's anchor, margins and zone whatever changed
//! (`LayerShellAttributes::apply`), so every call here is one with changes.
//!
//! The event loop is never run: the test drives the window's queue with roundtrips, as
//! `finish_window_setup` does while it waits for the first configure.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use dpi::{LogicalSize, PhysicalSize};
use sctk::reexports::client::Connection;
use wayland_protocols::xdg::shell::server::xdg_wm_base::XdgWmBase;
use wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_shell_v1::{self, ZwlrLayerShellV1};
use wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_surface_v1::{
    self, ZwlrLayerSurfaceV1,
};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use wayland_server::protocol::wl_buffer::WlBuffer;
use wayland_server::protocol::wl_callback::WlCallback;
use wayland_server::protocol::wl_compositor::{self, WlCompositor};
use wayland_server::protocol::wl_region::WlRegion;
use wayland_server::protocol::wl_shm::{self, WlShm};
use wayland_server::protocol::wl_shm_pool::{self, WlShmPool};
use wayland_server::protocol::wl_surface::{self, WlSurface};
use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New};
use winit_core::window::{Window as _, WindowAttributes};

use super::Window;
use super::layer_shell::{LayerShellAnchor, LayerShellAttributes, LayerShellMargins};
use crate::WindowAttributesWayland;
use crate::event_loop::EventLoop;

/// What the compositor got from the client, in order: the layer surface's state requests and
/// its surface's commits.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Got {
    Size(u32, u32),
    Margin(i32, i32, i32, i32),
    Anchor,
    Zone(i32),
    Commit,
}

#[derive(Default)]
struct Server {
    got: Vec<Got>,
    layer: Option<ZwlrLayerSurfaceV1>,
    layer_surface: Option<WlSurface>,
    configured: bool,
    last_size: (u32, u32),
}

type Job = Box<dyn FnOnce(&mut Server) + Send>;
type Message = Option<(Job, Sender<()>)>;

struct StandIn {
    jobs: Sender<Message>,
    thread: Option<JoinHandle<()>>,
}

impl StandIn {
    fn start(socket: UnixStream) -> Self {
        let (jobs, rx): (_, Receiver<Message>) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut display = Display::<Server>::new().unwrap();
            let dh = display.handle();
            dh.create_global::<Server, WlCompositor, ()>(6, ());
            dh.create_global::<Server, WlShm, ()>(1, ());
            dh.create_global::<Server, XdgWmBase, ()>(1, ());
            dh.create_global::<Server, ZwlrLayerShellV1, ()>(4, ());
            display.handle().insert_client(socket, Arc::new(NoClientData)).unwrap();
            let mut server = Server::default();
            loop {
                display.dispatch_clients(&mut server).unwrap();
                display.flush_clients().unwrap();
                match rx.recv_timeout(Duration::from_millis(1)) {
                    Ok(Some((job, done))) => {
                        // Whatever the client flushed before asking is read first.
                        display.dispatch_clients(&mut server).unwrap();
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

    fn send(&self, job: impl FnOnce(&mut Server) + Send + 'static) {
        let (done, wait) = mpsc::channel();
        self.jobs.send(Some((Box::new(job), done))).unwrap();
        wait.recv().unwrap();
    }

    /// What the compositor has got since the last call, once `connection`'s requests are in.
    fn take(&self, connection: &Connection) -> Vec<Got> {
        connection.flush().unwrap();
        let (tx, rx) = mpsc::channel();
        self.send(move |server| tx.send(std::mem::take(&mut server.got)).unwrap());
        rx.recv().unwrap()
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

/// A 200x40 layer surface anchored to the top-left corner, configured, against a fresh
/// stand-in; what the compositor got while it was made is dropped.
fn layer_window() -> (StandIn, Connection, EventLoop, Window) {
    let (client_socket, server_socket) = UnixStream::pair().unwrap();
    let stand_in = StandIn::start(server_socket);
    let connection = Connection::from_socket(client_socket).unwrap();
    let event_loop = EventLoop::with_connection(connection.clone()).unwrap();
    let layer = LayerShellAttributes {
        anchor: LayerShellAnchor { top: true, left: true, ..Default::default() },
        namespace: "rinch-2829".into(),
        ..Default::default()
    };
    let attributes = WindowAttributes::default()
        .with_surface_size(LogicalSize::new(200u32, 40u32))
        .with_platform_attributes(Box::new(
            WindowAttributesWayland::default().with_layer_shell(layer),
        ));
    let window = Window::new(event_loop.active(), attributes).expect("a layer surface");
    stand_in.take(&connection);
    (stand_in, connection, event_loop, window)
}

fn margin_left(left: i32) -> LayerShellMargins {
    LayerShellMargins { left, ..Default::default() }
}

/// Exactly one commit, the last thing sent, with `before` (in any order) ahead of it, and no
/// `set_size` but the ones in `before`.
fn one_commit(got: &[Got], before: &[Got]) {
    assert_eq!(got.iter().filter(|g| **g == Got::Commit).count(), 1, "one commit: {got:?}");
    assert_eq!(got.last(), Some(&Got::Commit), "the commit comes last: {got:?}");
    for want in before {
        assert!(got.contains(want), "{want:?} goes in the commit: {got:?}");
    }
    let sizes = |list: &[Got]| list.iter().filter(|g| matches!(g, Got::Size(..))).count();
    assert_eq!(sizes(got), sizes(before), "no other set_size: {got:?}");
}

#[test]
fn changes_and_a_size_that_goes_out_are_one_commit() {
    let (stand_in, connection, _event_loop, window) = layer_window();
    let answer = window.update_layer_shell_and_size(
        |a| a.margins = margin_left(50),
        Some(LogicalSize::new(300u32, 40u32).into()),
    );
    one_commit(&stand_in.take(&connection), &[
        Got::Anchor,
        Got::Margin(0, 0, 0, 50),
        Got::Zone(0),
        Got::Size(300, 40),
    ]);
    assert_eq!(answer, Some(PhysicalSize::new(300, 40)), "the size is applied at once");
}

#[test]
fn changes_with_no_size_go_out_with_the_current_size() {
    let (stand_in, connection, _event_loop, window) = layer_window();
    let answer = window.update_layer_shell_and_size(|a| a.exclusive_zone = 40, None);
    one_commit(&stand_in.take(&connection), &[
        Got::Anchor,
        Got::Margin(0, 0, 0, 0),
        Got::Zone(40),
        Got::Size(200, 40),
    ]);
    assert_eq!(answer, None);
}

#[test]
fn a_size_held_behind_one_in_flight_goes_out_with_the_changes_unapplied() {
    let (stand_in, connection, _event_loop, window) = layer_window();
    // A request goes out; its marker is not answered (the queue is not dispatched), so it
    // stays in flight.
    window.update_layer_shell_and_size(|_| (), Some(LogicalSize::new(300u32, 40u32).into()));
    one_commit(&stand_in.take(&connection), &[Got::Size(300, 40)]);

    // Held: no size request of its own, but the changes go out now, with the newest
    // request's size, not the applied one.
    let answer = window.update_layer_shell_and_size(
        |a| a.margins = margin_left(20),
        Some(LogicalSize::new(400u32, 40u32).into()),
    );
    one_commit(&stand_in.take(&connection), &[Got::Margin(0, 0, 0, 20), Got::Size(400, 40)]);
    assert_eq!(answer, None, "a held request is not applied");
    assert_eq!(window.surface_size(), PhysicalSize::new(300, 40), "the size in flight stays");
}

// The stand-in compositor's protocol: just enough for `WinitState::new` and a layer surface.

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
        _: &mut Self,
        _: &Client,
        _: &WlCompositor,
        request: wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                init.init(id, ());
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
        state: &mut Self,
        _: &Client,
        surface: &WlSurface,
        request: wl_surface::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Frame { callback } => {
                init.init(callback, ());
            },
            wl_surface::Request::Commit if state.layer_surface.as_ref() == Some(surface) => {
                state.got.push(Got::Commit);
                if !state.configured {
                    state.configured = true;
                    let (width, height) = state.last_size;
                    state.layer.as_ref().unwrap().configure(1, width, height);
                }
            },
            _ => (),
        }
    }
}

impl GlobalDispatch<ZwlrLayerShellV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwlrLayerShellV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrLayerShellV1,
        request: zwlr_layer_shell_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_layer_shell_v1::Request::GetLayerSurface { id, surface, .. } = request {
            state.layer = Some(init.init(id, ()));
            state.layer_surface = Some(surface);
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrLayerSurfaceV1,
        request: zwlr_layer_surface_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use zwlr_layer_surface_v1::Request;
        match request {
            Request::SetSize { width, height } => {
                state.last_size = (width, height);
                state.got.push(Got::Size(width, height));
            },
            Request::SetMargin { top, right, bottom, left } => {
                state.got.push(Got::Margin(top, right, bottom, left));
            },
            Request::SetAnchor { .. } => state.got.push(Got::Anchor),
            Request::SetExclusiveZone { zone } => state.got.push(Got::Zone(zone)),
            _ => (),
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
        _: <XdgWmBase as wayland_server::Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
