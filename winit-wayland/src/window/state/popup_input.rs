//! What a popup needs from the input and the compositor (rinch patch): the press that names its
//! grab, whether a configure moved it, and the reposition requests still waiting for their
//! configure.

use std::sync::atomic::{AtomicU64, Ordering};

use winit_core::event_loop::AsyncRequestSerial;

/// A press a window received: a pointer button press, a key press or repeat, or a touch down.
///
/// `xdg_popup.grab` names the user action that opened the popup by such a press. A release is
/// never one: compositors that check the serial (Mutter) accept only the press's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotedPress<S> {
    /// The seat the press came from.
    pub seat: S,
    /// The press's serial.
    pub serial: u32,
    /// When it was noted, among every press the process noted. Serials are not compared to
    /// find the newest: they belong to the compositor and wrap around.
    order: u64,
}

impl<S> NotedPress<S> {
    /// A press noted now.
    pub fn new(seat: S, serial: u32) -> Self {
        static ORDER: AtomicU64 = AtomicU64::new(0);
        Self { seat, serial, order: ORDER.fetch_add(1, Ordering::Relaxed) }
    }
}

/// The press noted last among `presses` (a window's, and its ancestors'), if any.
pub fn newest_press<'a, S: 'a>(
    presses: impl IntoIterator<Item = Option<&'a NotedPress<S>>>,
) -> Option<&'a NotedPress<S>> {
    presses.into_iter().flatten().max_by_key(|press| press.order)
}

/// Whether a configure placing the popup at `new` (relative to its parent) moved it from where
/// the last one had put it. The first configure always places it.
pub fn placement_moved(previous: Option<(i32, i32)>, new: (i32, i32)) -> bool {
    previous != Some(new)
}

/// The `xdg_popup.reposition` requests a popup sent and the compositor has not yet answered.
///
/// Each request gets a token of its own. The compositor answers with `repositioned(token)` before
/// the configure that applies it, and may skip all but the last of several requests: an answer to
/// a token therefore completes every request sent before it as well.
#[derive(Debug, Default)]
pub struct Repositions {
    next_token: u32,
    pending: Vec<(u32, AsyncRequestSerial)>,
}

impl Repositions {
    /// The token to send with a new request that `serial` names to the application.
    pub fn begin(&mut self, serial: AsyncRequestSerial) -> u32 {
        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        self.pending.push((token, serial));
        token
    }

    /// The requests the answer to `token` completes, oldest first; none for a token this popup
    /// did not send.
    pub fn complete(&mut self, token: u32) -> Vec<AsyncRequestSerial> {
        let Some(index) = self.pending.iter().position(|(sent, _)| *sent == token) else {
            return Vec::new();
        };
        self.pending.drain(..=index).map(|(_, serial)| serial).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_press_of_a_chain_wins_whatever_its_serial() {
        let panel_press = NotedPress::new("seat0", 900);
        // Noted later, with a serial that wrapped around.
        let menu_press = NotedPress::new("seat0", 3);
        let newest = newest_press([Some(&panel_press), Some(&menu_press)]).unwrap();
        assert_eq!(newest.serial, 3);
        let newest = newest_press([Some(&menu_press), Some(&panel_press)]).unwrap();
        assert_eq!(newest.serial, 3, "the chain's order does not matter");
    }

    #[test]
    fn a_window_without_a_press_takes_its_ancestors() {
        let panel_press = NotedPress::new("seat1", 42);
        let newest = newest_press([None, Some(&panel_press)]).unwrap();
        assert_eq!((newest.seat, newest.serial), ("seat1", 42));
    }

    #[test]
    fn no_press_in_the_chain_names_nothing() {
        assert!(newest_press::<&str>([None, None]).is_none());
        assert!(newest_press::<&str>([]).is_none());
    }

    #[test]
    fn the_first_configure_places_the_popup() {
        assert!(placement_moved(None, (0, 0)));
    }

    #[test]
    fn a_configure_moves_the_popup_only_when_its_place_changed() {
        assert!(!placement_moved(Some((-20, 32)), (-20, 32)));
        // A flip to the anchor's other side.
        assert!(placement_moved(Some((-20, 32)), (-140, 32)));
    }

    #[test]
    fn an_answer_completes_its_request_and_those_before_it() {
        let mut repositions = Repositions::default();
        let (a, b, c) =
            (AsyncRequestSerial::get(), AsyncRequestSerial::get(), AsyncRequestSerial::get());
        let ta = repositions.begin(a);
        let tb = repositions.begin(b);
        let tc = repositions.begin(c);
        assert_ne!(ta, tb);
        // The compositor skipped the first request and answered the second.
        assert_eq!(repositions.complete(tb), vec![a, b]);
        assert_eq!(repositions.complete(ta), vec![], "already completed");
        assert_eq!(repositions.complete(tc), vec![c]);
    }

    #[test]
    fn an_unknown_token_completes_nothing() {
        let mut repositions = Repositions::default();
        let a = AsyncRequestSerial::get();
        let ta = repositions.begin(a);
        assert_eq!(repositions.complete(ta.wrapping_add(7)), vec![]);
        assert_eq!(repositions.complete(ta), vec![a]);
    }
}
