//! The layer surface's size requests (see the module docs).

use super::*;

const FREE: (bool, bool) = (false, false);

fn s(width: u32, height: u32) -> LogicalSize<u32> {
    LogicalSize::new(width, height)
}

/// Sends `size` from `applied`, answering the generation it went out with.
fn sent(asks: &mut LayerAsks, size: LogicalSize<u32>, applied: LogicalSize<u32>) -> u64 {
    match asks.ask(size, applied, FREE) {
        Ask::Send(sent, generation) => {
            assert_eq!(sent, size);
            generation
        },
        other => panic!("expected {size:?} to go out, got {other:?}"),
    }
}

fn applies(size: LogicalSize<u32>) -> Step {
    Step { apply: Some(size), send: None }
}

/// Toplevels and popups are resized at once, as upstream: only a layer surface asks, one
/// request in flight.
#[test]
fn only_a_layer_surface_asks_for_its_size() {
    assert!(asks_for_its_size(WaylandSurfaceRole::LayerShell));
    assert!(!asks_for_its_size(WaylandSurfaceRole::XdgToplevel));
}

#[test]
fn a_request_with_nothing_in_flight_goes_out_at_once() {
    let mut asks = LayerAsks::default();
    sent(&mut asks, s(396, 36), s(60, 36));
    assert!(asks.in_flight());
}

#[test]
fn a_request_for_the_applied_size_sends_nothing() {
    let mut asks = LayerAsks::default();
    assert_eq!(asks.ask(s(60, 36), s(60, 36), FREE), Ask::Nothing);
    assert!(!asks.in_flight());
}

/// The review's grow then shrink: 396 then 60. 60 is held; the answer to 396 sends it and
/// is not applied (the client replaced 396); the answer to 60 is applied. The application
/// is told 60 alone.
#[test]
fn grow_then_shrink_applies_only_the_answer_to_the_newest_request() {
    let mut asks = LayerAsks::default();
    sent(&mut asks, s(396, 36), s(60, 36));
    assert_eq!(asks.ask(s(60, 36), s(60, 36), FREE), Ask::Hold);
    let step = asks.configured(s(396, 36), FREE);
    assert_eq!(step.apply, None, "the answer to a replaced request");
    assert!(matches!(step.send, Some(Sent { size: sz, .. }) if sz == s(60, 36)));
    assert_eq!(asks.configured(s(60, 36), FREE), applies(s(60, 36)));
    assert!(!asks.in_flight());
}

/// The review's shrink then grow: 108 then 492. The answer to 108 is never applied, so the
/// application never sees 108 against its 492 request.
#[test]
fn shrink_then_grow_applies_only_the_answer_to_the_newest_request() {
    let mut asks = LayerAsks::default();
    sent(&mut asks, s(108, 36), s(396, 36));
    assert_eq!(asks.ask(s(492, 36), s(396, 36), FREE), Ask::Hold);
    let step = asks.configured(s(108, 36), FREE);
    assert_eq!(step.apply, None);
    assert!(matches!(step.send, Some(Sent { size: sz, .. }) if sz == s(492, 36)));
    assert_eq!(asks.configured(s(492, 36), FREE), applies(s(492, 36)));
}

/// A held request replaced before the answer: only the newest goes out.
#[test]
fn only_the_newest_held_request_goes_out() {
    let mut asks = LayerAsks::default();
    sent(&mut asks, s(396, 36), s(100, 36));
    assert_eq!(asks.ask(s(60, 36), s(100, 36), FREE), Ask::Hold);
    assert_eq!(asks.ask(s(80, 36), s(100, 36), FREE), Ask::Hold);
    let step = asks.configured(s(396, 36), FREE);
    assert!(matches!(step.send, Some(Sent { size: sz, .. }) if sz == s(80, 36)));
}

/// 396, then 60, then back to 396 before the first answer: the held request is the
/// answer's size, so that answer is applied and nothing goes out.
#[test]
fn a_held_request_equal_to_the_answer_sends_nothing() {
    let mut asks = LayerAsks::default();
    sent(&mut asks, s(396, 36), s(100, 36));
    assert_eq!(asks.ask(s(60, 36), s(100, 36), FREE), Ask::Hold);
    assert_eq!(asks.ask(s(396, 36), s(100, 36), FREE), Ask::Hold);
    assert_eq!(asks.configured(s(396, 36), FREE), applies(s(396, 36)));
    assert!(!asks.in_flight());
}

