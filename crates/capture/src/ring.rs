use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{RING_CAPACITY_SAMPLES, Stem, StemFormat};

/// The real-time half of a stem ring. `push_interleaved` neither allocates nor locks, so it is
/// safe inside an audio callback.
pub(crate) struct MonoPusher {
    producer: rtrb::Producer<f32>,
    device_channels: usize,
    dropped_samples: Arc<AtomicU64>,
}

/// The ordinary-thread half of a stem ring.
pub struct StemReader {
    stem: Stem,
    format: StemFormat,
    consumer: rtrb::Consumer<f32>,
    dropped_samples: Arc<AtomicU64>,
}

pub(crate) fn stem_ring(stem: Stem, format: StemFormat) -> (MonoPusher, StemReader) {
    let (producer, consumer) = rtrb::RingBuffer::new(RING_CAPACITY_SAMPLES);
    let dropped_samples = Arc::new(AtomicU64::new(0));
    let pusher = MonoPusher {
        producer,
        device_channels: usize::from(format.device_channels.max(1)),
        dropped_samples: Arc::clone(&dropped_samples),
    };
    let reader = StemReader {
        stem,
        format,
        consumer,
        dropped_samples,
    };
    (pusher, reader)
}

impl MonoPusher {
    pub(crate) fn push_interleaved(&mut self, samples: &[f32]) {
        let channels = self.device_channels;
        let frame_count = samples.len() / channels;
        let accepted = frame_count.min(self.producer.slots());
        if let Ok(chunk) = self.producer.write_chunk_uninit(accepted) {
            #[expect(
                clippy::cast_precision_loss,
                reason = "channel counts are tiny and exact in f32"
            )]
            let scale = 1.0 / channels as f32;
            chunk.fill_from_iter(
                samples
                    .chunks_exact(channels)
                    .map(|frame| frame.iter().sum::<f32>() * scale),
            );
        }
        let dropped = frame_count - accepted;
        if dropped > 0 {
            self.dropped_samples
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }
}

impl StemReader {
    #[must_use]
    pub fn stem(&self) -> Stem {
        self.stem
    }

    #[must_use]
    pub fn format(&self) -> &StemFormat {
        &self.format
    }

    /// Samples the callback could not push because the ring was full.
    #[must_use]
    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples.load(Ordering::Relaxed)
    }

    /// Hands every queued sample to `sink`, in order, as one or two slices.
    pub fn drain(&mut self, mut sink: impl FnMut(&[f32])) {
        let queued = self.consumer.slots();
        let Ok(chunk) = self.consumer.read_chunk(queued) else {
            return;
        };
        let (first, second) = chunk.as_slices();
        if !first.is_empty() {
            sink(first);
        }
        if !second.is_empty() {
            sink(second);
        }
        chunk.commit_all();
    }
}
