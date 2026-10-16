//! The `wlr-layer-shell` role of a window (rinch patch).
//!
//! A window asked for a layer surface ([`crate::WindowAttributesWayland::with_layer_shell`])
//! gets its `wl_surface` from a `zwlr_layer_surface_v1` instead of an `xdg_toplevel`, when the
//! compositor has `zwlr_layer_shell_v1`. Everything else — buffers, scale, input, the
//! configure → `SurfaceResized` → `RedrawRequested` path — is the ordinary window's, so a
//! renderer that presents to the raw `wl_surface` needs nothing special. The layer surface's
//! configure is turned into an `xdg` [`WindowConfigure`] with no state and server-side
//! decorations, which the existing window state code already handles.

use std::num::NonZeroU32;

use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::shell::WaylandSurface;
use sctk::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerSurface, LayerSurfaceConfigure,
};
use sctk::reexports::csd_frame::WindowState;
use sctk::shell::xdg::XdgSurface;
use sctk::shell::xdg::window::{DecorationMode, Window, WindowConfigure};

/// The role a Wayland window's surface was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaylandSurfaceRole {
    /// An ordinary `xdg_toplevel`.
    XdgToplevel,
    /// A `zwlr_layer_surface_v1`.
    LayerShell,
}

/// The layer a layer surface is stacked in, bottom to top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LayerShellLayer {
    /// Below everything, a desktop background.
    Background,
    /// Above the background, below ordinary windows.
    Bottom,
    /// Above ordinary windows (panels, docks).
    #[default]
    Top,
    /// Above everything, fullscreen windows included.
    Overlay,
}

/// Whether a layer surface can take keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LayerShellKeyboardInteractivity {
    /// Never takes keyboard focus.
    #[default]
    None,
    /// Takes focus the compositor's usual way (a click).
    OnDemand,
    /// Takes all keyboard input while mapped in the top or overlay layer.
    Exclusive,
}

/// The edges of the output a layer surface is anchored to. None anchored: centred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct LayerShellAnchor {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}

impl LayerShellAnchor {
    /// The top-left corner: margins `top` and `left` are then the surface's position.
    pub const TOP_LEFT: Self = Self { top: true, bottom: false, left: true, right: false };
}

/// Distances from the anchored edges, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct LayerShellMargins {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

/// What a layer surface is created with ([`crate::WindowAttributesWayland::with_layer_shell`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LayerShellAttributes {
    pub layer: LayerShellLayer,
    pub keyboard_interactivity: LayerShellKeyboardInteractivity,
    pub anchor: LayerShellAnchor,
    pub margins: LayerShellMargins,
    /// `-1` ignores other surfaces' zones, `0` (the default) keeps clear of them, a
    /// positive value reserves that much of the anchored edge.
    pub exclusive_zone: i32,
    /// The output (by `wl_output` name, e.g. `DP-1`) to show on; `None` lets the
    /// compositor choose. An unknown name is the compositor's choice too.
    pub output_name: Option<String>,
    /// What the surface is for (`zwlr_layer_shell_v1.get_layer_surface`'s namespace).
    pub namespace: String,
}

impl LayerShellAttributes {
    pub(crate) fn set_margins(&mut self, margins: LayerShellMargins) {
        self.margins = margins;
    }

    pub(crate) fn set_anchor(&mut self, anchor: LayerShellAnchor) {
        self.anchor = anchor;
    }

    pub(crate) fn set_layer(&mut self, layer: LayerShellLayer) {
        self.layer = layer;
    }

    pub(crate) fn set_keyboard_interactivity(&mut self, value: LayerShellKeyboardInteractivity) {
        self.keyboard_interactivity = value;
    }

    pub(crate) fn set_exclusive_zone(&mut self, zone: i32) {
        self.exclusive_zone = zone;
    }

    pub(crate) fn sctk_layer(&self) -> Layer {
        match self.layer {
            LayerShellLayer::Background => Layer::Background,
            LayerShellLayer::Bottom => Layer::Bottom,
            LayerShellLayer::Top => Layer::Top,
            LayerShellLayer::Overlay => Layer::Overlay,
        }
    }

