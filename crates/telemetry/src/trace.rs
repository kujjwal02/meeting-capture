//! Trace and span ids in W3C trace context form. A trace id is a hash of the id it stands for,
//! so the app, `mc-capture` and later mc-meet derive the same one for a call on their own.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Serialize, Serializer};
use sha2::{Digest as _, Sha256};

/// 16 bytes, written as 32 lowercase hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId([u8; 16]);

/// 8 bytes, written as 16 lowercase hex digits. Never zero, which W3C reserves for "no span".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId(u64);

impl TraceId {
    /// The trace of the call `call_id`: the call's source log path, as the app logs it.
    #[must_use]
    pub fn for_call(call_id: &str) -> Self {
        Self::hashed(&["call", call_id])
    }

    /// The trace of one pipeline attempt: one meeting on one input revision. A rerun on a new
    /// capture is a new trace.
    #[must_use]
    pub fn for_pipeline_run(meeting_id: &str, input_revision: &str) -> Self {
        Self::hashed(&["pipeline", meeting_id, input_revision])
    }

    fn hashed(parts: &[&str]) -> Self {
        let mut hasher = Sha256::new();
        for part in parts {
            // The length keeps ("ab", "c") apart from ("a", "bc").
            hasher.update(part.len().to_le_bytes());
            hasher.update(part.as_bytes());
        }
        let digest = hasher.finalize();
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        Self(bytes)
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for TraceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:016x}", self.0)
    }
}

impl Serialize for SpanId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Hands out span ids unique within the process and, through the seed, across processes.
/// `tracing` reuses its own span ids once a span closes, so they cannot be W3C ids.
pub(crate) struct SpanIds {
    seed: u64,
    next: AtomicU64,
}

impl SpanIds {
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            seed,
            next: AtomicU64::new(1),
        }
    }

    pub(crate) fn next(&self) -> SpanId {
        let count = self.next.fetch_add(1, Ordering::Relaxed);
        let mixed = splitmix64(
            self.seed
                .wrapping_add(count.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
        );
        SpanId(mixed.max(1))
    }
}

// SplitMix64's finalizer: spreads consecutive counts over all 64 bits.
fn splitmix64(input: u64) -> u64 {
    let mut mixed = input;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    mixed ^ (mixed >> 31)
}
