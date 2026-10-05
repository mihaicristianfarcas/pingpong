//! PongService: the Windows service that keeps a host running, the way
//! SunshineService keeps sunshine.exe running (`tools/sunshinesvc.cpp`).
//!
//! The service runs as LocalSystem in session 0, which has no desktop. It
//! launches `pong host` into the active console session with a copy of its own
//! SYSTEM token. That combination is the point:
//! - SYSTEM can open the secure desktop, so the host captures UAC prompts, the
//!   lock screen and the sign-in screen, and injects input into them;
//! - the console session is where the displays are.
//!
//! It relaunches the host when the console session changes (sign-out, fast user
//! switching, a reboot to the sign-in screen) or if the host exits.

use std::ffi::OsString;
use std::time::Duration;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId,
    TOKEN_ACCESS_MASK, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, ResetEvent, SetEvent,
    TerminateProcess, WaitForMultipleObjects, WaitForSingleObject, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const SERVICE_NAME: &str = "PongService";
const DISPLAY_NAME: &str = "Pong streaming host";
/// Set by the service to ask the host to shut down cleanly (restore the
/// displays, tell the client). The host watches it.
pub const STOP_EVENT: &str = "Global\\PongHostStop";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

define_windows_service!(ffi_service_main, service_main);

/// Entry point when the SCM starts us (`pong service`).
pub fn run() -> Result<(), String> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| format!("not started by the SCM: {e}"))
}

struct Events {
    stop: HANDLE,
    session: HANDLE,
}
unsafe impl Send for Events {}
unsafe impl Sync for Events {}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = service_body() {
        tracing::error!("service failed: {e}");
    }
}

fn service_body() -> Result<(), String> {
    let events = std::sync::Arc::new(unsafe {
        Events {
            stop: CreateEventW(None, true, false, None).map_err(|e| e.to_string())?,
            session: CreateEventW(None, false, false, None).map_err(|e| e.to_string())?,
        }
    });
    let ev = events.clone();
    let status = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Preshutdown | ServiceControl::Shutdown => {
            unsafe { SetEvent(ev.stop).ok() };
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(_) => {
            unsafe { SetEvent(ev.session).ok() };
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .map_err(|e| e.to_string())?;

    let set = |state: ServiceState, accept: ServiceControlAccept| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(20),
            process_id: None,
        });
    };
    set(
        ServiceState::Running,
        ServiceControlAccept::STOP
            | ServiceControlAccept::PRESHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
    );
    tracing::info!("PongService running");

    let host_stop = unsafe { CreateEventW(None, true, false, PCWSTR(wide(STOP_EVENT).as_ptr())) }
        .map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_w = wide(&exe.to_string_lossy());

    // Every 3 s until stopped: make sure a host runs in the console session.
    while unsafe { WaitForSingleObject(events.stop, 3000) } != WAIT_OBJECT_0 {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session == 0xFFFF_FFFF {
            continue;
        }
        unsafe { ResetEvent(host_stop).ok() };
        let child = match launch(&exe_w, session) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(session, "could not launch the host: {e}");
                continue;
            }
        };
        tracing::info!(session, pid = child.dwProcessId, "host launched");
        loop {
            let handles = [events.stop, child.hProcess, events.session];
            let which = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            match which.0 - WAIT_OBJECT_0.0 {
                2 if unsafe { WTSGetActiveConsoleSessionId() } == session => continue,
                0 | 2 => {
                    // Stopping, or the console moved to another session: ask
                    // the host to wind down, then insist.
                    unsafe { SetEvent(host_stop).ok() };
                    if unsafe { WaitForSingleObject(child.hProcess, 15_000) } != WAIT_OBJECT_0 {
                        tracing::warn!("host did not stop in time; terminating it");
                        unsafe { TerminateProcess(child.hProcess, 1).ok() };
                    }
                    break;
                }
                _ => {
                    tracing::warn!("host exited; relaunching");
                    break;
                }
            }
        }
        unsafe {
            let _ = CloseHandle(child.hThread);
            let _ = CloseHandle(child.hProcess);
        }
    }

    set(ServiceState::Stopped, ServiceControlAccept::empty());
    Ok(())
}

