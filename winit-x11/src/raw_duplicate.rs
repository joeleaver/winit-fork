//! Dropping the second copy of a raw XInput2 pointer event while this client holds a pointer
//! grab (rinch #2114).
//!
//! winit selects `XI_RawMotion` / `XI_RawButtonPress` / `XI_RawButtonRelease` on the root window
//! for all master devices. `CursorGrabMode::Confined` is an `XGrabPointer` on the window itself,
//! with `owner_events` set. With that grab active the X server delivers every raw pointer event
//! to this client **twice** (`dix/events.c`, `DeliverRawEvent`):
//!
//! 1. `DeliverGrabbedEvent` → the `ownerEvents` path → `DeliverDeviceEvents`, which walks from the
//!    sprite window up to the root and finds this client's raw selection there;
//! 2. the loop over root windows that follows, because `FilterRawEvents` suppresses that copy only
//!    when `grab->window == root`, and winit grabs on its own window.
//!
//! The two copies are identical (device, source, time, detail, flags, valuators) and arrive back
//! to back, since both are written during one `DeliverRawEvent` call. So while a grab is held,
//! the filter passes the first event of each identical pair and drops the second. It pairs
//! events rather than dropping every repeat, so two genuine identical events within the same
//! millisecond (a high polling-rate mouse moving steadily) arrive as four copies and come out as
//! two. Without a grab it passes everything, so ungrabbed delivery is unchanged.

use x11_dl::xinput2::XIRawEvent;
use x11_dl::xlib::Time;

/// The fields of a raw event that two copies of one delivery share.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawEventView<'a> {
    pub evtype: i32,
    pub deviceid: i32,
    pub sourceid: i32,
    pub detail: i32,
    pub flags: i32,
    pub time: Time,
    pub mask: &'a [u8],
    pub values: &'a [f64],
    pub raw_values: &'a [f64],
}

impl<'a> RawEventView<'a> {
    /// Reads the view out of an `XIRawEvent`.
    ///
    /// # Safety
    ///
    /// `xev` must be a raw event as Xlib delivers it: `valuators.mask` points at
    /// `valuators.mask_len` bytes (or `mask_len` is 0), and `valuators.values` and `raw_values`
    /// each point at one `f64` per bit set in that mask.
    pub(crate) unsafe fn from_xi(evtype: i32, xev: &'a XIRawEvent) -> Self {
        let mask: &[u8] = if xev.valuators.mask_len <= 0 || xev.valuators.mask.is_null() {
            &[]
        } else {
            unsafe {
                std::slice::from_raw_parts(xev.valuators.mask, xev.valuators.mask_len as usize)
            }
        };
        let count = mask.iter().map(|b| b.count_ones() as usize).sum::<usize>();
        let read = |p: *const f64| -> &'a [f64] {
            if count == 0 || p.is_null() {
                &[]
            } else {
                unsafe { std::slice::from_raw_parts(p, count) }
            }
        };
        RawEventView {
            evtype,
            deviceid: xev.deviceid,
            sourceid: xev.sourceid,
            detail: xev.detail,
            flags: xev.flags,
            time: xev.time,
            mask,
            values: read(xev.valuators.values),
            raw_values: read(xev.raw_values),
        }
    }
}

/// Passes the first of each identical back-to-back pair of raw pointer events while a pointer
/// grab is held, and drops the second. See the module docs.
#[derive(Debug, Default)]
pub(crate) struct RawDuplicateFilter {
    /// Whether the fields below hold an event whose second copy may come next.
    pending: bool,
    evtype: i32,
    deviceid: i32,
    sourceid: i32,
    detail: i32,
    flags: i32,
    time: Time,
    mask: Vec<u8>,
    values: Vec<u64>,
    raw_values: Vec<u64>,
}

