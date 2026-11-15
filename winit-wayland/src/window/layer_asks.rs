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
//! A configure can also answer an earlier commit than the newest. wlroots sends one for
//! every commit with layer state, and reconfigures every layer surface on an output when
//! any one of them commits, so a configure carrying the surface's old size can arrive
//! after a new request was committed: the map's configure (answering the first buffer),
//! or one caused by another surface's commit. Nothing in it says it is old.
//!
//! So a layer surface keeps at most one request in flight, and a request ends only at its
//! marker, never at a configure:
//!
//! - A request made while one is in flight is held; a newer one replaces it.
//! - A request that goes out is applied at once (the client sizes those axes, and draws its next
//!   frame at the size it asked for), after its `set_size` is committed, so the commit carries no
//!   state for a buffer not drawn yet.
//! - The marker is a `wl_display.sync` sent behind the commit and, when it is done, a second one.
//! - While a request is in flight, a configure is remembered, not applied; only the newest is kept.
//!   At the marker it is the compositor's answer: wlroots sends its answer from the commit handler,
//!   so it is the last configure before the first `done`, after any stale one; KWin sends its
//!   answer from a zero timer, after the first `done` and before the second.
//! - With nothing in flight, a configure is the compositor's own word, applied at once (with no
//!   `set_size` back). The very first configure is always applied, on the axes the compositor
//!   assigns when a request is in flight.
//!
//! At the marker:
//!
//! - A held request (other than the one in flight) goes out, applied at once; the answer to the
//!   replaced request is not applied on the client's axes, only on the axes the compositor assigns.
//!   A compositor that holds to a size answers the newer request with it again.
//! - Else, an answer equal to the request changes nothing; one that differs on an axis the client
//!   sizes is applied, and the request goes out once more (not applied). A second end at another
//!   size than asked stands. This also covers the one corner left: KWin silent on the request (its
//!   size equals the client's buffer) while a stale configure sent just before the commit is the
//!   newest remembered; applied, the request sent once more is then answered.
//! - Silence (no configure since the request went out) means the size the surface has suits the
//!   compositor.

use dpi::LogicalSize;

use crate::window::layer_shell::WaylandSurfaceRole;

/// Whether a surface of `role` asks the compositor for a new size, one request in flight,
/// holding newer ones (a layer surface), rather than resizing at once as upstream does (a
/// toplevel, and a popup, whose role is reported as a toplevel's).
pub fn asks_for_its_size(role: WaylandSurfaceRole) -> bool {
    role == WaylandSurfaceRole::LayerShell
}

/// What to do with a size request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// Send `set_size` for this size, commit, apply it, and start its marker with this
    /// generation (stage 1).
    Send(LogicalSize<u32>, u64),
    /// Kept until the request in flight ends.
    Hold,
    /// Nothing to send: the size is the surface's already.
    Nothing,
}

/// What follows a configure, or a marker that ended a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// A request to send now: its `set_size` is sent and committed before `apply`.
    pub send: Option<Sent>,
    /// The size to apply now, when it is not the surface's already: a held request going
    /// out, the compositor's answer to the newest request, or its own word.
    pub apply: Option<LogicalSize<u32>>,
}

/// A request sent after a configure or a marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    pub size: LogicalSize<u32>,
    /// Its marker's generation.
    pub generation: u64,
}

/// What a marker's `done` leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// The first sync is done: send the second, with the same generation.
    SyncAgain,
    /// The second is done: the request ended.
    Ended(Step),
    /// For a request that is not the one in flight: nothing.
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
    /// The newest configure since the request in flight went out.
    answer: Option<LogicalSize<u32>>,
    generation: u64,
}

/// Whether `a` and `b` are the same size on the axes the client sizes;
/// `assigned` names the axes the compositor assigns (width, height).
fn same_on_client_axes(a: LogicalSize<u32>, b: LogicalSize<u32>, assigned: (bool, bool)) -> bool {
    (assigned.0 || a.width == b.width) && (assigned.1 || a.height == b.height)
}

/// `client` on the axes the client sizes, `compositor` on the axes it assigns.
fn with_assigned_axes(
    client: LogicalSize<u32>,
    compositor: LogicalSize<u32>,
    assigned: (bool, bool),
) -> LogicalSize<u32> {
    LogicalSize::new(
        if assigned.0 { compositor.width } else { client.width },
        if assigned.1 { compositor.height } else { client.height },
    )
}

/// `size`, when it is not `applied`.
fn change(size: LogicalSize<u32>, applied: LogicalSize<u32>) -> Option<LogicalSize<u32>> {
    (size != applied).then_some(size)
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
        let sent = self.start(size, false);
        Ask::Send(sent.size, sent.generation)
    }

    /// A configure of `size` arrived; `applied` is the surface's size, and `first` says
    /// this is the surface's first configure.
    ///
    /// With a request in flight it is remembered, not applied: it may answer an earlier
    /// commit, and the request ends at its marker. The first configure is applied anyway,
    /// on the axes the compositor assigns (the client's axes keep the size asked for).
    /// With nothing in flight it is the compositor's word, applied.
    pub fn configured(
        &mut self,
        size: LogicalSize<u32>,
        applied: LogicalSize<u32>,
        assigned: (bool, bool),
        first: bool,
    ) -> Step {
        if self.in_flight.is_some() {
            self.answer = Some(size);
            let apply = first.then(|| with_assigned_axes(applied, size, assigned));
            return Step { send: None, apply };
        }
        Step { send: None, apply: change(size, applied).or(first.then_some(size)) }
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
        let answer = self.answer.take();
        // A held request for the size in flight is that request.
        let held = self.held.take().filter(|next| !same_on_client_axes(*next, done.size, assigned));
        let step = match (held, answer) {
            // The compositor's answer is the newer request's size already.
            (Some(next), Some(answer)) if same_on_client_axes(next, answer, assigned) => {
                Step { send: None, apply: change(answer, applied) }
            },
            // A newer request goes out, applied at once; the answer to the replaced one
            // counts only on the axes the compositor assigns.
            (Some(next), answer) => {
                let sent = self.start(next, false);
                let size = with_assigned_axes(next, answer.unwrap_or(applied), assigned);
                Step { send: Some(sent), apply: change(size, applied) }
            },
            // The answer to the newest request: equal on the client's axes, nothing more
            // than the axes the compositor assigns changes.
            (None, Some(answer)) if same_on_client_axes(answer, done.size, assigned) => {
                Step { send: None, apply: change(answer, applied) }
            },
            // Another size than asked: applied, and asked once more (a configure that
            // crossed the request, or a stale one KWin left newest by staying silent); a
            // second end at another size stands.
            (None, Some(answer)) => Step {
                send: (!done.resent).then(|| self.start(done.size, true)),
                apply: change(answer, applied),
            },
            // Silent: the size the surface has suits the compositor. It differs from the
            // request only after a resend, or when the surface's size changed under it.
            (None, None) if !done.resent && !same_on_client_axes(done.size, applied, assigned) => {
                Step { send: Some(self.start(done.size, true)), apply: None }
            },
            (None, None) => Step::default(),
        };
        Marker::Ended(step)
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

    fn start(&mut self, size: LogicalSize<u32>, resent: bool) -> Sent {
        self.generation += 1;
        self.in_flight = Some(InFlight { size, generation: self.generation, resent });
        self.answer = None;
        Sent { size, generation: self.generation }
    }
}

#[cfg(test)]
#[path = "layer_asks_tests.rs"]
mod tests;
