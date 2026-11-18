//! # Winit's Wayland backend.
//!
//! **Note:** Windows don't appear on Wayland until you draw/present to them.
//!
//! By default, Winit loads system libraries using `dlopen`. This can be
//! disabled by disabling the `"wayland-dlopen"` cargo feature.
//!
//! ## Client-side decorations
//!
//! Winit provides client-side decorations by default, but the behaviour can
//! be controlled with the following feature flags:
//!
//! * `wayland-csd-adwaita` (default).
//! * `wayland-csd-adwaita-crossfont`.
//! * `wayland-csd-adwaita-notitle`.
//! * `wayland-csd-adwaita-notitlebar`.

#![allow(clippy::mutable_key_type)]
#![warn(clippy::exhaustive_enums)]

use std::ffi::c_void;
use std::hash::BuildHasher;
use std::ptr::NonNull;

use dpi::{LogicalPosition, LogicalSize, PhysicalSize, Size};
use sctk::reexports::client::Proxy;
use sctk::reexports::client::backend::ObjectId;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::shm::slot::{Buffer, CreateBufferError, SlotPool};
use wayland_client::protocol::wl_shm::Format;
use winit_core::data_transfer::DataTransferId;
use winit_core::error::{NotSupportedError, RequestError};
use winit_core::event_loop::{ActiveEventLoop as CoreActiveEventLoop, AsyncRequestSerial};
use winit_core::monitor::MonitorHandleProvider;
use winit_core::window::{
    ActivationToken, PlatformWindowAttributes, Window as CoreWindow, WindowId, WindowPositioner,
};

macro_rules! os_error {
    ($error:expr) => {{ winit_core::error::OsError::new(line!(), file!(), $error) }};
}

mod data_transfer;
mod event_loop;
mod output;
mod popup;
mod seat;
mod state;
mod types;
mod window;

pub use self::data_transfer::{DataOffer, DragSource, MimeData, MimeType};
pub use self::event_loop::{ActiveEventLoop, EventLoop};
use self::output::MonitorHandle;
pub use self::popup::Popup;
pub use self::window::Window;
pub use self::window::layer_shell::{
    LayerShellAnchor, LayerShellAttributes, LayerShellChanges, LayerShellCompositorSize,
    LayerShellKeyboardInteractivity, LayerShellLayer, LayerShellMargins, WaylandSurfaceRole,
};

/// Additional methods on [`ActiveEventLoop`] that are specific to Wayland.
pub trait ActiveEventLoopExtWayland {
    /// True if the [`ActiveEventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;

    /// The globals the compositor's registry advertises now, in no particular order:
    /// every protocol interface it offers, each with its version and its global name.
    /// Empty when the event loop is not Wayland.
    ///
    /// It is the registry's list as winit keeps it, so a global the compositor adds or
    /// withdraws later (a `wl_output` plugged in) is in, or gone from, the next answer.
    /// What it says is what the compositor offers, not what winit bound: a
    /// `zwlr_layer_shell_v1` here is what lets
    /// [`WindowAttributesWayland::with_layer_shell`] make a layer surface.
    fn wayland_globals(&self) -> Vec<WaylandGlobal>;
}

impl ActiveEventLoopExtWayland for dyn CoreActiveEventLoop + '_ {
    #[inline]
    fn is_wayland(&self) -> bool {
        self.cast_ref::<ActiveEventLoop>().is_some()
    }

    fn wayland_globals(&self) -> Vec<WaylandGlobal> {
        let Some(event_loop) = self.cast_ref::<ActiveEventLoop>() else {
            return Vec::new();
        };
        let state = event_loop.state.borrow();
        state
            .registry_state
            .globals()
            .map(|global| WaylandGlobal {
                name: global.name,
                interface: global.interface.clone(),
                version: global.version,
            })
            .collect()
    }
}

/// One global the compositor's registry advertises
/// ([`ActiveEventLoopExtWayland::wayland_globals`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct WaylandGlobal {
    /// The global's name: the registry's number for it, unique among the globals the
    /// compositor advertises while it is advertised. A `wl_output`'s is what
    /// [`LayerShellAttributes::output_id`] takes, and what
    /// `MonitorHandle::native_id` answers for it.
    pub name: u32,
    /// The protocol interface, such as `zwlr_layer_shell_v1` or `wl_output`.
    pub interface: String,
    /// The highest version of the interface the compositor offers.
    pub version: u32,
}