/// Start `pong host` in `session` with a copy of this process's SYSTEM token.
fn launch(exe: &[u16], session: u32) -> Result<PROCESS_INFORMATION, String> {
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ACCESS_MASK(TOKEN_DUPLICATE.0 | TOKEN_QUERY.0),
            &mut own,
        )
        .map_err(|e| format!("OpenProcessToken: {e}"))?;
        let mut token = HANDLE::default();
        let dup = DuplicateTokenEx(
            own,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        );
        let _ = CloseHandle(own);
        dup.map_err(|e| format!("DuplicateTokenEx: {e}"))?;
        SetTokenInformation(token, TokenSessionId, &session as *const u32 as *const _, 4)
            .map_err(|e| format!("SetTokenInformation(TokenSessionId): {e}"))?;

        let mut desktop = wide("winsta0\\default");
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let exe_str = String::from_utf16_lossy(&exe[..exe.len() - 1]);
        let mut cmd = wide(&format!("\"{exe_str}\" host"));
        let mut pi = PROCESS_INFORMATION::default();
        let result = CreateProcessAsUserW(
            Some(token),
            PCWSTR(exe.as_ptr()),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            None,
            None,
            &si,
            &mut pi,
        );
        let _ = CloseHandle(token);
        result.map_err(|e| format!("CreateProcessAsUserW: {e}"))?;
        Ok(pi)
    }
}

/// Register the service (auto-start, LocalSystem, restart on failure), open
/// the firewall, and start it.
pub fn install() -> Result<(), String> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| format!("cannot open the service manager (run as administrator): {e}"))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![OsString::from("service")],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let service = match manager.create_service(&info, access) {
        Ok(s) => s,
        Err(_) => {
            let s = manager
                .open_service(SERVICE_NAME, access)
                .map_err(|e| format!("open existing service: {e}"))?;
            s.change_config(&info)
                .map_err(|e| format!("update service: {e}"))?;
            s
        }
    };
    let _ = service.set_description("Streams this PC to Ping clients over post-quantum WireGuard.");
    let _ = service.update_failure_actions(windows_service::service::ServiceFailureActions {
        reset_period: windows_service::service::ServiceFailureResetPeriod::After(
            Duration::from_secs(86400),
        ),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            windows_service::service::ServiceAction {
                action_type: windows_service::service::ServiceActionType::Restart,
                delay: Duration::from_secs(3),
            };
            3
        ]),
    });
    firewall(true);
    let _ = service.start::<&str>(&[]);
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| format!("cannot open the service manager (run as administrator): {e}"))?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        )
        .map_err(|e| format!("open service: {e}"))?;
    let _ = service.stop();
    for _ in 0..40 {
        match service.query_status() {
            Ok(s) if s.current_state == ServiceState::Stopped => break,
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    service
        .delete()
        .map_err(|e| format!("delete service: {e}"))?;
    firewall(false);
    Ok(())
}

/// Allow (or remove) inbound traffic to this executable.
fn firewall(allow: bool) {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let _ = std::process::Command::new("netsh")
        .args(["advfirewall", "firewall", "delete", "rule", "name=Pong"])
        .output();
    if allow {
        for proto in ["UDP", "TCP"] {
            let _ = std::process::Command::new("netsh")
                .args([
                    "advfirewall",
                    "firewall",
                    "add",
                    "rule",
                    "name=Pong",
                    "dir=in",
                    "action=allow",
                    &format!("protocol={proto}"),
                    &format!("program={exe}"),
                    "enable=yes",
                    "profile=any",
                ])
                .output();
        }
    }
}

/// In the host: return when the service asks us to stop.
pub fn wait_for_stop_request() {
    unsafe {
        let name = wide(STOP_EVENT);
        use windows::Win32::System::Threading::{OpenEventW, SYNCHRONIZATION_SYNCHRONIZE};
        let Ok(event) = OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr()))
        else {
            // Not launched by the service: nothing to wait for.
            loop {
                std::thread::park();
            }
        };
        WaitForSingleObject(event, INFINITE);
    }
}
