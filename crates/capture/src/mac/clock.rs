use std::time::SystemTime;

use cidre::core_audio as ca;
use stem_timing::UtcAnchor;

/// The host clock now, on the same scale as `BufferStamp::host_time_ns`.
#[must_use]
pub fn host_now_ns() -> u64 {
    ca::host_time_to_nanos(ca::current_host_time())
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
