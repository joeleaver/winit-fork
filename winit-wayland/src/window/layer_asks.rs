//! A layer surface's size requests, one in flight at a time (rinch patch).
//!
//! A layer surface's size is the client's on the axes the compositor does not
//! assign: the client sends `set_size`, commits, and the compositor answers
//! with a configure. The protocol orders configures but does not say which
//! commit a configure answers, and compositors differ:
//!
//! - wlroots (sway, labwc) sends one configure for every commit that carries layer state, from the
//!   commit handler, before it reads anything else the client sent, and never changes a size the
//!   client set.
//! - KWin rearranges on a zero timer, so its configure comes in a later pass of its loop (after a
//!   `wl_display.sync` sent behind the commit has been answered); it sends none when the size it
//!   computes equals the size of the client's committed buffer (or of a configure not yet
//!   acknowledged); and it can make a client-sized axis smaller (both-anchored margins, a virtual
//!   keyboard).
//!
//! With two requests outstanding, the answer to the older one can arrive after
//! the newer one was made, and nothing in it says it is old. So a layer
//! surface keeps at most one request in flight: a request made while one is
//! unanswered is held (a newer one replaces it), and goes out when the one in
//! flight ends. Nothing is matched by size.
//!
//! A request does not change the surface's size: a configure does (the
//! compositor's word, applied without sending `set_size` back). The one
//! exception is a configure that ends a request while a newer one is held:
//! that newer request goes out at once, and the configure, the answer to a
//! request the client has replaced, is not applied. Nothing is lost by that:
//! a compositor that holds to that size answers the newer request with it
//! again (wlroots answers every request; KWin compares its size with the
//! client's buffer, which never took the replaced answer). So the application
//! is told only the answer to its newest request, or the compositor's own
//! size, and a compositor's silence means the size the surface has suits it.
//!
//! The request in flight ends at the next configure, or, when none comes, at
//! a marker: a `wl_display.sync` sent behind the commit and, when it is done,
//! a second one. wlroots answers before the first is done; KWin's zero timer
//! has run before it reads the second, so a configure it was going to send
//! precedes the second `done`.
//!
//! When the request in flight ends with the compositor's size different from
//! it on an axis the client sizes, and nothing is held, it is sent once more:
//! a configure that crossed it (from a compositor that defers further than the
//! marker) costs one round trip, never a size stuck at an old answer. A second
//! end with the size still different is the compositor's word, and stands.

use dpi::LogicalSize;

use crate::window::layer_shell::WaylandSurfaceRole;

/// Whether a surface of `role` asks the compositor for a new size, one request in flight,
/// and takes the size its configure gives (a layer surface), rather than resizing at once
/// as upstream does (a toplevel, and a popup, whose role is reported as a toplevel's).
pub fn asks_for_its_size(role: WaylandSurfaceRole) -> bool {
    role == WaylandSurfaceRole::LayerShell
}

/// What to do with a size request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// Send `set_size` for this size, commit, and start its marker with this
    /// generation (stage 1).
    Send(LogicalSize<u32>, u64),
    /// Kept until the request in flight ends.
    Hold,
    /// Nothing to send: the size is the compositor's already.
    Nothing,
}

/// What follows a configure, or a marker that ended a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// The size to apply now (the compositor's word on the newest request, or its own).
    pub apply: Option<LogicalSize<u32>>,
    /// A request to send now, with its marker generation.
    pub send: Option<(LogicalSize<u32>, u64)>,
}

/// What a marker's `done` leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// The first sync is done: send the second, with the same generation.
    SyncAgain,
    /// The second is done and no configure came: the request ended.
    Ended(Step),
    /// For a request that already ended (at a configure): nothing.
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InFlight {
    size: LogicalSize<u32>,
    generation: u64,
    /// Sent again after it ended with a different size from the compositor.
    resent: bool,
}

/// One layer surface's size requests (see the module docs).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LayerAsks {
    in_flight: Option<InFlight>,
    held: Option<LogicalSize<u32>>,
    generation: u64,
}

/// Whether `a` and `b` are the same size on the axes the client sizes;
/// `assigned` names the axes the compositor assigns (width, height).
fn same_on_client_axes(a: LogicalSize<u32>, b: LogicalSize<u32>, assigned: (bool, bool)) -> bool {
    (assigned.0 || a.width == b.width) && (assigned.1 || a.height == b.height)
}

impl LayerAsks {
    /// The client asks for `size`; `applied` is the size the surface has.
    pub fn ask(
        &mut self,
        size: LogicalSize<u32>,
        applied: LogicalSize<u32>,
        assigned: (bool, bool),
    ) -> Ask {
        if self.in_flight.is_some() {
            self.held = Some(size);
            return Ask::Hold;
        }
        if same_on_client_axes(size, applied, assigned) {
            return Ask::Nothing;
        }
        let (size, generation) = self.start(size, false);
        Ask::Send(size, generation)
    }

    /// A configure of `size` arrived.
    pub fn configured(&mut self, size: LogicalSize<u32>, assigned: (bool, bool)) -> Step {
        let Some(done) = self.in_flight.take() else {
            // A late configure, or the compositor's own: its word, applied; it ends nothing.
            return Step { apply: Some(size), send: None };
        };
        match self.held.take() {
            // A newer request waits: it goes out, and this answer to the replaced one is
            // not applied.
            Some(next) if !same_on_client_axes(next, size, assigned) => {
                Step { apply: None, send: Some(self.start(next, false)) }
            },
            // The newer request is this size already.
            Some(_) => Step { apply: Some(size), send: None },
            None if !done.resent && !same_on_client_axes(done.size, size, assigned) => {
                Step { apply: Some(size), send: Some(self.start(done.size, true)) }
            },
            None => Step { apply: Some(size), send: None },
        }
    }

    /// A marker's sync is done: `stage` 1 or 2 of the request `generation`;
    /// `applied` is the surface's size.
    pub fn marker_done(
        &mut self,
        generation: u64,
        stage: u8,
        applied: LogicalSize<u32>,
        assigned: (bool, bool),
    ) -> Marker {
        match self.in_flight {
            Some(f) if f.generation == generation => {},
            _ => return Marker::Ignore,
        }
        if stage == 1 {
            return Marker::SyncAgain;
        }
        let done = self.in_flight.take().expect("in flight, above");
        // The compositor said nothing on this request: the size the surface has suits it.
        let send = match self.held.take() {
            Some(next) if !same_on_client_axes(next, applied, assigned) => {
                Some(self.start(next, false))
            },
            Some(_) => None,
            None if !done.resent && !same_on_client_axes(done.size, applied, assigned) => {
                Some(self.start(done.size, true))
            },
            None => None,
        };
        Marker::Ended(Step { apply: None, send })
    }

    /// Whether a request is in flight.
    pub fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The newest size the client asked for and the compositor has not answered (held, else
    /// in flight): what a `set_size` sent for another reason (new anchors) must carry, so it
    /// does not replace the request with an older size.
    pub fn newest_request(&self) -> Option<LogicalSize<u32>> {
        self.held.or(self.in_flight.map(|f| f.size))
    }

    fn start(&mut self, size: LogicalSize<u32>, resent: bool) -> (LogicalSize<u32>, u64) {
        self.generation += 1;
        self.in_flight = Some(InFlight { size, generation: self.generation, resent });
        (size, self.generation)
    }
}

#[cfg(test)]
#[path = "layer_asks_tests.rs"]
mod tests;
