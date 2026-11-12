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

use dpi::LogicalSize;
use sctk::reexports::client::Proxy;
use sctk::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerSurface, SurfaceKind};
use tracing::warn;

/// The role a Wayland window's surface was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WaylandSurfaceRole {
    /// An ordinary `xdg_toplevel`.
    XdgToplevel,
    /// A `zwlr_layer_surface_v1`.
    LayerShell,
}

/// The layer a layer surface is stacked in, bottom to top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
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
#[non_exhaustive]
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

/// The axes on which a layer surface takes the size the compositor gives it: the protocol's
/// size 0 (`zwlr_layer_surface_v1.set_size`), "the compositor assigns it".
///
/// The protocol allows 0 only on an axis anchored to both of its edges (`left` and `right` for
/// the width, `top` and `bottom` for the height), so an axis is assigned only while it is
/// anchored that way; otherwise the requested surface size is sent, as without the flag. A
/// full-width bar is anchored left, right and to one of top or bottom, with `width` set: the
/// compositor gives it the output's width less the margins, and gives it again when the
/// output's mode changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct LayerShellCompositorSize {
    pub width: bool,
    pub height: bool,
}

/// Several changes to a layer surface, applied together in one commit
/// ([`crate::WindowExtWayland::set_layer_shell_changes`]): the layer-shell state is
/// double-buffered, so the compositor sees the surface go from its old configuration to the
/// new one with no state in between (a bar moved to another edge with a new margin and
/// zone is arranged once, not once per property). `None` leaves a property as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct LayerShellChanges {
    pub layer: Option<LayerShellLayer>,
    pub keyboard_interactivity: Option<LayerShellKeyboardInteractivity>,
    pub anchor: Option<LayerShellAnchor>,
    pub margins: Option<LayerShellMargins>,
    pub exclusive_zone: Option<i32>,
    pub compositor_size: Option<LayerShellCompositorSize>,
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
    /// The axes the compositor sizes ([`LayerShellCompositorSize`]). On such an axis the
    /// surface size the window was created with, or later asked for, is only what it has
    /// until the compositor's first configure.
    pub compositor_size: LayerShellCompositorSize,
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

    pub(crate) fn set_compositor_size(&mut self, axes: LayerShellCompositorSize) {
        self.compositor_size = axes;
    }

    /// Take every change `changes` names.
    pub(crate) fn set_changes(&mut self, changes: LayerShellChanges) {
        let LayerShellChanges {
            layer,
            keyboard_interactivity,
            anchor,
            margins,
            exclusive_zone,
            compositor_size,
        } = changes;
        if let Some(layer) = layer {
            self.layer = layer;
        }
        if let Some(value) = keyboard_interactivity {
            self.keyboard_interactivity = value;
        }
        if let Some(anchor) = anchor {
            self.anchor = anchor;
        }
        if let Some(margins) = margins {
            self.margins = margins;
        }
        if let Some(zone) = exclusive_zone {
            self.exclusive_zone = zone;
        }
        if let Some(axes) = compositor_size {
            self.compositor_size = axes;
        }
    }

    /// The axes the compositor assigns now: asked for, and anchored to both edges.
    pub(crate) fn assigned_axes(&self) -> (bool, bool) {
        let a = self.anchor;
        (
            self.compositor_size.width && a.left && a.right,
            self.compositor_size.height && a.top && a.bottom,
        )
    }

    /// What `set_size` is sent for a surface of `size` (logical): 0 on an assigned axis, at
    /// least 1 on the others (0 there is a protocol error).
    pub(crate) fn requested_size(&self, size: LogicalSize<u32>) -> (u32, u32) {
        let (width, height) = self.assigned_axes();
        (if width { 0 } else { size.width.max(1) }, if height { 0 } else { size.height.max(1) })
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
    /// window state's own business (`WindowState::resize`, `WindowState::send_layer_size`),
    /// through [`Self::requested_size`]. `layer_was`: the layer the
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(width: bool, height: bool, anchor: LayerShellAnchor) -> LayerShellAttributes {
        LayerShellAttributes {
            anchor,
            compositor_size: LayerShellCompositorSize { width, height },
            ..Default::default()
        }
    }

    const LEFT_RIGHT_BOTTOM: LayerShellAnchor =
        LayerShellAnchor { top: false, bottom: true, left: true, right: true };
    const ALL: LayerShellAnchor =
        LayerShellAnchor { top: true, bottom: true, left: true, right: true };

    /// The width is the compositor's on a surface anchored left and right; the height stays
    /// the requested one.
    #[test]
    fn an_axis_anchored_to_both_edges_is_sized_by_the_compositor() {
        let a = bar(true, false, LEFT_RIGHT_BOTTOM);
        assert_eq!(a.assigned_axes(), (true, false));
        assert_eq!(a.requested_size(LogicalSize::new(800, 32)), (0, 32));
        let all = bar(true, true, ALL);
        assert_eq!(all.requested_size(LogicalSize::new(800, 600)), (0, 0));
    }

    /// Changes taken together set what they name and keep the rest.
    #[test]
    fn changes_set_what_they_name() {
        let mut a = bar(true, false, LEFT_RIGHT_BOTTOM);
        a.margins = LayerShellMargins { top: 0, right: 0, bottom: 6, left: 0 };
        a.exclusive_zone = 32;
        let top = LayerShellAnchor { top: true, bottom: false, left: true, right: true };
        let margins = LayerShellMargins { top: 6, right: 0, bottom: 0, left: 0 };
        a.set_changes(LayerShellChanges {
            anchor: Some(top),
            margins: Some(margins),
            ..Default::default()
        });
        assert_eq!((a.anchor, a.margins, a.exclusive_zone), (top, margins, 32));
        assert_eq!(a.assigned_axes(), (true, false), "still left and right");
        let before = a.clone();
        a.set_changes(LayerShellChanges::default());
        assert_eq!(a, before, "no change names nothing");
    }

    /// 0 on an axis not anchored to both edges is a protocol error (`invalid_size`): the
    /// flag is not sent there, and a size never goes out as 0 without it.
    #[test]
    fn an_axis_not_anchored_to_both_edges_keeps_the_requested_size() {
        let a = bar(true, true, LEFT_RIGHT_BOTTOM);
        assert_eq!(a.assigned_axes(), (true, false), "height: bottom only");
        assert_eq!(a.requested_size(LogicalSize::new(800, 32)), (0, 32));
        let top_left = bar(true, true, LayerShellAnchor::TOP_LEFT);
        assert_eq!(top_left.requested_size(LogicalSize::new(300, 40)), (300, 40));
        let none = bar(false, false, ALL);
        assert_eq!(none.requested_size(LogicalSize::new(0, 0)), (1, 1));
    }
}
