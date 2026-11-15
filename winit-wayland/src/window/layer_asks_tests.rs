//! The layer surface's size requests (see the module docs).

use super::*;

const FREE: (bool, bool) = (false, false);
/// A bar across an edge: the compositor assigns the width.
const BAR: (bool, bool) = (true, false);

fn s(width: u32, height: u32) -> LogicalSize<u32> {
    LogicalSize::new(width, height)
}

/// Sends `size` from `applied`, answering the generation it went out with.
fn sent(asks: &mut LayerAsks, size: LogicalSize<u32>, applied: LogicalSize<u32>) -> u64 {
    sent_on(asks, size, applied, FREE)
}

fn sent_on(
    asks: &mut LayerAsks,
    size: LogicalSize<u32>,
    applied: LogicalSize<u32>,
    assigned: (bool, bool),
) -> u64 {
    match asks.ask(size, applied, assigned) {
        Ask::Send(sent, generation) => {
            assert_eq!(sent, size);
            generation
        },
        other => panic!("expected {size:?} to go out, got {other:?}"),
    }
}

/// A configure (not the first) of `size` arrives on a surface of size `applied`.
fn configure(asks: &mut LayerAsks, size: LogicalSize<u32>, applied: LogicalSize<u32>) -> Step {
    asks.configured(size, applied, FREE, false)
}

/// Both syncs of request `generation`'s marker come back, the surface at `applied`: what
/// follows the end of the request.
fn end(asks: &mut LayerAsks, generation: u64, applied: LogicalSize<u32>) -> Step {
    end_on(asks, generation, applied, FREE)
}

fn end_on(
    asks: &mut LayerAsks,
    generation: u64,
    applied: LogicalSize<u32>,
    assigned: (bool, bool),
) -> Step {
    assert_eq!(asks.marker_done(generation, 1, applied, assigned), Marker::SyncAgain);
    match asks.marker_done(generation, 2, applied, assigned) {
        Marker::Ended(step) => {
            assert!(!asks.in_flight() || step.send.is_some(), "ended: {step:?}");
            step
        },
        other => panic!("expected request {generation} to end, got {other:?}"),
    }
}

/// The request a step sends: its size and its marker's generation.
fn goes_out(step: Step) -> (LogicalSize<u32>, u64) {
    let Some(Sent { size, generation }) = step.send else { panic!("a request goes out: {step:?}") };
    (size, generation)
}

const NOTHING: Step = Step { send: None, apply: None };

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

/// The review's grow then shrink: 396 then 60. 60 is held; the answer to 396 is only
/// remembered, and at 396's marker 60 goes out, applied at once; the answer to 60 changes
/// nothing. The application is told 396 (its own request) and 60, nothing else.
#[test]
fn grow_then_shrink_applies_only_the_newest_request() {
    let mut asks = LayerAsks::default();
    let first = sent(&mut asks, s(396, 36), s(60, 36));
    assert_eq!(asks.ask(s(60, 36), s(396, 36), FREE), Ask::Hold);
    assert_eq!(configure(&mut asks, s(396, 36), s(396, 36)), NOTHING);
    let step = end(&mut asks, first, s(396, 36));
    assert_eq!(step.apply, Some(s(60, 36)));
    let (size, second) = goes_out(step);
    assert_eq!(size, s(60, 36));
    assert_eq!(configure(&mut asks, s(60, 36), s(60, 36)), NOTHING);
    assert_eq!(end(&mut asks, second, s(60, 36)), NOTHING);
    assert!(!asks.in_flight());
}

/// The review's shrink then grow: 108 then 492. The answer to 108 is never applied over the
/// 492 request.
#[test]
fn shrink_then_grow_applies_only_the_newest_request() {
    let mut asks = LayerAsks::default();
    let first = sent(&mut asks, s(108, 36), s(396, 36));
    assert_eq!(asks.ask(s(492, 36), s(108, 36), FREE), Ask::Hold);
    assert_eq!(configure(&mut asks, s(108, 36), s(108, 36)), NOTHING);
    let step = end(&mut asks, first, s(108, 36));
    assert_eq!(step.apply, Some(s(492, 36)));
    let (size, second) = goes_out(step);
    assert_eq!(size, s(492, 36));
    assert_eq!(configure(&mut asks, s(492, 36), s(492, 36)), NOTHING);
    assert_eq!(end(&mut asks, second, s(492, 36)), NOTHING);
}

