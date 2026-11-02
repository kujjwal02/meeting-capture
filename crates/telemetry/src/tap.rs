//! The live feed: one in-process reader, the app's detectors, gets a copy of chosen log lines and
//! of every call metric row as the sinks write them.
//!
//! Log lines go to the tap when their string `event.name` is one it wants. Metric rows go to it
//! when they carry a `trace_id`, so a call's rows do and the daily host rows do not. Environment
//! rows never do.

use std::sync::{Arc, OnceLock};

static INSTALLED_TAP: OnceLock<Arc<dyn LineTap>> = OnceLock::new();

/// A reader of lines as they are written. Both methods run inside a sink, which may hold the log
/// file's or the recorder's lock, so they must not log and must return quickly.
pub trait LineTap: Send + Sync + 'static {
    /// Whether a log line with this `event.name` goes to `take`. Called on every log line.
    fn wants_event(&self, event_name: &str) -> bool;
    /// One encoded JSON line, `log` or `metric`, ending in its newline as the file holds it.
    fn take(&self, line: &[u8]);
}

/// Once per process, after `init`. A second call is ignored.
pub fn install_line_tap(tap: Arc<dyn LineTap>) {
    // The first tap stays: a second reader would split the feed.
    let _first_stays = INSTALLED_TAP.set(tap);
}

pub(crate) fn installed_tap() -> Option<&'static dyn LineTap> {
    INSTALLED_TAP.get().map(Arc::as_ref)
}
