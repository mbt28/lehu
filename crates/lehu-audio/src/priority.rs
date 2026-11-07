//! Realtime priority for the audio threads, best effort. The image either
//! raises the rtprio limit for `lehu` or grants `cap_sys_nice`
//! (`docs/image-requirements.md`); without either the threads stay at
//! `SCHED_OTHER` and the log says so once.

/// Above the `SCHED_OTHER` video decode, below the USB transport (FIFO 50):
/// the mixer sleeps in the device write and must never be starved into a
/// DAC underrun by a decode burst; the microphone drains a few frames per
/// wake and backs up into a heard gap when it is.
pub const AUDIO_FIFO: i32 = 10;

/// Make the calling thread `SCHED_FIFO` at `priority`. False when the
/// kernel refused.
pub fn fifo(priority: i32) -> bool {
    // SAFETY: sched_param is plain data; zeroed is a valid value of it.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = priority;
    // SAFETY: pthread_self is this thread; param is initialised.
    let err =
        unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) };
    err == 0
}

/// `fifo(AUDIO_FIFO)` with the outcome logged for `what`.
pub fn audio_thread(what: &str) {
    if fifo(AUDIO_FIFO) {
        log::debug!("{what}: SCHED_FIFO {AUDIO_FIFO}");
    } else {
        log::info!("{what}: no realtime priority (rtprio limit); running at SCHED_OTHER");
    }
}
