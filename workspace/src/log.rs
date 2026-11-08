//! The daemon's log: every line goes to stderr and into a ring of recent
//! lines, which the web UI streams. Generator stderr lands here too, so
//! panics and debug prints show up in the browser.

use std::collections::VecDeque;
use std::sync::Mutex;

const KEEP: usize = 1000;

struct Log {
    next_seq: u64,
    lines: VecDeque<(u64, String)>,
}

static LOG: Mutex<Log> = Mutex::new(Log { next_seq: 1, lines: VecDeque::new() });

pub fn log(line: String) {
    eprintln!("{line}");
    let mut log = LOG.lock().unwrap_or_else(|e| e.into_inner());
    let seq = log.next_seq;
    log.next_seq += 1;
    if log.lines.len() == KEEP {
        log.lines.pop_front();
    }
    log.lines.push_back((seq, line));
}

/// Lines with a sequence number above `after`, oldest first.
pub fn since(after: u64) -> Vec<(u64, String)> {
    let log = LOG.lock().unwrap_or_else(|e| e.into_inner());
    log.lines.iter().filter(|(seq, _)| *seq > after).cloned().collect()
}

/// `eprintln!` that also feeds the UI's log view.
macro_rules! wlog {
    ($($t:tt)*) => { $crate::log::log(format!($($t)*)) };
}
pub(crate) use wlog;