/// A held request replaced before the marker: only the newest goes out.
#[test]
fn only_the_newest_held_request_goes_out() {
    let mut asks = LayerAsks::default();
    let first = sent(&mut asks, s(396, 36), s(100, 36));
    assert_eq!(asks.ask(s(60, 36), s(396, 36), FREE), Ask::Hold);
    assert_eq!(asks.ask(s(80, 36), s(396, 36), FREE), Ask::Hold);
    configure(&mut asks, s(396, 36), s(396, 36));
    assert_eq!(goes_out(end(&mut asks, first, s(396, 36))).0, s(80, 36));
}

/// 396, then 60, then back to 396 before the marker: the held request is the one in flight,
/// whose answer equals it, so nothing goes out and nothing changes. And a held request
/// equal to the compositor's answer (another size than the one in flight) is that answer,
/// applied, with nothing sent.
#[test]
fn a_held_request_equal_to_the_answer_sends_nothing() {
    let mut asks = LayerAsks::default();
    let first = sent(&mut asks, s(396, 36), s(100, 36));
    assert_eq!(asks.ask(s(60, 36), s(396, 36), FREE), Ask::Hold);
    assert_eq!(asks.ask(s(396, 36), s(396, 36), FREE), Ask::Hold);
    configure(&mut asks, s(396, 36), s(396, 36));
    assert_eq!(end(&mut asks, first, s(396, 36)), NOTHING);
    assert!(!asks.in_flight());

    let first = sent(&mut asks, s(400, 36), s(396, 36));
    assert_eq!(asks.ask(s(350, 36), s(400, 36), FREE), Ask::Hold);
    configure(&mut asks, s(350, 36), s(400, 36));
    assert_eq!(end(&mut asks, first, s(400, 36)), Step { send: None, apply: Some(s(350, 36)) });
    assert!(!asks.in_flight());
}

/// The compositor really gives less (KWin's virtual keyboard): applied at the marker, the
/// request goes out once more, and when the compositor holds to its size, silent (KWin sends
/// no configure for the size of the client's buffer) or saying it again, that stands.
#[test]
fn a_real_smaller_grant_is_applied_asked_once_more_then_stands() {
    for again in [false, true] {
        let mut asks = LayerAsks::default();
        let first = sent(&mut asks, s(300, 300), s(300, 100));
        assert_eq!(configure(&mut asks, s(300, 200), s(300, 300)), NOTHING);
        let step = end(&mut asks, first, s(300, 300));
        assert_eq!(step.apply, Some(s(300, 200)));
        let (size, second) = goes_out(step);
        assert_eq!(size, s(300, 300));
        if again {
            assert_eq!(configure(&mut asks, s(300, 200), s(300, 200)), NOTHING);
        }
        assert_eq!(end(&mut asks, second, s(300, 200)), NOTHING, "again: {again}");
        assert!(!asks.in_flight());
    }
}

/// The compositor clamps a newer request to exactly the older answer, which was not applied
/// (the client had replaced it). The compositor sends it again for the newer request (wlroots
/// answers every request, KWin compares with the client's buffer, which never took it), and
/// then it is applied: the compositor's word is not lost. The newer request goes out once
/// more and, the compositor silent, the clamp stands.
#[test]
fn a_clamp_to_exactly_an_older_answer_comes_again_and_is_applied() {
    let mut asks = LayerAsks::default();
    let first = sent(&mut asks, s(400, 36), s(300, 36));
    assert_eq!(asks.ask(s(500, 36), s(400, 36), FREE), Ask::Hold);
    configure(&mut asks, s(400, 36), s(400, 36));
    let step = end(&mut asks, first, s(400, 36));
    assert_eq!(step.apply, Some(s(500, 36)));
    let (_, second) = goes_out(step);
    assert_eq!(configure(&mut asks, s(400, 36), s(500, 36)), NOTHING);
    let step = end(&mut asks, second, s(500, 36));
    assert_eq!(step.apply, Some(s(400, 36)));
    let (size, third) = goes_out(step);
    assert_eq!(size, s(500, 36));
    assert_eq!(end(&mut asks, third, s(400, 36)), NOTHING);
}