impl RawDuplicateFilter {
    /// Returns `true` when `ev` is the second copy of the event just passed and must be dropped.
    pub(crate) fn is_duplicate(&mut self, ev: &RawEventView<'_>, pointer_grabbed: bool) -> bool {
        if !pointer_grabbed {
            self.pending = false;
            return false;
        }
        if self.pending && self.matches(ev) {
            self.pending = false;
            return true;
        }
        self.store(ev);
        self.pending = true;
        false
    }

    /// Forgets the pending event: something else came in between, so the next event is not its
    /// second copy.
    pub(crate) fn interrupt(&mut self) {
        self.pending = false;
    }

    fn matches(&self, ev: &RawEventView<'_>) -> bool {
        self.evtype == ev.evtype
            && self.deviceid == ev.deviceid
            && self.sourceid == ev.sourceid
            && self.detail == ev.detail
            && self.flags == ev.flags
            && self.time == ev.time
            && self.mask == ev.mask
            && bits_eq(&self.values, ev.values)
            && bits_eq(&self.raw_values, ev.raw_values)
    }

    fn store(&mut self, ev: &RawEventView<'_>) {
        self.evtype = ev.evtype;
        self.deviceid = ev.deviceid;
        self.sourceid = ev.sourceid;
        self.detail = ev.detail;
        self.flags = ev.flags;
        self.time = ev.time;
        self.mask.clear();
        self.mask.extend_from_slice(ev.mask);
        self.values.clear();
        self.values.extend(ev.values.iter().map(|v| v.to_bits()));
        self.raw_values.clear();
        self.raw_values.extend(ev.raw_values.iter().map(|v| v.to_bits()));
    }
}

fn bits_eq(stored: &[u64], values: &[f64]) -> bool {
    stored.len() == values.len() && stored.iter().zip(values).all(|(a, b)| *a == b.to_bits())
}

#[cfg(test)]
mod tests {
    use x11_dl::xinput2::{self, XIValuatorState};

    use super::*;

    const MASK_XY: [u8; 1] = [0b11];