/// The compositor really gives less (KWin's virtual keyboard): applied, the request goes
/// out once more, and when the compositor holds to its size (silent: KWin sends no
/// configure for its last size) that stands.
#[test]
fn a_real_smaller_grant_is_applied_asked_once_more_then_stands() {
    let mut asks = LayerAsks::default();
    sent(&mut asks, s(300, 300), s(300, 100));
    let step = asks.configured(s(300, 200), FREE);
    assert_eq!(step.apply, Some(s(300, 200)));
    let Some(Sent { size: sz, generation, .. }) = step.send else {
        panic!("asked once more: {step:?}")
    };
    assert_eq!(sz, s(300, 300));
    assert_eq!(asks.marker_done(generation, 1, s(300, 200), FREE), Marker::SyncAgain);
    assert_eq!(asks.marker_done(generation, 2, s(300, 200), FREE), Marker::Ended(Step::default()));
    assert!(!asks.in_flight());
}

/// The compositor clamps a newer request to exactly the older answer, which was not
/// applied (the client had replaced it). The surface never took that size, so the
/// compositor sends it again for the newer request (KWin compares with the client's
/// buffer; wlroots answers every request), and then it is applied: the compositor's word
/// is not lost. The newer request goes out once more and, the compositor silent, stands.
#[test]
fn a_clamp_to_exactly_an_older_answer_comes_again_and_is_applied() {
    let mut asks = LayerAsks::default();
    sent(&mut asks, s(400, 36), s(300, 36));
    assert_eq!(asks.ask(s(500, 36), s(300, 36), FREE), Ask::Hold);
    let step = asks.configured(s(400, 36), FREE);
    assert_eq!(step.apply, None);
    assert!(matches!(step.send, Some(Sent { size: sz, .. }) if sz == s(500, 36)), "{step:?}");
    let step = asks.configured(s(400, 36), FREE);
    assert_eq!(step.apply, Some(s(400, 36)));
    let Some(Sent { size: sz, generation, .. }) = step.send else {
        panic!("asked once more: {step:?}")
    };
    assert_eq!(sz, s(500, 36));
    assert_eq!(asks.marker_done(generation, 1, s(400, 36), FREE), Marker::SyncAgain);
    assert_eq!(asks.marker_done(generation, 2, s(400, 36), FREE), Marker::Ended(Step::default()));
}

/// The compositor answers an older request, then is silent on the newer one (KWin, whose
/// size equals the client's buffer, which never took the older answer): that answer is not
/// applied, and the surface's size stands.
#[test]
fn silence_after_a_replaced_answer_keeps_the_surfaces_size() {
    let mut asks = LayerAsks::default();
    sent(&mut asks, s(396, 36), s(60, 36));
    assert_eq!(asks.ask(s(60, 36), s(60, 36), FREE), Ask::Hold);
    let step = asks.configured(s(396, 36), FREE);
    assert_eq!(step.apply, None);
    let Some(Sent { generation, .. }) = step.send else { panic!("60 goes out: {step:?}") };
    assert_eq!(asks.marker_done(generation, 1, s(60, 36), FREE), Marker::SyncAgain);
    assert_eq!(asks.marker_done(generation, 2, s(60, 36), FREE), Marker::Ended(Step::default()));
    assert!(!asks.in_flight());
}

/// Silent on a request with no earlier answer kept: the applied size stands, the request
/// goes out once more, then stands.
#[test]
fn a_silent_compositor_ends_the_request_at_the_marker() {
    let mut asks = LayerAsks::default();
    let generation = sent(&mut asks, s(500, 36), s(400, 36));
    assert_eq!(asks.marker_done(generation, 1, s(400, 36), FREE), Marker::SyncAgain);
    let Marker::Ended(step) = asks.marker_done(generation, 2, s(400, 36), FREE) else {
        panic!("ended");
    };
    assert_eq!(step.apply, None);
    let Some(Sent { generation, .. }) = step.send else { panic!("once more") };
    assert_eq!(asks.marker_done(generation, 1, s(400, 36), FREE), Marker::SyncAgain);
    assert_eq!(asks.marker_done(generation, 2, s(400, 36), FREE), Marker::Ended(Step::default()));
}

/// A held request goes out when the marker ends the one in flight, applied at once: the
/// size changes outside a configure, which the window reports as a resize.
#[test]
fn a_held_request_goes_out_at_the_marker() {
    let mut asks = LayerAsks::default();
    let generation = sent(&mut asks, s(600, 36), s(500, 36));
    assert_eq!(asks.ask(s(700, 36), s(500, 36), FREE), Ask::Hold);
    assert_eq!(asks.marker_done(generation, 1, s(500, 36), FREE), Marker::SyncAgain);
    let Marker::Ended(step) = asks.marker_done(generation, 2, s(500, 36), FREE) else {
        panic!("ended");
    };
    assert_eq!(step.apply, None);
    assert!(
        matches!(step.send, Some(Sent { size: sz, apply: true, .. }) if sz == s(700, 36)),
        "{step:?}"
    );
}