/// The compositor answers an older request, then is silent on the newer one (KWin, whose
/// size equals the client's buffer): that answer is not applied, and the size the surface
/// has stands.
#[test]
fn silence_after_a_replaced_answer_keeps_the_surfaces_size() {
    let mut asks = LayerAsks::default();
    let first = sent(&mut asks, s(396, 36), s(60, 36));
    assert_eq!(asks.ask(s(60, 36), s(396, 36), FREE), Ask::Hold);
    configure(&mut asks, s(396, 36), s(396, 36));
    let (_, second) = goes_out(end(&mut asks, first, s(396, 36)));
    assert_eq!(end(&mut asks, second, s(60, 36)), NOTHING);
    assert!(!asks.in_flight());
}

/// Silent on a request the surface took: it ends at the marker and nothing follows. Silent
/// on one the surface's size does not match: the request goes out once more, then stands.
#[test]
fn a_silent_compositor_ends_the_request_at_the_marker() {
    let mut asks = LayerAsks::default();
    let generation = sent(&mut asks, s(500, 36), s(400, 36));
    assert_eq!(end(&mut asks, generation, s(500, 36)), NOTHING);
    assert!(!asks.in_flight());

    let generation = sent(&mut asks, s(600, 36), s(500, 36));
    let step = end(&mut asks, generation, s(500, 36));
    assert_eq!(step.apply, None);
    let (_, generation) = goes_out(step);
    assert_eq!(end(&mut asks, generation, s(500, 36)), NOTHING);
}

/// A held request goes out when the marker ends the one in flight, applied at once: the size
/// changes outside a configure, which the window reports as a resize.
#[test]
fn a_held_request_goes_out_at_the_marker() {
    let mut asks = LayerAsks::default();
    let generation = sent(&mut asks, s(600, 36), s(500, 36));
    assert_eq!(asks.ask(s(700, 36), s(600, 36), FREE), Ask::Hold);
    let step = end(&mut asks, generation, s(600, 36));
    assert_eq!(step.apply, Some(s(700, 36)));
    assert_eq!(goes_out(step).0, s(700, 36));
}

/// A configure after the marker ended the request is the compositor's own word: applied, it
/// ends nothing and sends nothing.
#[test]
fn a_late_configure_after_the_marker_is_applied_and_ends_nothing() {
    let mut asks = LayerAsks::default();
    let generation = sent(&mut asks, s(500, 36), s(400, 36));
    assert_eq!(end(&mut asks, generation, s(500, 36)), NOTHING);
    assert_eq!(configure(&mut asks, s(520, 36), s(500, 36)), Step {
        send: None,
        apply: Some(s(520, 36))
    });
    assert!(!asks.in_flight());
}

/// A configure that crosses a newer request (from a compositor deferring past the marker)
/// is the newest remembered at its marker: applied, and the request goes out once more, so
/// the crossing costs one round trip.
#[test]
fn a_crossing_configure_costs_one_more_request() {
    let mut asks = LayerAsks::default();
    let first = sent(&mut asks, s(300, 36), s(400, 36));
    assert_eq!(configure(&mut asks, s(500, 36), s(300, 36)), NOTHING);
    let step = end(&mut asks, first, s(300, 36));
    assert_eq!(step.apply, Some(s(500, 36)));
    let (size, second) = goes_out(step);
    assert_eq!(size, s(300, 36));
    configure(&mut asks, s(300, 36), s(500, 36));
    assert_eq!(end(&mut asks, second, s(500, 36)), Step { send: None, apply: Some(s(300, 36)) });
}

/// A marker for a request that is not the one in flight (ended, or replaced by the one that
/// went out after it) is ignored.
#[test]
fn a_marker_for_another_request_is_ignored() {
    let mut asks = LayerAsks::default();
    let first = sent(&mut asks, s(500, 36), s(400, 36));
    assert_eq!(asks.ask(s(600, 36), s(500, 36), FREE), Ask::Hold);
    let (_, second) = goes_out(end(&mut asks, first, s(500, 36)));
    assert_eq!(asks.marker_done(first, 1, s(600, 36), FREE), Marker::Ignore);
    assert_eq!(asks.marker_done(first, 2, s(600, 36), FREE), Marker::Ignore);
    assert!(asks.in_flight(), "the request that went out is still in flight");
    assert_eq!(end(&mut asks, second, s(600, 36)), NOTHING);
    assert_eq!(asks.marker_done(second, 2, s(600, 36), FREE), Marker::Ignore);
}