/// Additional methods on [`EventLoop`] that are specific to Wayland.
pub trait EventLoopExtWayland {
    /// True if the [`EventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;
}

/// Additional methods when building event loop that are specific to Wayland.
pub trait EventLoopBuilderExtWayland {
    /// Force using Wayland.
    fn with_wayland(&mut self) -> &mut Self;

    /// Whether to allow the event loop to be created off of the main thread.
    ///
    /// By default, the window is only allowed to be created on the main
    /// thread, to make platform compatibility easier.
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self;
}

/// Additional methods on [`Window`] that are specific to Wayland.
///
/// [`Window`]: crate::window::Window
pub trait WindowExtWayland {
    /// Returns `xdg_toplevel` of the window or [`None`] if the window is X11 window
    /// (or a `wlr-layer-shell` surface).
    fn xdg_toplevel(&self) -> Option<NonNull<c_void>>;

    /// The role the window's surface was given: [`None`] on X11. A window asked for a
    /// layer-shell role ([`WindowAttributesWayland::with_layer_shell`]) answers
    /// [`WaylandSurfaceRole::XdgToplevel`] when the compositor had no `zwlr_layer_shell_v1`.
    fn wayland_surface_role(&self) -> Option<WaylandSurfaceRole>;

    /// Change a layer surface's margins (logical px). Ignored by an xdg-toplevel.
    /// Committed at once (with the buffer already attached), so no new frame is needed.
    ///
    /// A change is confirmed by a `WindowEvent::Moved` carrying the new top-left margins
    /// (scaled to physical px), delivered in order with the pointer's events once the
    /// compositor has processed the commit: pointer positions before it are over the
    /// surface's old place, those after it over the new one.
    fn set_layer_shell_margins(&self, margins: LayerShellMargins);

    /// Change a layer surface's anchors. Ignored by an xdg-toplevel.
    fn set_layer_shell_anchor(&self, anchor: LayerShellAnchor);

    /// Change a layer surface's layer. Ignored by an xdg-toplevel.
    fn set_layer_shell_layer(&self, layer: LayerShellLayer);

    /// Change a layer surface's keyboard interactivity. Ignored by an xdg-toplevel.
    fn set_layer_shell_keyboard_interactivity(
        &self,
        interactivity: LayerShellKeyboardInteractivity,
    );

    /// Change a layer surface's exclusive zone. Ignored by an xdg-toplevel.
    fn set_layer_shell_exclusive_zone(&self, zone: i32);

    /// Change the axes a layer surface takes the compositor's size on
    /// ([`LayerShellCompositorSize`]). Ignored by an xdg-toplevel. A change of anchors
    /// ([`Self::set_layer_shell_anchor`]) can also start or end an axis's assignment; either
    /// way the size request is sent again with the change.
    fn set_layer_shell_compositor_size(&self, axes: LayerShellCompositorSize);

    /// Change several of a layer surface's properties in **one** commit
    /// ([`LayerShellChanges`]; `None` keeps a property). Ignored by an xdg-toplevel. Each
    /// single-property setter above is a commit of its own, so changing anchors, margins and
    /// the exclusive zone through them shows the compositor each state in between; this
    /// shows it only the result. A change of margins is confirmed by `WindowEvent::Moved`, as
    /// for [`Self::set_layer_shell_margins`].
    fn set_layer_shell_changes(&self, changes: LayerShellChanges);

    /// [`Self::set_layer_shell_changes`] and a size request (`request_surface_size`) in
    /// **one** commit (rinch patch): the compositor sees a bar's new edge, zone and height
    /// as one change, with no state in between. `request_surface_size` on a layer surface
    /// commits its `set_size` at once, so calling the two one after the other is two
    /// commits, in either order.
    ///
    /// The size is read against the new anchors and answered as `request_surface_size`
    /// answers it, and so is the return value: the new size when it was applied now, `None`
    /// when it is held behind a request in flight (it comes as a `SurfaceResized` then) or
    /// when `size` is `None`. On a surface that is not a layer surface the changes are
    /// ignored and the size is requested as `request_surface_size` requests it.
    fn set_layer_shell_changes_and_size(
        &self,
        changes: LayerShellChanges,
        size: Option<Size>,
    ) -> Option<PhysicalSize<u32>>;