/// A configure after the marker ended the request: applied, it ends nothing and sends
/// nothing.
#[test]
fn a_late_configure_after_the_marker_is_applied_and_ends_nothing() {
    let mut asks = LayerAsks::default();
    let generation = sent(&mut asks, s(500, 36), s(400, 36));
    assert_eq!(asks.marker_done(generation, 1, s(400, 36), FREE), Marker::SyncAgain);
    let Marker::Ended(Step { send: Some(Sent { generation, .. }), .. }) =
        asks.marker_done(generation, 2, s(400, 36), FREE)
    else {
        panic!("asked once more");
    };
    assert_eq!(asks.marker_done(generation, 1, s(400, 36), FREE), Marker::SyncAgain);
    assert_eq!(asks.marker_done(generation, 2, s(400, 36), FREE), Marker::Ended(Step::default()));
    assert_eq!(asks.configured(s(500, 36), FREE), applies(s(500, 36)));
    assert!(!asks.in_flight());
}

/// A configure that crosses a newer request (from a compositor deferring past the marker)
/// ends it with another size: applied, and the request goes out once more, so the crossing
/// costs one round trip.
#[test]
fn a_crossing_configure_costs_one_more_request() {
    let mut asks = LayerAsks::default();
    sent(&mut asks, s(300, 36), s(400, 36));
    let step = asks.configured(s(500, 36), FREE);
    assert_eq!(step.apply, Some(s(500, 36)));
    assert!(matches!(step.send, Some(Sent { size: sz, .. }) if sz == s(300, 36)), "{step:?}");
    assert_eq!(asks.configured(s(300, 36), FREE), applies(s(300, 36)));
}

/// A marker for a request a configure already ended is ignored.
#[test]
fn a_marker_for_an_ended_request_is_ignored() {
    let mut asks = LayerAsks::default();
    let generation = sent(&mut asks, s(500, 36), s(400, 36));
    assert_eq!(asks.configured(s(500, 36), FREE), applies(s(500, 36)));
    assert_eq!(asks.marker_done(generation, 1, s(500, 36), FREE), Marker::Ignore);
    assert_eq!(asks.marker_done(generation, 2, s(500, 36), FREE), Marker::Ignore);
}

/// An axis the compositor assigns is never compared: a bar whose width is the compositor's
/// asks only for its height, and a configure that changes the width alone sends nothing
/// again.
#[test]
fn an_assigned_axis_is_never_compared() {
    let bar = (true, false);
    let mut asks = LayerAsks::default();
    assert_eq!(asks.ask(s(1280, 20), s(1024, 20), bar), Ask::Nothing, "the width is theirs");
    let Ask::Send(..) = asks.ask(s(1024, 40), s(1024, 20), bar) else {
        panic!("a new height goes out");
    };
    assert_eq!(asks.configured(s(1600, 40), bar), applies(s(1600, 40)));
}

/// The first configure finds nothing in flight (the first size goes out with the surface's
/// creation): applied, it ends nothing.
#[test]
fn the_first_configure_is_applied_and_ends_nothing() {
    let mut asks = LayerAsks::default();
    assert_eq!(asks.configured(s(60, 36), FREE), applies(s(60, 36)));
    assert!(!asks.in_flight());
}

/// A `set_size` sent for another reason carries the newest request: the held one, else the
/// one in flight, else none (the applied size).
#[test]
fn the_newest_request_is_the_held_else_the_one_in_flight() {
    let mut asks = LayerAsks::default();
    assert_eq!(asks.newest_request(), None);
    sent(&mut asks, s(500, 36), s(400, 36));
    assert_eq!(asks.newest_request(), Some(s(500, 36)));
    asks.ask(s(300, 36), s(400, 36), FREE);
    assert_eq!(asks.newest_request(), Some(s(300, 36)));
}

/// A held request going out is applied at once (the client draws at the size it asked
/// for); a request sent once more after the compositor gave another size is not (that size
/// stands until the compositor answers otherwise).
#[test]
fn a_held_request_is_applied_as_it_goes_out_and_a_resent_one_is_not() {
    let mut asks = LayerAsks::default();
    sent(&mut asks, s(108, 36), s(396, 36));
    asks.ask(s(492, 36), s(108, 36), FREE);
    let step = asks.configured(s(108, 36), FREE);
    assert!(matches!(step.send, Some(Sent { apply: true, .. })), "{step:?}");
    let step = asks.configured(s(400, 36), FREE);
    assert!(matches!(step.send, Some(Sent { apply: false, .. })), "{step:?}");
}
