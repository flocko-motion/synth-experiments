//! The few unix calls std doesn't expose: shutdown signals and killing a
//! whole process group.

use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;

extern "C" {
    fn signal(sig: i32, handler: usize) -> usize;
    fn kill(pid: i32, sig: i32) -> i32;
}

extern "C" fn on_stop(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}

/// Ctrl-C, `kill` and a closed terminal ask the daemon to shut down
/// cleanly, taking its generators with it.
pub fn install_stop_handlers() {
    for sig in [SIGHUP, SIGINT, SIGTERM] {
        // SAFETY: the handler only stores to an atomic.
        unsafe { signal(sig, on_stop as *const () as usize) };
    }
}

pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Kills every process in the group led by `pgid`, including anything a
/// generator started itself (a shell wrapper, a python child, …).
pub fn kill_group(pgid: u32) {
    if pgid > 1 {
        // SAFETY: plain syscall; a negative pid addresses the group.
        unsafe { kill(-(pgid as i32), SIGKILL) };
    }
}
