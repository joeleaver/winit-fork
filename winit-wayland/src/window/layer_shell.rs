//! The `wlr-layer-shell` role of a window (rinch patch).
//!
//! A window asked for a layer surface ([`crate::WindowAttributesWayland::with_layer_shell`])
//! gets its `wl_surface` from a `zwlr_layer_surface_v1` instead of an `xdg_toplevel`, when the
//! compositor has `zwlr_layer_shell_v1`. The window state holds it as
//! [`WindowType::Layer`](super::state::WindowType::Layer), beside upstream's toplevel and
//! popup. Everything else — buffers, scale, input, the configure → `SurfaceResized` →
//! `RedrawRequested` path — is the ordinary window's, so a renderer that presents to the raw
//! `wl_surface` needs nothing special. A layer surface's configure carries only a size
//! (`WindowState::configure_layer`): no state, no decorations, so no frame is made.
//!
//! Requests newer than the bound `zwlr_layer_shell_v1` version are not sent (that is a
//! protocol error): `set_layer` needs v2, on-demand keyboard interactivity v4, which falls
//! back to none below it.

use sctk::reexports::client::Proxy;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::shell::WaylandSurface;
use sctk::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerSurface, SurfaceKind};
use sctk::shell::xdg::window::Window;
use tracing::warn;

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
    /// window state's own business (`WindowState::resize`). `layer_was`: the layer the
    /// surface already has (`None` at creation, where `get_layer_surface` set it), so
    /// `set_layer` is sent only for a change.
    pub(crate) fn apply(&self, surface: &LayerSurface, layer_was: Option<LayerShellLayer>) {
        let version = match surface.kind() {
            SurfaceKind::Wlr(wlr) => wlr.version(),
            _ => 1,
        };
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
            LayerShellKeyboardInteractivity::OnDemand if version >= 4 => {
                KeyboardInteractivity::OnDemand
            },
            LayerShellKeyboardInteractivity::OnDemand => {
                warn!(
                    "zwlr_layer_surface_v1 v{version} has no on-demand keyboard interactivity \
                     (v4); the layer surface takes no keyboard focus"
                );
                KeyboardInteractivity::None
            },
            LayerShellKeyboardInteractivity::Exclusive => KeyboardInteractivity::Exclusive,
        });
        if layer_was.is_some_and(|was| was != self.layer) {
            if version >= 2 {
                surface.set_layer(self.sctk_layer());
            } else {
                warn!("zwlr_layer_surface_v1 v{version} cannot change its layer (v2)");
            }
        }
    }
}

/// The role object a [`crate::Window`] holds: an xdg window or a layer surface.
#[derive(Debug, Clone)]
pub enum ShellSurface {
    Xdg(Window),
    Layer(LayerSurface),
}

impl ShellSurface {
    /// The xdg window, when this is one.
    pub fn xdg(&self) -> Option<&Window> {
        match self {
            ShellSurface::Xdg(window) => Some(window),
            ShellSurface::Layer(_) => None,
        }
    }

    pub fn role(&self) -> WaylandSurfaceRole {
        match self {
            ShellSurface::Xdg(_) => WaylandSurfaceRole::XdgToplevel,
            ShellSurface::Layer(_) => WaylandSurfaceRole::LayerShell,
        }
    }
}

impl WaylandSurface for ShellSurface {
    fn wl_surface(&self) -> &WlSurface {
        match self {
            ShellSurface::Xdg(window) => window.wl_surface(),
            ShellSurface::Layer(surface) => surface.wl_surface(),
        }
    }
}
