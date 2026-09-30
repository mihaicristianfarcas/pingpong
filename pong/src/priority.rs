//! Scheduling for the stream's latency-critical threads.

/// The calling thread carries the stream (capture, encode, send, receive).
/// Windows: the highest normal priority. macOS: the user-interactive QoS
/// class -- without it the scheduler may put the thread on an efficiency
/// core and stretch its timers (the sender's pacing sleeps among them).
pub fn latency_critical() {
    #[cfg(windows)]
    // SAFETY: changes only the calling thread's own priority.
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    #[cfg(target_os = "macos")]
    // SAFETY: changes only the calling thread's own QoS class.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}
