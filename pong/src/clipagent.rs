//! Clipboard sharing on a Windows host. Pong runs as SYSTEM, and Windows
//! shows a SYSTEM process only part of the user's clipboard (text, but not
//! the files copied in Explorer); reading files as SYSTEM would also let a
//! user copy, and so send, what only SYSTEM may read. So the sharing runs in
//! a helper as the signed-in user -- `pong clipboard-agent`, started for the
//! session -- and Pong relays between it and the tunnel, over its standard
//! input and output:
//!
//! ```text
//! frame: kind u8 | length u32 LE | bytes
//! Pong -> agent   PACKET: a clipboard control body from the client
//!                 DIRECTIONS: which ways copies go from now on (one byte)
//! agent -> Pong   PACKET: a whole packet for the client;  LOG: a log line
//! ```
//!
//! The ways copies go at the start are on its command line.
//!
//! The agent ends when its input closes (the session ended).

use std::io::{Read, Write};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use parking_lot::Mutex;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0,
};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TokenPrimary, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

const PACKET: u8 = 0;
const LOG: u8 = 1;
const DIRECTIONS: u8 = 2;
/// A frame larger than this is not one of ours.
const MAX_FRAME: usize = 64 * 1024;

fn write_frame(w: &mut impl Write, kind: u8, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = Vec::with_capacity(5 + bytes.len());
    f.push(kind);
    f.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    f.extend_from_slice(bytes);
    w.write_all(&f)?;
    w.flush()
}

fn read_frame(r: &mut impl Read) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head).ok()?;
    let len = u32::from_le_bytes(head[1..5].try_into().ok()?) as usize;
    if len > MAX_FRAME {
        return None;
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    Some((head[0], body))
}

struct Process(HANDLE);

unsafe impl Send for Process {}

