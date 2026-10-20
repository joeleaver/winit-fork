//! A layer surface's move, confirmed by the compositor (rinch #1523).
//!
//! A layer surface has no compositor move: the client moves it by setting its
//! margins and committing. Pointer events keep coming relative to where the
//! compositor has the surface, which until the commit is processed is the old
//! place. So a move commits, then sends a `wl_display.sync`; when its callback
//! is done every event the compositor sent before it processed the commit has
//! been read, and [`WindowEvent::Moved`] is pushed into the same event stream
//! as the pointer's. An app that follows the pointer with the surface reads
//! each pointer position against the position the last `Moved` before it
//! reported.

use sctk::reexports::client::protocol::wl_callback::{Event as CallbackEvent, WlCallback};
use sctk::reexports::client::{Connection, Dispatch, QueueHandle};
use winit_core::event::WindowEvent;
use winit_core::window::WindowId;

use crate::state::WinitState;

/// The position a move put a layer surface at, physical px (the margins times
/// the scale at the move).
#[derive(Debug, Clone, Copy)]
pub struct LayerMoveBarrier {
    pub window_id: WindowId,
    pub position: dpi::PhysicalPosition<i32>,
}

impl Dispatch<WlCallback, LayerMoveBarrier, WinitState> for WinitState {
    fn event(
        state: &mut WinitState,
        _proxy: &WlCallback,
        event: CallbackEvent,
        data: &LayerMoveBarrier,
        _conn: &Connection,
        _qh: &QueueHandle<WinitState>,
    ) {
        if let CallbackEvent::Done { .. } = event {
            state.events_sink.push_window_event(WindowEvent::Moved(data.position), data.window_id);
        }
    }
}
