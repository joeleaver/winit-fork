use std::sync::Arc;

use dpi::{LogicalSize, PhysicalPosition};
use sctk::reexports::csd_frame::{DecorationsFrame, WindowState as XdgWindowState};
use sctk::shell::wlr_layer::LayerSurfaceConfigure;
use sctk::shell::xdg::popup::{ConfigureKind, PopupConfigure};
use sctk::shell::xdg::window::{DecorationMode, WindowConfigure};
use sctk::shm::Shm;
use sctk::subcompositor::SubcompositorState;
use tracing::warn;
use winit_core::event_loop::AsyncRequestSerial;

#[cfg(feature = "sctk-adwaita")]
use super::create_sctk_adwaita_config;
use super::{WindowState, WindowType, WinitFrame, placement_moved};

/// What a popup's configure changed (rinch patch).
#[derive(Debug, Default)]
pub struct PopupConfigured {
    /// Whether its surface was resized.
    pub resized: bool,
    /// Its new place, relative to its parent, when the configure placed it somewhere new (always
    /// for the first configure).
    pub moved: Option<PhysicalPosition<i32>>,
    /// The application's reposition requests this configure completed, oldest first.
    pub repositioned: Vec<AsyncRequestSerial>,
}

impl WindowState {
    pub fn configure_popup(&mut self, configure: PopupConfigure) -> PopupConfigured {
        let mut moved = false;
        let mut repositioned = Vec::new();
        if let WindowType::Popup { last_configure, repositions, .. } = &mut self.window {
            let previous = last_configure.as_ref().map(|configure| configure.position);
            moved = placement_moved(previous, configure.position);
            if let ConfigureKind::Reposition { token } = configure.kind {
                repositioned = repositions.complete(token);
            }
        }
        let resized = self.configure_popup_size(configure);
        // Read once the configure is stored, at the scale it was applied with.
        let moved = if moved { self.popup_position() } else { None };
        PopupConfigured { resized, moved, repositioned }
    }

    fn configure_popup_size(&mut self, configure: PopupConfigure) -> bool {
        // NOTE: when using fractional scaling or wl_compositor@v6 the scaling
        // should be delivered before the first configure, thus apply it to
        // properly scale the physical sizes provided by the users.
        if let Some(initial_size) = self.initial_size.take() {
            self.size = initial_size.to_logical(self.scale_factor());
        }

        // The popup was constrained to a different size by the compositor
        let constrained = self.size.width != configure.width as u32
            || self.size.height != configure.height as u32;
        let new_size =
            LogicalSize { width: configure.width as u32, height: configure.height as u32 };

        // NOTE: Set the configure before doing a resize, since we query it during it.
        if let WindowType::Popup { last_configure, .. } = &mut self.window {
            let kind = configure.kind.clone();
            *last_configure = Some(configure);

            // Always resize on the initial configure to properly initialize the viewport
            // destination and window geometry. This is required for fractional scaling
            // to work correctly: without calling resize(), viewport.set_destination()
            // is never called, and the compositor would interpret the buffer size as
            // logical pixels, making the popup appear at the wrong size. Also resize
            // when the compositor constrained us to a different size than requested.
            if matches!(kind, ConfigureKind::Initial) || constrained {
                self.resize(new_size);
                true
            } else {
                false
            }
        } else {
            tracing::error!(
                "configure_popup called for window type unequal of popup. This should never \
                 happen, because we start configuring with a popup"
            );
            false
        }
    }

    /// Creates (or drops) the CSD frame per `configure`'s decoration mode, and computes the
    /// surface size to apply, accounting for borders, configure bounds, and resize increments.
    ///
    /// Used by [`Self::configure_xdg_toplevel`] to configure an `xdg_toplevel`-based surface from
    /// a [`WindowConfigure`] event.
    fn configure_frame_and_size(
        &mut self,
        configure: &WindowConfigure,
        shm: &Shm,
        subcompositor: &Option<Arc<SubcompositorState>>,
    ) -> LogicalSize<u32> {
        // NOTE: when using fractional scaling or wl_compositor@v6 the scaling
        // should be delivered before the first configure, thus apply it to
        // properly scale the physical sizes provided by the users.
        if let Some(initial_size) = self.initial_size.take() {
            self.size = initial_size.to_logical(self.scale_factor());
            self.stateless_size = self.size;
        }

        if let Some(subcompositor) = subcompositor.as_ref().filter(|_| {
            configure.decoration_mode == DecorationMode::Client
                && self.frame.is_none()
                && !self.csd_fails
        }) {
            match WinitFrame::new(
                &self.window,
                shm,
                #[cfg(feature = "sctk-adwaita")]
                self.compositor.clone(),
                subcompositor.clone(),
                self.queue_handle.clone(),
                #[cfg(feature = "sctk-adwaita")]
                create_sctk_adwaita_config(self.theme),
            ) {
                Ok(mut frame) => {
                    frame.set_title(&self.title);
                    frame.set_scaling_factor(self.scale_factor);
                    // Hide the frame if we were asked to not decorate.
                    frame.set_hidden(!self.decorate);
                    self.frame = Some(frame);
                },
                Err(err) => {
                    warn!("Failed to create client side decorations frame: {err}");
                    self.csd_fails = true;
                },
            }
        } else if configure.decoration_mode == DecorationMode::Server {
            // Drop the frame for server side decorations to save resources.
            self.frame = None;
        }

        let stateless = Self::is_stateless(configure);

        let (mut new_size, constrain) = if let Some(frame) = self.frame.as_mut() {
            // Configure the window states.
            frame.update_state(configure.state);

            match configure.new_size {
                (Some(width), Some(height)) => {
                    let (width, height) = frame.subtract_borders(width, height);
                    let width = width.map(|w| w.get()).unwrap_or(1);
                    let height = height.map(|h| h.get()).unwrap_or(1);
                    ((width, height).into(), false)
                },
                (..) if stateless => (self.stateless_size, true),
                _ => (self.size, true),
            }
        } else {
            match configure.new_size {
                (Some(width), Some(height)) => ((width.get(), height.get()).into(), false),
                _ if stateless => (self.stateless_size, true),
                _ => (self.size, true),
            }
        };

        // Apply configure bounds only when compositor let the user decide what size to pick.
        if constrain {
            let bounds = self.surface_size_bounds(configure);
            new_size.width =
                bounds.0.map(|bound_w| new_size.width.min(bound_w.get())).unwrap_or(new_size.width);
            new_size.height = bounds
                .1
                .map(|bound_h| new_size.height.min(bound_h.get()))
                .unwrap_or(new_size.height);
        }

        // Apply size increments.
        //
        // We conditionally apply increments to avoid conflicts with the compositor's layout rules:
        // 1. If the window is floating (constrain == true), we snap to increments to ensure the
        //    app's grid alignment.
        // 2. If the user is interactively resizing (is_resizing), we snap the size to provide
        //    feedback.
        //
        // However, we MUST NOT snap if the compositor enforces a specific size (constrain == false,
        // or states like Maximized/Tiled). Snapping in these cases (e.g. corner tiling) would
        // shrink the window below the allocated area, creating visible gaps between valid
        // windows or screen edges.
        if (constrain || configure.is_resizing())
            && !configure.is_maximized()
            && !configure.is_fullscreen()
            && !configure.is_tiled()
        {
            new_size = self.snap_to_resize_increments(new_size);
        }

        new_size
    }

