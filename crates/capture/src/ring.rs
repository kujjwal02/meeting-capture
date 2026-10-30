use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{RING_CAPACITY_SAMPLES, StemFormat};

/// Room for every buffer that fits in the sample ring, down to 64-frame buffers.
const STAMP_CAPACITY: usize = 1_024;

/// When one device buffer started, and where its samples sit in the reader's sample stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferStamp {
    /// Host clock time of the buffer's first sample, as the OS hands it to the callback, with no
    /// latency correction. Both stems use the same clock. On macOS it is mach host time in
    /// nanoseconds since boot, sleep excluded. On Windows it will be QPC time in nanoseconds.
    pub host_time_ns: u64,
    /// Samples the reader received before this buffer's first sample. A ring overflow drops the
    /// tail of a buffer, so the next stamp's index grows by less than `frame_count`.
    pub sample_index: u64,
    /// Frames the device delivered in this buffer, dropped ones included.
    pub frame_count: u32,
    /// The rate the device delivered this buffer at.
    pub sample_rate_hz: u32,
}

/// Losses a stem ring counts while it runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StemLosses {
    /// Samples the callback could not push because the sample ring was full.
    pub dropped_samples: u64,
    /// Stamps the callback could not push because the stamp ring was full.
    pub dropped_stamps: u64,
    /// Buffers that arrived without a valid host time, so they have no stamp.
    pub untimed_buffers: u64,
}

#[derive(Default)]
struct LossCounters {
    dropped_samples: AtomicU64,
    dropped_stamps: AtomicU64,
    untimed_buffers: AtomicU64,
}

/// The real-time half of a stem ring. `push_buffer` neither allocates nor locks, so it is safe
/// inside an audio callback.
pub(crate) struct MonoPusher {
    samples: rtrb::Producer<f32>,
    stamps: rtrb::Producer<BufferStamp>,
    device_channels: usize,
    pushed_samples: u64,
    losses: Arc<LossCounters>,
}

/// The ordinary-thread half of a stem ring.
pub struct StemReader {
    format: StemFormat,
    samples: rtrb::Consumer<f32>,
    stamps: rtrb::Consumer<BufferStamp>,
    losses: Arc<LossCounters>,
}

pub(crate) fn stem_ring(format: StemFormat) -> (MonoPusher, StemReader) {
    let (sample_producer, sample_consumer) = rtrb::RingBuffer::new(RING_CAPACITY_SAMPLES);
    let (stamp_producer, stamp_consumer) = rtrb::RingBuffer::new(STAMP_CAPACITY);
    let losses = Arc::new(LossCounters::default());
    let pusher = MonoPusher {
        samples: sample_producer,
        stamps: stamp_producer,
        device_channels: usize::from(format.device_channels.max(1)),
        pushed_samples: 0,
        losses: Arc::clone(&losses),
    };
    let reader = StemReader {
        format,
        samples: sample_consumer,
        stamps: stamp_consumer,
        losses,
    };
    (pusher, reader)
}

impl MonoPusher {
    /// Pushes channel 0 of an interleaved buffer, then its stamp. Channel 0, not the average:
    /// averaging the beams of a mic array cancels much of the voice (about 25 dB on a 3-beam
    /// array).
    pub(crate) fn push_buffer(
        &mut self,
        interleaved: &[f32],
        host_time_ns: Option<u64>,
        sample_rate_hz: u32,
    ) {
        let channels = self.device_channels;
        let frame_count = interleaved.len() / channels;
        let accepted = frame_count.min(self.samples.slots());
        let sample_index = self.pushed_samples;
        if let Ok(chunk) = self.samples.write_chunk_uninit(accepted) {
            chunk.fill_from_iter(interleaved.iter().step_by(channels).copied());
        }
        self.pushed_samples += accepted as u64;
        let dropped = frame_count - accepted;
        if dropped > 0 {
            self.losses
                .dropped_samples
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }

        // The stamp goes in after its samples, so a reader that drains stamps first always holds
        // the samples of every stamp it has.
        let Some(host_time_ns) = host_time_ns else {
            self.losses.untimed_buffers.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let stamp = BufferStamp {
            host_time_ns,
            sample_index,
            frame_count: u32::try_from(frame_count).unwrap_or(u32::MAX),
            sample_rate_hz,
        };
        if self.stamps.push(stamp).is_err() {
            self.losses.dropped_stamps.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl StemReader {
    #[must_use]
    pub fn format(&self) -> &StemFormat {
        &self.format
    }

    #[must_use]
    pub fn losses(&self) -> StemLosses {
        StemLosses {
            dropped_samples: self.losses.dropped_samples.load(Ordering::Relaxed),
            dropped_stamps: self.losses.dropped_stamps.load(Ordering::Relaxed),
            untimed_buffers: self.losses.untimed_buffers.load(Ordering::Relaxed),
        }
    }

    /// Hands every queued stamp to `on_stamp`, then every queued sample to `on_samples` as one or
    /// two slices, both in order. Every stamp handed out refers to samples handed out by this
    /// drain or an earlier one.
    pub fn drain(
        &mut self,
        mut on_stamp: impl FnMut(BufferStamp),
        mut on_samples: impl FnMut(&[f32]),
    ) {
        while let Ok(stamp) = self.stamps.pop() {
            on_stamp(stamp);
        }
        let queued = self.samples.slots();
        let Ok(chunk) = self.samples.read_chunk(queued) else {
            return;
        };
        let (first, second) = chunk.as_slices();
        if !first.is_empty() {
            on_samples(first);
        }
        if !second.is_empty() {
            on_samples(second);
        }
        chunk.commit_all();
    }
}