/// The session's clipboard helper, as Pong holds it.
pub struct ClipAgent {
    /// Frames for the helper: (kind, body).
    tx: Option<Sender<(u8, Vec<u8>)>>,
    process: Process,
    writer: Option<std::thread::JoinHandle<()>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl ClipAgent {
    /// Start the helper as the user signed in at the console; `send` puts
    /// its packets on the tunnel.
    pub fn spawn(
        rate: u64,
        directions: pingpong_clipboard::Directions,
        send: impl Fn(&[u8]) + Send + 'static,
    ) -> Result<ClipAgent, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let cmdline = format!(
            "\"{}\" clipboard-agent {rate} {}",
            exe.display(),
            directions.to_bits()
        );
        let (process, to_child, from_child) = unsafe { spawn_as_user(&cmdline)? };
        let (tx, rx) = crossbeam_channel::bounded::<(u8, Vec<u8>)>(8192);
        let writer = std::thread::Builder::new()
            .name("clip-agent-in".into())
            .spawn(move || {
                let mut to_child = to_child;
                for (kind, body) in rx {
                    if write_frame(&mut to_child, kind, &body).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let reader = std::thread::Builder::new()
            .name("clip-agent-out".into())
            .spawn(move || {
                let mut from_child = std::io::BufReader::new(from_child);
                while let Some((kind, bytes)) = read_frame(&mut from_child) {
                    match kind {
                        PACKET => send(&bytes),
                        LOG => tracing::info!(
                            "clipboard agent: {}",
                            String::from_utf8_lossy(&bytes).trim_end()
                        ),
                        _ => {}
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(ClipAgent {
            tx: Some(tx),
            process,
            writer: Some(writer),
            reader: Some(reader),
        })
    }

    pub fn deliver(&self, body: &[u8]) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send((PACKET, body.to_vec()));
        }
    }

    /// Which ways copies go, from now on. Waits for room rather than being
    /// dropped behind a burst of packets: it is a permission.
    pub fn set_directions(&self, d: pingpong_clipboard::Directions) {
        if let Some(tx) = &self.tx {
            let _ = tx.send_timeout((DIRECTIONS, vec![d.to_bits()]), Duration::from_secs(1));
        }
    }
}

impl Drop for ClipAgent {
    fn drop(&mut self) {
        // Its input closes: it stops. (The writer is joined after: one stuck
        // on a helper that stopped reading ends when the helper is ended.)
        drop(self.tx.take());
        unsafe {
            let started = Instant::now();
            if WaitForSingleObject(self.process.0, 3000) != WAIT_OBJECT_0 {
                tracing::warn!(
                    ms = started.elapsed().as_millis() as u64,
                    "the clipboard agent did not stop; ending it"
                );
                let _ = TerminateProcess(self.process.0, 1);
            }
            let _ = CloseHandle(self.process.0);
        }
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A pipe whose `child` end the child inherits; the other end is ours.
unsafe fn pipe(child_reads: bool) -> Result<(HANDLE, HANDLE), String> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        bInheritHandle: true.into(),
        ..Default::default()
    };
    let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
    CreatePipe(&mut read, &mut write, Some(&sa), 0).map_err(|e| format!("CreatePipe: {e}"))?;
    let (child, ours) = if child_reads {
        (read, write)
    } else {
        (write, read)
    };
    SetHandleInformation(ours, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))
        .map_err(|e| format!("SetHandleInformation: {e}"))?;
    Ok((child, ours))
}

/// Start `cmdline` as the console's user, its standard input and output
/// piped to us: (process, its input, its output).
unsafe fn spawn_as_user(cmdline: &str) -> Result<(Process, std::fs::File, std::fs::File), String> {
    let session = WTSGetActiveConsoleSessionId();
    let mut user = HANDLE::default();
    WTSQueryUserToken(session, &mut user).map_err(|e| format!("nobody is signed in ({e})"))?;
    let mut token = HANDLE::default();
    let dup = DuplicateTokenEx(
        user,
        TOKEN_ALL_ACCESS,
        None,
        SecurityImpersonation,
        TokenPrimary,
        &mut token,
    );
    let _ = CloseHandle(user);
    dup.map_err(|e| format!("DuplicateTokenEx: {e}"))?;

    let (child_in, our_in) = pipe(true)?;
    let (child_out, our_out) = pipe(false)?;
    let mut env: *mut std::ffi::c_void = std::ptr::null_mut();
    let have_env = CreateEnvironmentBlock(&mut env, Some(token), false).is_ok();
    let mut desktop = wide("winsta0\\default");
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        dwFlags: STARTF_USESTDHANDLES,
        hStdInput: child_in,
        hStdOutput: child_out,
        hStdError: HANDLE::default(),
        ..Default::default()
    };
    let mut cmd = wide(cmdline);
    let mut pi = PROCESS_INFORMATION::default();
    let result = CreateProcessAsUserW(
        Some(token),
        PCWSTR::null(),
        Some(PWSTR(cmd.as_mut_ptr())),
        None,
        None,
        true,
        CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
        have_env.then_some(env as *const std::ffi::c_void),
        None,
        &si,
        &mut pi,
    );
    if have_env {
        let _ = DestroyEnvironmentBlock(env);
    }
    let _ = CloseHandle(token);
    // The child's ends are the child's now.
    let _ = CloseHandle(child_in);
    let _ = CloseHandle(child_out);
    if let Err(e) = result {
        let _ = CloseHandle(our_in);
        let _ = CloseHandle(our_out);
        return Err(format!("CreateProcessAsUserW: {e}"));
    }
    let _ = CloseHandle(pi.hThread);
    Ok((
        Process(pi.hProcess),
        std::fs::File::from_raw_handle(our_in.0 as RawHandle),
        std::fs::File::from_raw_handle(our_out.0 as RawHandle),
    ))
}

/// Log lines, sent to Pong as frames.
struct LogWriter {
    out: Arc<Mutex<std::io::Stdout>>,
    line: Vec<u8>,
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.line.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        if !self.line.is_empty() {
            let _ = write_frame(&mut *self.out.lock(), LOG, &self.line);
        }
    }
}

/// `pong clipboard-agent RATE DIRECTIONS`: the helper itself, as the user.
pub fn run(args: &[String]) -> ExitCode {
    let rate = args.get(1).and_then(|r| r.parse().ok()).unwrap_or(4 << 20);
    // Nothing either way, unless Pong said.
    let directions = pingpong_clipboard::Directions::from_bits(
        args.get(2).and_then(|d| d.parse().ok()).unwrap_or(0),
    );
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let log_out = out.clone();
    tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_level(false)
        .with_writer(move || LogWriter {
            out: log_out.clone(),
            line: Vec::new(),
        })
        .init();
    let opts = pingpong_clipboard::Options {
        // The user's own temporary folder.
        files_dir: std::env::temp_dir().join("Pong Clipboard"),
        rate,
        offer_current: false,
        peer: "the client".into(),
        directions,
    };
    let sync = pingpong_clipboard::ClipSync::start(opts, move |p| {
        let _ = write_frame(&mut *out.lock(), PACKET, p);
    });
    let mut input = std::io::BufReader::new(std::io::stdin().lock());
    while let Some((kind, body)) = read_frame(&mut input) {
        match kind {
            PACKET => sync.deliver(&body),
            DIRECTIONS => {
                if let Some(&bits) = body.first() {
                    sync.set_directions(pingpong_clipboard::Directions::from_bits(bits));
                }
            }
            _ => {}
        }
    }
    drop(sync);
    // Give the last log lines a moment to go out.
    std::thread::sleep(Duration::from_millis(50));
    ExitCode::SUCCESS
}