    /// `size` snapped down to the resize increments, from the min size.
    fn snap_to_resize_increments(&self, size: LogicalSize<u32>) -> LogicalSize<u32> {
        let Some(increments) = self.resize_increments else { return size };

        // We use min size as a base size for the increments, similar to how X11 does it.
        //
        // This ensures that we can always reach the min size and the increments are
        // calculated from it.
        let (delta_width, delta_height) = (
            size.width.saturating_sub(self.min_surface_size.width),
            size.height.saturating_sub(self.min_surface_size.height),
        );

        let width =
            self.min_surface_size.width + (delta_width / increments.width) * increments.width;
        let height =
            self.min_surface_size.height + (delta_height / increments.height) * increments.height;

        (width, height).into()
    }

    /// A `wlr-layer-shell` surface's configure (rinch patch): only a size, `0` on an axis
    /// meaning "you choose". Each axis given takes the compositor's size (an axis the
    /// compositor assigns gets it here, a bar's width); on an axis given as 0 the requested
    /// size is kept, snapped to the resize increments. A layer surface has no states and no
    /// decorations.
    pub fn configure_layer(&mut self, configure: LayerSurfaceConfigure) -> bool {
        // NOTE: when using fractional scaling or wl_compositor@v6 the scaling
        // should be delivered before the first configure, thus apply it to
        // properly scale the physical sizes provided by the users.
        if let Some(initial_size) = self.initial_size.take() {
            self.size = initial_size.to_logical(self.scale_factor());
            self.stateless_size = self.size;
        }

        let own = self.snap_to_resize_increments(self.size);
        let new_size = match configure.new_size {
            (width, height) if width > 0 && height > 0 => (width, height).into(),
            (width, height) => LogicalSize::new(
                if width > 0 { width } else { own.width },
                if height > 0 { height } else { own.height },
            ),
        };

        if let WindowType::Layer { last_configure, .. } = &mut self.window {
            // NOTE: `None` is present for the initial configure, thus we must always resize.
            let initial = last_configure.is_none();
            *last_configure = Some(configure);

            if initial || new_size != self.surface_size() {
                self.resize(new_size);
                true
            } else {
                false
            }
        } else {
            tracing::error!("configure_layer called for a window that is not a layer surface");
            false
        }
    }

    /// Update the last-received configure and resize if necessary.
    ///
    /// Used for surfaces backed by an `xdg_toplevel` (unlike `Popup`, handled by
    /// [`Self::configure_popup`], and a layer surface, by [`Self::configure_layer`]).
    pub fn configure_xdg_toplevel(
        &mut self,
        configure: WindowConfigure,
        shm: &Shm,
        subcompositor: &Option<Arc<SubcompositorState>>,
    ) -> bool {
        let new_size = self.configure_frame_and_size(&configure, shm, subcompositor);

        let new_state = configure.state;
        let last_configure = match &mut self.window {
            WindowType::Window { last_configure, .. } => last_configure,
            WindowType::Popup { .. } => {
                tracing::error!(
                    "configure_xdg_toplevel called for a `Popup`. This should never happen, \
                     because popups are configured through `configure_popup`"
                );
                return false;
            },
            WindowType::Layer { .. } => {
                tracing::error!(
                    "configure_xdg_toplevel called for a layer surface. This should never happen, \
                     because layer surfaces are configured through `configure_layer`"
                );
                return false;
            },
        };

        let old_state = last_configure.as_ref().map(|configure| configure.state);

        let state_change_requires_resize = old_state
            .map(|old_state| {
                !old_state
                    .symmetric_difference(new_state)
                    .difference(XdgWindowState::ACTIVATED | XdgWindowState::SUSPENDED)
                    .is_empty()
            })
            // NOTE: `None` is present for the initial configure, thus we must always resize.
            .unwrap_or(true);

        // NOTE: Set the configure before doing a resize, since we query it during it.
        *last_configure = Some(configure);

        if state_change_requires_resize || new_size != self.surface_size() {
            self.resize(new_size);
            true
        } else {
            false
        }
    }
}
