//! Reading what the foreground application is doing with the pointer.
//!
//! This is what makes the mouse mode automatic (v2 design §5.2, as revised
//! since). The client used to switch between absolute and relative on a
//! hotkey, so the CS2 buy menu had no visible pointer until the user remembered
//! a chord nobody had taught them.
//!
//! **The application declares its intent; nothing here guesses.** Measured on
//! the Windows test host over 90 s and 180 samples, driven by hand through CS2:
//!
//! | | |
//! |---|---|
//! | `GetCursor` live (ARROW) | 105 |
//! | `GetCursor` NULL -- the app hid it | 75 |
//! | `AttachThreadInput` failures | 0 |
//!
//! switching cleanly on all four gameplay/menu transitions. An earlier draft
//! was going to infer this by comparing the deltas we sent against the position
//! the host reported back; that is a heuristic and this is a fact.
//!
//! **Two signals, not one.** CS2 also clips the pointer to a 1x1 rectangle at
//! the screen centre throughout gameplay and releases it in menus. The two
//! agreed on every sample, and both are carried because neither is contractual:
//! a game that hides without clipping, or clips without hiding, is easy to
//! imagine.
//!
//! **Why the per-thread call and not `GetCursorInfo`.** A cursor is per-thread
//! state. The global `GetCursorInfo` reports nothing at all on a host with no
//! mouse attached -- `flags=0x0, hCursor=0x0` on 24 of 24 samples -- while
//! `GetCursor` behind `AttachThreadInput` stayed live for all 24. The
//! per-thread call is the only one measured working in both states.

use pingpong_proto::control::{CursorShape, CursorState};

#[cfg(windows)]
mod imp;

#[cfg(windows)]
pub use imp::CursorWatcher;

#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "macos")]
pub use mac::{prepare, CursorWatcher};

/// Decide the pointer state from the raw readings, with no Win32 in sight.
///
/// Split out so the rule -- which combination means "the app has the mouse" --
/// is testable on a machine that has no `SendInput` at all.
///
/// `clip` is the app's cursor confinement as a fraction of the display: `None`
/// when unconfined. A game holding the pointer at a point clips to 1x1; a
/// window that merely confines to itself clips to something large, which is NOT
/// the same thing and must not lock the client's pointer.
pub fn classify(
    cursor: Option<CursorShape>,
    clip_fraction: Option<f32>,
    x: u16,
    y: u16,
) -> CursorState {
    // A clip is only evidence of a grab when it is SMALL. Windows reports the
    // full virtual desktop as the clip rectangle in the normal case, and an app
    // legitimately confining the pointer to its own window is not claiming the
    // mouse -- treating that as a grab would lock the client's pointer every
    // time a dialog restricted it.
    let clipped = clip_fraction.is_some_and(|f| f < GRAB_CLIP_FRACTION);
    CursorState {
        visible: cursor.is_some(),
        clipped,
        shape: cursor.unwrap_or(CursorShape::Arrow),
        x,
        y,
    }
}

/// How small a clip rectangle has to be, against the display, to read as "the
/// app has taken the mouse".
///
/// CS2 clips to 1x1 -- effectively zero -- so this has enormous headroom. It is
/// deliberately far below anything an app would use to confine the pointer to a
/// window or a dialog, because those are not grabs.
pub const GRAB_CLIP_FRACTION: f32 = 0.02;

/// Whether the client should lock its pointer and send relative motion.
///
/// Either signal is sufficient. They agreed on all 180 measured samples, but
/// requiring BOTH would fail on the first game that only does one of them, and
/// a locked pointer is recoverable while a camera that will not turn is not.
pub fn app_has_the_mouse(state: &CursorState) -> bool {
    !state.visible || state.clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hidden_cursor_means_the_app_took_the_mouse() {
        // The CS2 gameplay reading: NULL cursor, clipped to a point.
        let s = classify(None, Some(0.0), 1280, 720);
        assert!(!s.visible);
        assert!(s.clipped);
        assert!(app_has_the_mouse(&s));
    }

    #[test]
    fn a_visible_unclipped_cursor_means_the_user_has_it() {
        // The CS2 buy-menu reading, and every desktop reading.
        let s = classify(Some(CursorShape::Arrow), None, 100, 200);
        assert!(s.visible);
        assert!(!s.clipped);
        assert!(!app_has_the_mouse(&s));
    }

    #[test]
    fn either_signal_alone_is_enough() {
        // They agreed on all 180 measured samples -- but requiring both would
        // fail on the first game that only does one, and neither behaviour is
        // contractual.
        let hidden_only = classify(None, None, 0, 0);
        assert!(app_has_the_mouse(&hidden_only), "hidden but not clipped");

        let clipped_only = classify(Some(CursorShape::Arrow), Some(0.0), 0, 0);
        assert!(app_has_the_mouse(&clipped_only), "clipped but not hidden");
    }

    #[test]
    fn confining_the_pointer_to_a_window_is_not_a_grab() {
        // The case that would otherwise lock the client's pointer whenever a
        // dialog restricted the cursor to itself. A grab is a POINT, not a
        // rectangle someone can still move around in.
        let s = classify(Some(CursorShape::Arrow), Some(0.4), 0, 0);
        assert!(!s.clipped);
        assert!(!app_has_the_mouse(&s));
    }

    #[test]
    fn the_shape_is_carried_through() {
        let s = classify(Some(CursorShape::IBeam), None, 7, 9);
        assert_eq!(s.shape, CursorShape::IBeam);
        assert_eq!((s.x, s.y), (7, 9));
    }

    #[test]
    fn a_hidden_cursor_still_reports_a_shape_rather_than_none() {
        // `CursorState` has no optional shape: the client needs something to
        // draw the instant the pointer comes back, and Arrow is the safe answer
        // for a frame where the app was not telling us.
        let s = classify(None, None, 0, 0);
        assert_eq!(s.shape, CursorShape::Arrow);
    }
}
