//! The live feed: the lines the app logs for detectors, held until the board's next tick.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use telemetry::LineTap;
use telemetry::events::DETECTOR_EVENTS;

/// About 1 MB of lines. A second normally brings under 10.
const QUEUE_CAP: usize = 2048;

/// Lines from the telemetry tap, in arrival order. It never logs: it runs inside a sink.
pub struct LineQueue {
    wanted: HashSet<&'static str>,
    lines: Mutex<VecDeque<Vec<u8>>>,
    dropped: AtomicU64,
}

impl LineQueue {
    #[must_use]
    pub fn new() -> Self {
        Self {
            wanted: DETECTOR_EVENTS.iter().copied().collect(),
            lines: Mutex::new(VecDeque::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// Takes every queued line, and how many the full queue dropped since the last drain.
    pub fn drain(&self) -> (Vec<Vec<u8>>, u64) {
        let lines = std::mem::take(&mut *self.lines.lock().unwrap_or_else(PoisonError::into_inner));
        (lines.into(), self.dropped.swap(0, Ordering::Relaxed))
    }
}

impl Default for LineQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl LineTap for LineQueue {
    fn wants_event(&self, event_name: &str) -> bool {
        self.wanted.contains(event_name)
    }

    fn take(&self, line: &[u8]) {
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() >= QUEUE_CAP {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            lines.push_back(line.to_vec());
        }
    }
}
