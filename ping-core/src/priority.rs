//! Scheduling for the stream's latency-critical threads.

/// The calling thread carries the stream (receiving, decoding, presenting,
/// input). On macOS that is the user-interactive QoS class: without it, the
/// scheduler may run the thread on an efficiency core and stretch its
/// timers, and every frame waits on it.
pub fn latency_critical() {
    #[cfg(target_os = "macos")]
    // SAFETY: changes only the calling thread's own QoS class.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

/// CPU time the calling thread has used, for the stream's diagnostics
/// (macOS only for now).
pub fn thread_cpu_time() -> Option<std::time::Duration> {
    #[cfg(target_os = "macos")]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: a valid out-param for this thread's own clock.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0 {
            return Some(std::time::Duration::new(
                ts.tv_sec as u64,
                ts.tv_nsec as u32,
            ));
        }
    }
    None
}
