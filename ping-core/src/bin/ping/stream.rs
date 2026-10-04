//! `ping stream` and `ping watch`: the stream in a window of the platform's
//! own, until it ends.

use std::process::ExitCode;
#[cfg(any(target_os = "macos", windows))]
use std::sync::Arc;

#[cfg(any(target_os = "macos", windows))]
use ping_core::session::Session;
use ping_core::session::{EndCallback, StreamRequest};
use pingpong_proto::control::codec;

use crate::script::run_script;

/// The request the flags make, from this display's native mode at 60 fps.
fn request(flags: &[String]) -> StreamRequest {
    let native = ping_core::session::native_mode();
    let mut r = StreamRequest {
        width: native.width,
        height: native.height,
        fps: 60,
        mute_in_background: false,
        ..StreamRequest::default()
    };
    let mut bitrate_given = false;
    let mut it = flags.iter();
    while let Some(f) = it.next() {
        match f.as_str() {
            "--size" => {
                if let Some((w, h)) = it.next().and_then(|v| v.split_once('x')) {
                    r.width = w.parse().unwrap_or(r.width);
                    r.height = h.parse().unwrap_or(r.height);
                }
            }
            "--fps" => r.fps = it.next().and_then(|v| v.parse().ok()).unwrap_or(r.fps),
            "--mbps" => {
                bitrate_given = true;
                r.bitrate_kbps = it
                    .next()
                    .and_then(|v| v.parse::<u32>().ok())
                    .map(|m| m * 1000)
                    .unwrap_or(r.bitrate_kbps)
            }
            "--codec" => {
                r.codecs = match it.next().map(String::as_str) {
                    Some("h264") => codec::H264,
                    Some("hevc") => codec::HEVC,
                    Some("av1") => codec::AV1,
                    _ => r.codecs,
                }
            }
            "--hdr" => r.hdr = true,
            "--yuv444" => r.yuv444 = true,
            "--windowed" => r.fullscreen = false,
            "--no-vsync" => r.vsync = false,
            "--frame-pacing" => r.frame_pacing = true,
            "--steam" => r.app = pingpong_proto::control::app::STEAM_BIG_PICTURE,
            "--mute-in-background" => r.mute_in_background = true,
            "--no-audio" => r.audio_channels = 0,
            // 2, 6 (5.1) or 8 (7.1).
            "--audio-channels" => {
                r.audio_channels = it.next().and_then(|v| v.parse().ok()).unwrap_or(2)
            }
            "--host-audio" => r.host_audio = true,
            "--keep-host-displays" => r.keep_host_displays = true,
            "--wan-only" => r.wan_only = true,
            // --via ADDR: only this address (repeatable), for diagnosing a path.
            "--via" => r.via.extend(
                it.next()
                    .and_then(|v| v.parse::<std::net::SocketAddr>().ok()),
            ),
            "--stats" => r.show_stats = true,
            "--cmd-is-win" => r.cmd_is_win = true,
            "--no-clipboard" => r.clipboard = false,
            // Watch the agent working on the host (Ctrl+Alt+Shift+T takes over).
            "--watch" => {
                r.watch = true;
                r.audio_channels = 0;
            }
            other => eprintln!("ignoring unknown flag {other}"),
        }
    }
    if !bitrate_given {
        r.bitrate_kbps =
            ping_core::stream::default_bitrate_kbps(r.width as u32, r.height as u32, r.fps);
    }
    r
}

#[cfg(target_os = "linux")]
pub fn run(name: &str, flags: &[String]) -> ExitCode {
    let request = request(flags);
    let hooks = ping_core::linux::Hooks {
        stdin_quit: false,
        started: Some(Box::new(|input, controls, quit: EndCallback| {
            if let Ok(script) = std::env::var("PING_TEST_INPUT") {
                let quit = quit.clone();
                std::thread::spawn(move || run_script(&script, &input, Some(controls), quit));
            }
            let _ = ctrlc::set_handler(move || quit(None));
        })),
    };
    match ping_core::linux::run(name, &request, hooks) {
        Ok(None) => ExitCode::SUCCESS,
        Ok(Some(r)) => {
            eprintln!("{r}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// Scripted input (PING_TEST_INPUT="wait 3000; key 01; move 100 0;
/// click; wheel 120") drives the stream without hands, for end-to-end
/// tests; Ctrl-C or SIGTERM end the session properly, so the host
/// restores its displays now rather than after its timeout.
#[cfg(any(target_os = "macos", windows))]
fn attach(session: &Session, on_end: &EndCallback) {
    if let (Ok(script), Some(input)) = (std::env::var("PING_TEST_INPUT"), session.input()) {
        let (controls, quit) = (session.controls(), on_end.clone());
        std::thread::spawn(move || run_script(&script, &input, controls, quit));
    }
    let quit = on_end.clone();
    let _ = ctrlc::set_handler(move || quit(None));
}

#[cfg(target_os = "macos")]
pub fn run(name: &str, flags: &[String]) -> ExitCode {
    use std::cell::RefCell;

    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    thread_local! {
        static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    }

    let Some(mtm) = MainThreadMarker::new() else {
        return ExitCode::FAILURE;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let request = request(flags);

    // A reason means the stream ended without the user quitting: exit 1,
    // so scripts can tell (NSApplication's terminate would always exit 0).
    let on_end: EndCallback = Arc::new(|reason| {
        if let Some(r) = &reason {
            eprintln!("{r}");
        }
        let failed = reason.is_some();
        dispatch2::DispatchQueue::main().exec_async(move || {
            SESSION.with(|s| {
                if let Some(mut s) = s.borrow_mut().take() {
                    s.close();
                }
            });
            if failed {
                std::process::exit(1);
            }
            if let Some(mtm) = MainThreadMarker::new() {
                NSApplication::sharedApplication(mtm).terminate(None);
            }
        });
    });
    match ping_core::session::start(name, &request, on_end.clone()) {
        Ok(session) => {
            attach(&session, &on_end);
            SESSION.with(|s| *s.borrow_mut() = Some(session));
        }
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    }
    app.run();
    ExitCode::SUCCESS
}

#[cfg(windows)]
pub fn run(name: &str, flags: &[String]) -> ExitCode {
    let request = request(flags);
    let (tx, rx) = crossbeam_channel::bounded::<Option<String>>(1);
    let on_end: EndCallback = Arc::new(move |reason| {
        let _ = tx.try_send(reason);
    });
    let mut session = match ping_core::session::start(name, &request, on_end.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    attach(&session, &on_end);
    let reason = rx.recv().unwrap_or(None);
    session.close();
    match reason {
        Some(r) => {
            eprintln!("{r}");
            ExitCode::FAILURE
        }
        None => ExitCode::SUCCESS,
    }
}