/// An axis the compositor assigns is never compared: a bar whose width is the compositor's
/// asks only for its height, and an answer that changes the width alone is applied and
/// sends nothing again.
#[test]
fn an_assigned_axis_is_never_compared() {
    let mut asks = LayerAsks::default();
    assert_eq!(asks.ask(s(1280, 20), s(1024, 20), BAR), Ask::Nothing, "the width is theirs");
    let generation = sent_on(&mut asks, s(1024, 40), s(1024, 20), BAR);
    assert_eq!(asks.configured(s(1600, 40), s(1024, 40), BAR, false), NOTHING);
    assert_eq!(end_on(&mut asks, generation, s(1024, 40), BAR), Step {
        send: None,
        apply: Some(s(1600, 40))
    });
}

/// The first configure with nothing in flight (the first size goes out with the surface's
/// creation): applied, it ends nothing.
#[test]
fn the_first_configure_is_applied_and_ends_nothing() {
    let mut asks = LayerAsks::default();
    assert_eq!(asks.configured(s(60, 36), s(60, 36), FREE, true), Step {
        send: None,
        apply: Some(s(60, 36))
    });
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
    asks.ask(s(300, 36), s(500, 36), FREE);
    assert_eq!(asks.newest_request(), Some(s(300, 36)));
}

/// A held request going out is applied at once (the client draws at the size it asked
/// for); a request sent once more after the compositor gave another size is not (that
/// size stands until the compositor answers otherwise).
#[test]
fn a_held_request_is_applied_as_it_goes_out_and_a_resent_one_is_not() {
    let mut asks = LayerAsks::default();
    let first = sent(&mut asks, s(108, 36), s(396, 36));
    asks.ask(s(492, 36), s(108, 36), FREE);
    configure(&mut asks, s(108, 36), s(108, 36));
    let step = end(&mut asks, first, s(108, 36));
    assert_eq!(step.apply, Some(s(492, 36)), "the held request, applied as it goes out");
    let (_, second) = goes_out(step);
    configure(&mut asks, s(400, 36), s(492, 36));
    let step = end(&mut asks, second, s(492, 36));
    assert_eq!(goes_out(step).0, s(492, 36), "sent once more");
    assert_eq!(step.apply, Some(s(400, 36)), "the compositor's size, not the resent one");
}

/// Review of rinch PR 2703, trace 1 (sway): the map's configure, answering the first buffer
/// with the old size, arrives after the request for 156 was committed, and before the
/// compositor's answer to it. It must not end the request: applied, the old 60 became the
/// application's permanent limit. Only the newest configure before the marker counts, and
/// the surface is never told 60.
#[test]
fn a_late_map_configure_does_not_end_the_request() {
    let mut asks = LayerAsks::default();
    // The first configure, before any request: applied.
    assert_eq!(asks.configured(s(60, 36), s(60, 36), FREE, true).apply, Some(s(60, 36)));
    let request = sent(&mut asks, s(156, 36), s(60, 36));
    // The map's configure (old size), then the answer to the request.
    assert_eq!(configure(&mut asks, s(60, 36), s(156, 36)), NOTHING, "the map's configure");
    assert_eq!(configure(&mut asks, s(156, 36), s(156, 36)), NOTHING);
    assert_eq!(end(&mut asks, request, s(156, 36)), NOTHING);
    assert!(!asks.in_flight());
}

/// Review of rinch PR 2703, trace 2 (sway): sway reconfigures every layer surface on an
/// output when any one commits, so another surface's commit carries this surface's old size
/// while its request is in flight. That configure must not end the request either, also
/// with a newer request held behind it.
#[test]
fn another_surfaces_commit_reconfiguring_this_one_does_not_end_the_request() {
    let mut asks = LayerAsks::default();
    let request = sent(&mut asks, s(100, 60), s(100, 20));
    // The other surface's commit: this surface's old size. Then this request's answer.
    assert_eq!(configure(&mut asks, s(100, 20), s(100, 60)), NOTHING, "the other's commit");
    assert_eq!(configure(&mut asks, s(100, 60), s(100, 60)), NOTHING);
    assert_eq!(end(&mut asks, request, s(100, 60)), NOTHING);

    // Again with a newer request held: it goes out at the marker, and the old size from
    // the other surface's commit is never applied.
    let request = sent(&mut asks, s(100, 100), s(100, 60));
    assert_eq!(asks.ask(s(100, 40), s(100, 100), FREE), Ask::Hold);
    assert_eq!(configure(&mut asks, s(100, 60), s(100, 100)), NOTHING, "the other's commit");
    assert_eq!(configure(&mut asks, s(100, 100), s(100, 100)), NOTHING);
    let step = end(&mut asks, request, s(100, 100));
    assert_eq!(step.apply, Some(s(100, 40)));
    let (size, request) = goes_out(step);
    assert_eq!(size, s(100, 40));
    assert_eq!(configure(&mut asks, s(100, 100), s(100, 40)), NOTHING, "the other's commit");
    assert_eq!(configure(&mut asks, s(100, 40), s(100, 40)), NOTHING);
    assert_eq!(end(&mut asks, request, s(100, 40)), NOTHING);
}