    /// Re-place a [`WindowType::Popup`] with a new `positioner` and/or at a new `size`, in one
    /// `xdg_popup.reposition` (rinch patch). The serial returned comes back in
    /// `WindowEvent::PopupRepositioned` once the compositor has applied it, after the
    /// `WindowEvent::Moved` and `WindowEvent::SurfaceResized` it causes.
    ///
    /// Fails with a not-supported error for a window that is not a popup, on X11, and where the
    /// compositor's `xdg_wm_base` is older than v3.
    ///
    /// [`WindowType::Popup`]: winit_core::window::WindowType::Popup
    fn request_popup_reposition(
        &self,
        positioner: Option<WindowPositioner>,
        size: Option<Size>,
    ) -> Result<AsyncRequestSerial, RequestError>;
    /// Set the surface's input region (rinch patch): the union of `rects`, each `(x, y,
    /// width, height)` in surface-local logical px, or the whole surface for `None` (the
    /// default). An empty slice is an **empty** region, so every pointer and touch event
    /// over the surface goes to whatever is below it. A rect with no area is skipped.
    ///
    /// `wl_surface.set_input_region` is double-buffered: with `commit: false` the region
    /// goes with the surface's next commit, which is the next frame the app presents, so a
    /// region computed from a frame's layout is set just before that frame's present and
    /// lands in the same commit as its buffer. With `commit: true` the surface is committed
    /// at once (with the buffer already attached), for a change no new frame follows.
    ///
    /// Works for an xdg-toplevel, a layer surface and a popup. Fails with a not-supported
    /// error on X11.
    fn set_input_region(
        &self,
        rects: Option<&[(i32, i32, i32, i32)]>,
        commit: bool,
    ) -> Result<(), RequestError>;
}

impl WindowExtWayland for dyn CoreWindow + '_ {
    #[inline]
    fn xdg_toplevel(&self) -> Option<NonNull<c_void>> {
        self.cast_ref::<Window>()?.xdg_toplevel()
    }

    fn wayland_surface_role(&self) -> Option<WaylandSurfaceRole> {
        Some(self.cast_ref::<Window>()?.surface_role())
    }

    fn set_layer_shell_margins(&self, margins: LayerShellMargins) {
        if let Some(window) = self.cast_ref::<Window>() {
            window.update_layer_shell(|layer| layer.set_margins(margins));
        }
    }

    fn set_layer_shell_anchor(&self, anchor: LayerShellAnchor) {
        if let Some(window) = self.cast_ref::<Window>() {
            window.update_layer_shell(|layer| layer.set_anchor(anchor));
        }
    }

    fn set_layer_shell_layer(&self, layer: LayerShellLayer) {
        if let Some(window) = self.cast_ref::<Window>() {
            window.update_layer_shell(|surface| surface.set_layer(layer));
        }
    }

    fn set_layer_shell_keyboard_interactivity(
        &self,
        interactivity: LayerShellKeyboardInteractivity,
    ) {
        if let Some(window) = self.cast_ref::<Window>() {
            window.update_layer_shell(|layer| layer.set_keyboard_interactivity(interactivity));
        }
    }

    fn set_layer_shell_exclusive_zone(&self, zone: i32) {
        if let Some(window) = self.cast_ref::<Window>() {
            window.update_layer_shell(|layer| layer.set_exclusive_zone(zone));
        }
    }

    fn set_layer_shell_compositor_size(&self, axes: LayerShellCompositorSize) {
        if let Some(window) = self.cast_ref::<Window>() {
            window.update_layer_shell(|layer| layer.set_compositor_size(axes));
        }
    }

    fn set_layer_shell_changes(&self, changes: LayerShellChanges) {
        if let Some(window) = self.cast_ref::<Window>() {
            window.update_layer_shell(|layer| layer.set_changes(changes));
        }
    }

    fn set_layer_shell_changes_and_size(
        &self,
        changes: LayerShellChanges,
        size: Option<Size>,
    ) -> Option<PhysicalSize<u32>> {
        match self.cast_ref::<Window>() {
            Some(window) => {
                window.update_layer_shell_and_size(|layer| layer.set_changes(changes), size)
            },
            None => size.and_then(|size| self.request_surface_size(size)),
        }
    }

    fn request_popup_reposition(
        &self,
        positioner: Option<WindowPositioner>,
        size: Option<Size>,
    ) -> Result<AsyncRequestSerial, RequestError> {
        let Some(popup) = self.cast_ref::<Popup>() else {
            return Err(NotSupportedError::new("only a Wayland popup is repositioned").into());
        };
        popup.request_reposition(positioner, size)
    }

    fn set_input_region(
        &self,
        rects: Option<&[(i32, i32, i32, i32)]>,
        commit: bool,
    ) -> Result<(), RequestError> {
        if let Some(window) = self.cast_ref::<Window>() {
            return window.set_input_region(rects, commit);
        }
        if let Some(popup) = self.cast_ref::<Popup>() {
            return popup.set_input_region(rects, commit);
        }
        Err(NotSupportedError::new("not a Wayland surface").into())
    }
}