    /// Send every double-buffered request (applied with the next commit). The size is the
    /// window state's own business ([`ShellSurface::set_size`]).
    pub(crate) fn apply(&self, surface: &LayerSurface) {
        let a = self.anchor;
        let mut anchor = Anchor::empty();
        anchor.set(Anchor::TOP, a.top);
        anchor.set(Anchor::BOTTOM, a.bottom);
        anchor.set(Anchor::LEFT, a.left);
        anchor.set(Anchor::RIGHT, a.right);
        surface.set_anchor(anchor);
        let m = self.margins;
        surface.set_margin(m.top, m.right, m.bottom, m.left);
        surface.set_exclusive_zone(self.exclusive_zone);
        surface.set_keyboard_interactivity(match self.keyboard_interactivity {
            LayerShellKeyboardInteractivity::None => KeyboardInteractivity::None,
            LayerShellKeyboardInteractivity::OnDemand => KeyboardInteractivity::OnDemand,
            LayerShellKeyboardInteractivity::Exclusive => KeyboardInteractivity::Exclusive,
        });
        surface.set_layer(self.sctk_layer());
    }
}

/// The role object behind a window's surface.
#[derive(Debug, Clone)]
pub enum ShellSurface {
    Xdg(Window),
    Layer(LayerSurface, Box<LayerShellAttributes>),
}

impl ShellSurface {
    /// The xdg window, when this is one.
    pub fn xdg(&self) -> Option<&Window> {
        match self {
            ShellSurface::Xdg(window) => Some(window),
            ShellSurface::Layer(..) => None,
        }
    }

    pub fn role(&self) -> WaylandSurfaceRole {
        match self {
            ShellSurface::Xdg(_) => WaylandSurfaceRole::XdgToplevel,
            ShellSurface::Layer(..) => WaylandSurfaceRole::LayerShell,
        }
    }

    /// Change a layer surface's attributes and send them; `false` for an xdg window.
    pub fn update_layer(&mut self, update: impl FnOnce(&mut LayerShellAttributes)) -> bool {
        match self {
            ShellSurface::Xdg(_) => false,
            ShellSurface::Layer(surface, attributes) => {
                update(attributes);
                attributes.apply(surface);
                true
            },
        }
    }

    /// Set the window geometry (xdg) or the layer surface's size.
    pub fn set_size(&self, x: i32, y: i32, width: u32, height: u32) {
        match self {
            ShellSurface::Xdg(window) => {
                window.xdg_surface().set_window_geometry(x, y, width as i32, height as i32)
            },
            ShellSurface::Layer(surface, _) => surface.set_size(width, height),
        }
    }
}

impl WaylandSurface for ShellSurface {
    fn wl_surface(&self) -> &WlSurface {
        match self {
            ShellSurface::Xdg(window) => window.wl_surface(),
            ShellSurface::Layer(surface, _) => surface.wl_surface(),
        }
    }
}

/// A configure as the window state reads it: an xdg one's fields, or a layer surface's
/// size with no state and server-side decorations, so no frame is made. (sctk's
/// [`WindowConfigure`] is `#[non_exhaustive]` and cannot be built here.)
#[derive(Debug, Clone)]
pub struct SurfaceConfigure {
    pub new_size: (Option<NonZeroU32>, Option<NonZeroU32>),
    pub suggested_bounds: Option<(u32, u32)>,
    pub decoration_mode: DecorationMode,
    pub state: WindowState,
}

impl SurfaceConfigure {
    #[inline]
    pub fn is_maximized(&self) -> bool {
        self.state.contains(WindowState::MAXIMIZED)
    }

    #[inline]
    pub fn is_fullscreen(&self) -> bool {
        self.state.contains(WindowState::FULLSCREEN)
    }

    #[inline]
    pub fn is_tiled(&self) -> bool {
        self.state.contains(WindowState::TILED)
    }
}

impl From<WindowConfigure> for SurfaceConfigure {
    fn from(configure: WindowConfigure) -> Self {
        Self {
            new_size: configure.new_size,
            suggested_bounds: configure.suggested_bounds,
            decoration_mode: configure.decoration_mode,
            state: configure.state,
        }
    }
}

impl From<LayerSurfaceConfigure> for SurfaceConfigure {
    /// `0` is "you choose", i.e. `None`.
    fn from(configure: LayerSurfaceConfigure) -> Self {
        let (width, height) = configure.new_size;
        Self {
            new_size: (NonZeroU32::new(width), NonZeroU32::new(height)),
            suggested_bounds: None,
            decoration_mode: DecorationMode::Server,
            state: WindowState::empty(),
        }
    }
}
