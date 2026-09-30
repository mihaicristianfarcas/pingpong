//! Keep a Linux host's screen on while it streams: a remote user resets no
//! idle timer, and a blanked X screen is captured black. The screensaver is
//! reset and suspended (DPMS with it) for as long as the session holds this,
//! on a connection of its own: X lifts the suspension if Pong goes away.

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::dpms::{ConnectionExt as _, DPMSMode};
use x11rb::protocol::screensaver::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ScreenSaver};
use x11rb::rust_connection::RustConnection;

/// Held for a session: the screen stays on until it is dropped.
pub struct Awake {
    conn: Option<RustConnection>,
}

impl Awake {
    /// Wake the screen (as a key press would) and keep it on.
    pub fn hold() -> Awake {
        let conn = match x11rb::connect(None) {
            Ok((conn, _)) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "cannot keep the screen on: no X display");
                return Awake { conn: None };
            }
        };
        let has = |ext: &'static str| conn.extension_information(ext).ok().flatten().is_some();
        let _ = conn.force_screen_saver(ScreenSaver::RESET);
        if has("DPMS") {
            let _ = conn.dpms_force_level(DPMSMode::ON);
        }
        if has("MIT-SCREEN-SAVER") {
            let _ = conn.screensaver_suspend(1);
        } else {
            tracing::info!(
                "the X server has no MIT-SCREEN-SAVER: the screen may blank during a session"
            );
        }
        let _ = conn.flush();
        Awake { conn: Some(conn) }
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        if let Some(conn) = &self.conn {
            let _ = conn.screensaver_suspend(0);
            let _ = conn.flush();
        }
    }
}