/// KWin answers from a zero timer, after the marker's first `done` and before its second:
/// that answer counts.
#[test]
fn an_answer_between_the_two_syncs_counts() {
    let mut asks = LayerAsks::default();
    let request = sent(&mut asks, s(300, 36), s(200, 36));
    assert_eq!(asks.marker_done(request, 1, s(300, 36), FREE), Marker::SyncAgain);
    assert_eq!(configure(&mut asks, s(250, 36), s(300, 36)), NOTHING);
    let Marker::Ended(step) = asks.marker_done(request, 2, s(300, 36), FREE) else {
        panic!("ended");
    };
    assert_eq!(step.apply, Some(s(250, 36)));
    assert_eq!(goes_out(step).0, s(300, 36), "asked once more");
}

/// The corner the design leaves: KWin silent on the request (its size equals the client's
/// buffer) while a stale configure sent just before the commit is the newest remembered.
/// The stale size is applied, the request goes out once more, and KWin, the client's buffer
/// now the stale size, answers it: the surface ends at the newest request's size.
#[test]
fn a_stale_newest_configure_with_kwin_silent_costs_one_more_request() {
    let mut asks = LayerAsks::default();
    let request = sent(&mut asks, s(300, 36), s(200, 36));
    configure(&mut asks, s(200, 36), s(300, 36));
    let step = end(&mut asks, request, s(300, 36));
    assert_eq!(step.apply, Some(s(200, 36)));
    let (size, request) = goes_out(step);
    assert_eq!(size, s(300, 36));
    configure(&mut asks, s(300, 36), s(200, 36));
    assert_eq!(end(&mut asks, request, s(200, 36)), Step { send: None, apply: Some(s(300, 36)) });
}

/// The review's low item: a held request going out keeps the compositor's size on an axis
/// it assigns (a bar's width changed while the request was in flight), and does not put
/// back the width it had when the request was held.
#[test]
fn a_held_request_going_out_keeps_the_compositors_size_on_an_assigned_axis() {
    let mut asks = LayerAsks::default();
    let request = sent_on(&mut asks, s(1024, 40), s(1024, 20), BAR);
    assert_eq!(asks.ask(s(1024, 60), s(1024, 40), BAR), Ask::Hold);
    asks.configured(s(1600, 40), s(1024, 40), BAR, false);
    let step = end_on(&mut asks, request, s(1024, 40), BAR);
    assert_eq!(step.apply, Some(s(1600, 60)), "the compositor's width, the held height");
    assert_eq!(goes_out(step).0.height, 60);
}

/// The first configure arriving with a request in flight is applied at once on the axes the
/// compositor assigns (a bar's width, unknown until then), keeping the size asked for on the
/// client's; it is remembered like any other, and the newest before the marker is the answer.
#[test]
fn the_first_configure_with_a_request_in_flight_applies_the_assigned_axes() {
    let mut asks = LayerAsks::default();
    let request = sent_on(&mut asks, s(1, 40), s(1, 20), BAR);
    assert_eq!(asks.configured(s(1280, 20), s(1, 40), BAR, true), Step {
        send: None,
        apply: Some(s(1280, 40))
    });
    asks.configured(s(1280, 40), s(1280, 40), BAR, false);
    assert_eq!(end_on(&mut asks, request, s(1280, 40), BAR), NOTHING);
}

/// With nothing in flight, a configure of the surface's own size changes nothing (sway
/// reconfiguring every surface when another commits).
#[test]
fn a_configure_of_the_surfaces_size_with_nothing_in_flight_changes_nothing() {
    let mut asks = LayerAsks::default();
    assert_eq!(configure(&mut asks, s(60, 36), s(60, 36)), NOTHING);
}