    fn motion<'a>(time: Time, raw: &'a [f64]) -> RawEventView<'a> {
        RawEventView {
            evtype: xinput2::XI_RawMotion,
            deviceid: 2,
            sourceid: 4,
            detail: 0,
            flags: 0,
            time,
            mask: &MASK_XY,
            values: raw,
            raw_values: raw,
        }
    }

    fn button(evtype: i32, time: Time, detail: i32) -> RawEventView<'static> {
        RawEventView {
            evtype,
            deviceid: 2,
            sourceid: 4,
            detail,
            flags: 0,
            time,
            mask: &[],
            values: &[],
            raw_values: &[],
        }
    }

    /// Runs `events` through one filter and returns the indices it passes.
    fn passed(events: &[RawEventView<'_>], grabbed: bool) -> Vec<usize> {
        let mut filter = RawDuplicateFilter::default();
        (0..events.len()).filter(|&i| !filter.is_duplicate(&events[i], grabbed)).collect()
    }

    /// The stream measured under a Confined grab in Xwayland: every event twice, back to back.
    #[test]
    fn grabbed_pairs_come_out_once() {
        let a = [20.0, -8.0];
        let b = [1.0, 0.0];
        let events = [
            motion(100, &a),
            motion(100, &a),
            motion(605, &b),
            motion(605, &b),
            button(xinput2::XI_RawButtonPress, 1110, 1),
            button(xinput2::XI_RawButtonPress, 1110, 1),
            button(xinput2::XI_RawButtonRelease, 1110, 1),
            button(xinput2::XI_RawButtonRelease, 1110, 1),
        ];
        assert_eq!(passed(&events, true), vec![0, 2, 4, 6]);
    }

    /// Two genuine identical motions in one millisecond, each doubled by the grab: four copies,
    /// two motions out. A filter that dropped every repeat would pass only one.
    #[test]
    fn grabbed_genuine_repeats_survive() {
        let a = [1.0, 0.0];
        let events = [motion(7, &a), motion(7, &a), motion(7, &a), motion(7, &a)];
        assert_eq!(passed(&events, true), vec![0, 2]);
    }

    /// Without a grab the server sends one copy, so identical events are genuine and all pass.
    #[test]
    fn ungrabbed_passes_everything() {
        let a = [1.0, 0.0];
        let events = [motion(7, &a), motion(7, &a), motion(7, &a)];
        assert_eq!(passed(&events, false), vec![0, 1, 2]);
    }

    /// Events that differ in any field are distinct, grab or not.
    #[test]
    fn grabbed_distinct_events_all_pass() {
        let a = [20.0, -8.0];
        let a_other_raw = [20.0, -7.0];
        let mut other_source = motion(100, &a);
        other_source.sourceid = 9;
        let mut other_flags = motion(100, &a);
        other_flags.flags = xinput2::XIPointerEmulated;
        let mask_x = [0b1u8];
        let mut other_mask = motion(100, &a[..1]);
        other_mask.mask = &mask_x;
        let mut accel_differs = motion(100, &a);
        accel_differs.values = &a_other_raw;
        let events = [
            motion(100, &a),
            motion(101, &a),
            motion(101, &a_other_raw),
            other_source,
            other_flags,
            other_mask,
            accel_differs,
            button(xinput2::XI_RawButtonPress, 101, 1),
            button(xinput2::XI_RawButtonPress, 101, 3),
            button(xinput2::XI_RawButtonRelease, 101, 3),
        ];
        assert_eq!(passed(&events, true), (0..events.len()).collect::<Vec<_>>());
    }

    /// A grab released between the two copies of a pair (the server sent both, the flag is
    /// already clear) passes the second copy rather than leaving it pending for later.
    #[test]
    fn grab_released_clears_pending() {
        let a = [3.0, 3.0];
        let mut filter = RawDuplicateFilter::default();
        assert!(!filter.is_duplicate(&motion(5, &a), true));
        assert!(!filter.is_duplicate(&motion(5, &a), false));
        // Grabbed again: the next copy starts a new pair instead of matching the old one.
        assert!(!filter.is_duplicate(&motion(5, &a), true));
        assert!(filter.is_duplicate(&motion(5, &a), true));
    }

    #[test]
    fn interrupt_forgets_pending() {
        let a = [3.0, 3.0];
        let mut filter = RawDuplicateFilter::default();
        assert!(!filter.is_duplicate(&motion(5, &a), true));
        filter.interrupt();
        assert!(!filter.is_duplicate(&motion(5, &a), true));
    }

    /// The view reads the valuator arrays an `XIRawEvent` points at, one value per mask bit.
    #[test]
    fn view_reads_xi_raw_event() {
        let mut mask = [0b0000_1011u8, 0b0000_0001]; // valuators 0, 1, 3 and 8
        let mut values = [1.5f64, -2.0, 0.25, 9.0];
        let mut raw = [1.0f64, -1.0, 0.5, 8.0];
        let mut xev: XIRawEvent = unsafe { std::mem::zeroed() };
        xev.deviceid = 2;
        xev.sourceid = 4;
        xev.detail = 0;
        xev.flags = 0;
        xev.time = 1234;
        xev.valuators = XIValuatorState {
            mask_len: mask.len() as i32,
            mask: mask.as_mut_ptr(),
            values: values.as_mut_ptr(),
        };
        xev.raw_values = raw.as_mut_ptr();
        let view = unsafe { RawEventView::from_xi(xinput2::XI_RawMotion, &xev) };
        assert_eq!(view.deviceid, 2);
        assert_eq!(view.sourceid, 4);
        assert_eq!(view.time, 1234);
        assert_eq!(view.mask, &mask[..]);
        assert_eq!(view.values, &values[..]);
        assert_eq!(view.raw_values, &raw[..]);

        let empty: XIRawEvent = unsafe { std::mem::zeroed() };
        let view = unsafe { RawEventView::from_xi(xinput2::XI_RawButtonPress, &empty) };
        assert!(view.mask.is_empty() && view.values.is_empty() && view.raw_values.is_empty());
    }
}
