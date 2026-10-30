use std::time::SystemTime;

use cidre::core_audio as ca;

/// The host clock now, on the same scale as `BufferStamp::host_time_ns`.
#[must_use]
pub fn host_now_ns() -> u64 {
    ca::host_time_to_nanos(ca::current_host_time())
}

/// One reading of the host clock against UTC, to turn host times into wall-clock times.
#[derive(Debug, Clone, Copy)]
pub struct UtcAnchor {
    /// Midpoint of the two host clock reads around the UTC read.
    pub host_time_ns: u64,
    pub utc: SystemTime,
    /// Host time between the two reads: the anchor's error bound.
    pub read_gap_ns: u64,
}

#[must_use]
pub fn utc_anchor() -> UtcAnchor {
    let before_ns = host_now_ns();
    let utc = SystemTime::now();
    let after_ns = host_now_ns();
    let read_gap_ns = after_ns.saturating_sub(before_ns);
    UtcAnchor {
        host_time_ns: before_ns + read_gap_ns / 2,
        utc,
        read_gap_ns,
    }
}