/// Additional methods on [`MonitorHandle`] that are specific to Wayland.
///
/// [`MonitorHandle`]: winit_core::monitor::MonitorHandle
pub trait MonitorHandleExtWayland {
    /// The output's top-left in the compositor's logical (global) space: `xdg_output`'s
    /// `logical_position`, the coordinates the compositor lays its outputs out in. [`None`] when
    /// the compositor has no `zxdg_output_manager_v1`, or for a monitor that is not Wayland's.
    ///
    /// Unlike `MonitorHandle::position`, this is not multiplied by the output's integer
    /// `wl_output.scale`, so it is exact under fractional scaling.
    fn logical_position(&self) -> Option<LogicalPosition<i32>>;

    /// The output's size in the compositor's logical space: `xdg_output`'s `logical_size`, its
    /// current mode after the output's transform (a rotated output's is rotated) divided by the
    /// scale the compositor renders it at, fractional scales included. [`None`] as for
    /// [`logical_position`](Self::logical_position).
    fn logical_size(&self) -> Option<LogicalSize<i32>>;
}

impl MonitorHandleExtWayland for dyn MonitorHandleProvider + '_ {
    fn logical_position(&self) -> Option<LogicalPosition<i32>> {
        let monitor = self.cast_ref::<MonitorHandle>()?;
        monitor.logical_position()
    }

    fn logical_size(&self) -> Option<LogicalSize<i32>> {
        let monitor = self.cast_ref::<MonitorHandle>()?;
        monitor.logical_size()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApplicationName {
    pub(crate) general: String,
    pub(crate) instance: String,
}

/// Window attributes methods specific to Wayland.
#[derive(Debug, Default, Clone)]
pub struct WindowAttributesWayland {
    pub(crate) name: Option<ApplicationName>,
    pub(crate) activation_token: Option<ActivationToken>,
    pub(crate) prefer_csd: bool,
    pub(crate) layer_shell: Option<LayerShellAttributes>,
    pub(crate) initial_scale_factor: Option<f64>,
}

impl WindowAttributesWayland {
    /// Build window with the given name.
    ///
    /// The `general` name sets an application ID, which should match the `.desktop`
    /// file distributed with your program. The `instance` is a `no-op`.
    ///
    /// For details about application ID conventions, see the
    /// [Desktop Entry Spec](https://specifications.freedesktop.org/desktop-entry-spec/desktop-entry-spec-latest.html#desktop-file-id)
    #[inline]
    pub fn with_name(mut self, general: impl Into<String>, instance: impl Into<String>) -> Self {
        self.name = Some(ApplicationName { general: general.into(), instance: instance.into() });
        self
    }

    /// Sets an activation token to use when creating the window.
    ///
    /// The activation token allows the compositor to grant focus to the new window,
    /// overriding focus-stealing prevention. Obtain a token via
    /// [`Window::request_activation_token`].
    #[inline]
    pub fn with_activation_token(mut self, token: ActivationToken) -> Self {
        self.activation_token = Some(token);
        self
    }

    /// Builds the window with a given preference for client-side decorations.
    ///
    /// When set to `true`, the window will tell the compositor that it prefers
    /// client-side decorations, even if server-side decorations are available.
    /// When set to `false` (the default), the window will indicate a preference
    /// for server-side decorations.
    #[inline]
    pub fn with_prefer_csd(mut self, prefer_csd: bool) -> Self {
        self.prefer_csd = prefer_csd;
        self
    }

    /// Give the window a `wlr-layer-shell` role instead of an xdg-toplevel (a panel, a
    /// dock, an overlay that never takes keyboard focus).
    ///
    /// When the compositor has no `zwlr_layer_shell_v1` (GNOME/Mutter) the window is an
    /// ordinary xdg-toplevel, as without this call; [`WindowExtWayland::wayland_surface_role`]
    /// says which one it got. A layer surface has no decorations, title, minimize, maximize,
    /// fullscreen, compositor move or resize: `drag_window` and `drag_resize_window` return
    /// [`NotSupportedError`](winit_core::error::NotSupportedError). Its size is the surface
    /// size, requested with `request_surface_size`, except on an axis the compositor sizes
    /// ([`LayerShellAttributes::compositor_size`]), where a request keeps the compositor's.
    #[inline]
    pub fn with_layer_shell(mut self, layer_shell: LayerShellAttributes) -> Self {
        self.layer_shell = Some(layer_shell);
        self
    }

    /// The scale factor the window starts with, until the compositor sends one (rinch patch).
    ///
    /// A compositor tells a surface its scale (`wp_fractional_scale_v1.preferred_scale`,
    /// `wl_surface.preferred_buffer_scale`, or the outputs it entered) only once the surface
    /// is mapped, which takes a first buffer. Without this the window starts at scale `1.0`
    /// (or its parent's), so on a scaled output its first frame is drawn at scale 1 and the
    /// next at the output's. Give the scale the window will most likely get, the scale of the
    /// output it is going to (a layer surface's output), of the only output, or the scale it
    /// had when last shown, and the first buffer is drawn at it: [`Window::scale_factor`] and
    /// a [`Size::Physical`] surface size use it from creation on, and the buffer scale is set
    /// to it (rounded up to a whole number where the compositor has no fractional scaling, as
    /// the compositor itself would). When the compositor's scale arrives and differs,
    /// [`WindowEvent::ScaleFactorChanged`] is sent as usual; when it is the same, nothing is.
    ///
    /// A value that is not finite and positive is ignored.
    ///
    /// [`Window::scale_factor`]: winit_core::window::Window::scale_factor
    /// [`Size::Physical`]: dpi::Size::Physical
    /// [`WindowEvent::ScaleFactorChanged`]: winit_core::event::WindowEvent::ScaleFactorChanged
    #[inline]
    pub fn with_initial_scale_factor(mut self, scale_factor: f64) -> Self {
        self.initial_scale_factor = Some(scale_factor);
        self
    }
}

impl PlatformWindowAttributes for WindowAttributesWayland {
    fn box_clone(&self) -> Box<dyn PlatformWindowAttributes> {
        Box::from(self.clone())
    }
}

/// Get the WindowId out of the surface.
#[inline]
fn make_wid(surface: &WlSurface) -> WindowId {
    WindowId::from_raw(surface.id().as_ptr() as usize)
}

/// Create a `DataTransferId` for the given data device and serial.
///
/// It's currently unclear if this will result in the same ID when transferring to the same
/// application.
#[inline]
fn make_data_transfer_id(data_device_id: ObjectId, serial: u32) -> DataTransferId {
    const BUILD_HASHER: foldhash::fast::FixedState = foldhash::fast::FixedState::with_seed(0);

    DataTransferId::from_raw(BUILD_HASHER.hash_one((data_device_id, serial)) as i64)
}

/// The default routine does floor, but we need round on Wayland.
fn logical_to_physical_rounded(size: LogicalSize<u32>, scale_factor: f64) -> PhysicalSize<u32> {
    let width = size.width as f64 * scale_factor;
    let height = size.height as f64 * scale_factor;
    (width.round(), height.round()).into()
}

/// Converts an image buffer to a Wayland buffer (`wl_buffer`)
fn image_to_buffer(
    width: i32,
    height: i32,
    data: &[u8],
    format: Format,
    pool: &mut SlotPool,
) -> Result<Buffer, CreateBufferError> {
    let (buffer, canvas) = pool.create_buffer(width, height, 4 * width, format)?;

    for (canvas_chunk, rgba) in canvas.chunks_exact_mut(4).zip(data.chunks_exact(4)) {
        // Alpha in buffer is premultiplied.
        let alpha = rgba[3] as f32 / 255.;
        let r = (rgba[0] as f32 * alpha) as u32;
        let g = (rgba[1] as f32 * alpha) as u32;
        let b = (rgba[2] as f32 * alpha) as u32;
        let color = ((rgba[3] as u32) << 24) + (r << 16) + (g << 8) + b;
        let array: &mut [u8; 4] = canvas_chunk.try_into().unwrap();
        *array = color.to_le_bytes();
    }

    Ok(buffer)
}
