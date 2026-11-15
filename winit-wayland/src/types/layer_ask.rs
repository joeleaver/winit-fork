//! The marker that ends a layer surface's size request when no configure answers it (rinch
//! patch; see [`crate::window::layer_asks`]).
//!
//! The request commits, then sends a `wl_display.sync`; when that is done a second one is
//! sent, and when the second is done every configure the compositor was going to send for
//! the request has arrived. The request ends there, and only there: the newest configure
//! remembered since it went out is the compositor's answer. Whatever the state machine says
//! follows (the answer applied, or a held request going out, applied at once, with its own
//! marker) is done here.

use sctk::dispatch2::Dispatch2;
use sctk::reexports::client::protocol::wl_callback::{Event as CallbackEvent, WlCallback};
use sctk::reexports::client::{Connection, QueueHandle};
use winit_core::window::WindowId;

use crate::state::WinitState;
use crate::window::state::MarkerDone;

/// One sync of a size request's marker.
#[derive(Debug, Clone, Copy)]
pub struct LayerAskMarker {
    pub window_id: WindowId,
    /// The request's generation ([`crate::window::layer_asks::Ask::Send`]).
    pub generation: u64,
    /// 1 or 2.
    pub stage: u8,
}

impl LayerAskMarker {
    /// Start the marker of a request just sent and committed on `window_id`.
    pub fn start(
        conn: &Connection,
        qh: &QueueHandle<WinitState>,
        window_id: WindowId,
        generation: u64,
    ) {
        conn.display().sync(qh, LayerAskMarker { window_id, generation, stage: 1 });
    }
}

impl Dispatch2<WlCallback, WinitState> for LayerAskMarker {
    fn event(
        &self,
        state: &mut WinitState,
        _proxy: &WlCallback,
        event: CallbackEvent,
        conn: &Connection,
        qh: &QueueHandle<WinitState>,
    ) {
        let CallbackEvent::Done { .. } = event else {
            return;
        };
        let Some(window) = state.windows.get_mut().get(&self.window_id).cloned() else {
            return;
        };
        let done = window.lock().unwrap().layer_marker_done(self.generation, self.stage);
        match done {
            MarkerDone::SyncAgain => {
                conn.display().sync(qh, LayerAskMarker { stage: 2, ..*self });
            },
            MarkerDone::Ended { resized, marker } => {
                if resized {
                    // A held request went out, applied at once, or the compositor's answer
                    // was applied: told as a resize.
                    state.queue_resize(self.window_id);
                }
                if let Some(generation) = marker {
                    // A request went out (committed): start its marker.
                    LayerAskMarker::start(conn, qh, self.window_id, generation);
                }
            },
            MarkerDone::Nothing => {},
        }
    }
}
