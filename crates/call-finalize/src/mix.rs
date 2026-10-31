//! The mic and remote stems summed into one 16 kHz mono track, block by block, so a long call
//! never sits in memory whole. Portable: it reads the stems' WAV files directly.

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use crate::FinalizeError;
use crate::plan::{PlacedStem, StemChunk};

/// stem-writer's header is always the plain 44-byte PCM one.
const WAV_HEADER_BYTES: u64 = 44;
const BYTES_PER_SAMPLE: u64 = 2;

struct OpenStem {
    path: PathBuf,
    file: File,
    chunks: Vec<StemChunk>,
    /// The first chunk that may still reach the next block.
    next_chunk: usize,
    /// Whole samples on disk. A file repair truncated may hold fewer than the manifest counts.
    samples_on_disk: u64,
}

pub struct StemMix {
    stems: Vec<OpenStem>,
    bytes: Vec<u8>,
    sums: Vec<i32>,
}

impl StemMix {
    /// Opens every placed stem.
    ///
    /// # Errors
    ///
    /// Fails when a stem file cannot be opened.
    pub fn open(stems: &[PlacedStem]) -> Result<Self, FinalizeError> {
        let stems = stems
            .iter()
            .map(|stem| {
                let file = File::open(&stem.file).map_err(io_error(&stem.file))?;
                let length = file.metadata().map_err(io_error(&stem.file))?.len();
                let mut chunks = stem.chunks.clone();
                chunks.sort_by_key(|chunk| chunk.mix_sample);
                Ok(OpenStem {
                    path: stem.file.clone(),
                    file,
                    chunks,
                    next_chunk: 0,
                    samples_on_disk: length.saturating_sub(WAV_HEADER_BYTES) / BYTES_PER_SAMPLE,
                })
            })
            .collect::<Result<_, FinalizeError>>()?;
        Ok(Self {
            stems,
            bytes: Vec::new(),
            sums: Vec::new(),
        })
    }

    /// Fills `out` with the mix from timeline sample `start`. Blocks must come in timeline
    /// order. Where no stem plays, the mix is silent; where both do, it is their sum, limited.
    ///
    /// # Errors
    ///
    /// Fails when a stem file cannot be read.
    pub fn read_block(&mut self, start: u64, out: &mut [i16]) -> Result<(), FinalizeError> {
        let end = start + out.len() as u64;
        self.sums.clear();
        self.sums.resize(out.len(), 0);
        for stem in &mut self.stems {
            while stem
                .chunks
                .get(stem.next_chunk)
                .is_some_and(|chunk| chunk.mix_sample + chunk.sample_count <= start)
            {
                stem.next_chunk += 1;
            }
            for chunk in &stem.chunks[stem.next_chunk..] {
                if chunk.mix_sample >= end {
                    break;
                }
                let from = chunk.mix_sample.max(start);
                let to = (chunk.mix_sample + chunk.sample_count).min(end);
                let file_from = chunk.file_sample + (from - chunk.mix_sample);
                let file_to = (file_from + (to - from)).min(stem.samples_on_disk);
                if file_to <= file_from {
                    continue;
                }
                let count = usize::try_from(file_to - file_from).unwrap_or(0);
                self.bytes
                    .resize(count * usize::try_from(BYTES_PER_SAMPLE).unwrap_or(2), 0);
                stem.file
                    .seek(SeekFrom::Start(
                        WAV_HEADER_BYTES + file_from * BYTES_PER_SAMPLE,
                    ))
                    .and_then(|_| stem.file.read_exact(&mut self.bytes))
                    .map_err(io_error(&stem.path))?;
                let offset = usize::try_from(from - start).unwrap_or(0);
                for (sum, pair) in self.sums[offset..offset + count]
                    .iter_mut()
                    .zip(self.bytes.as_chunks::<2>().0)
                {
                    *sum += i32::from(i16::from_le_bytes(*pair));
                }
            }
        }
        for (sample, sum) in out.iter_mut().zip(&self.sums) {
            *sample = soft_limit(*sum);
        }
        Ok(())
    }
}

/// Where the limiter starts to bend: 80% of full scale. Speech on one stem rarely gets there,
/// so it passes untouched.
const LIMIT_KNEE: f64 = 0.8;

/// Both stems loud at once can sum past full scale. Above the knee the sum bends smoothly
/// towards full scale instead of clipping, which would crackle.
fn soft_limit(sum: i32) -> i16 {
    let full_scale = f64::from(i16::MAX);
    let level = f64::from(sum) / full_scale;
    let magnitude = level.abs();
    let limited = if magnitude <= LIMIT_KNEE {
        magnitude
    } else {
        let headroom = 1.0 - LIMIT_KNEE;
        LIMIT_KNEE + headroom * ((magnitude - LIMIT_KNEE) / headroom).tanh()
    };
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the limited level stays inside full scale"
    )]
    let sample = (limited.copysign(level) * full_scale).round() as i16;
    sample
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> FinalizeError + '_ {
    move |source| FinalizeError::Io {
        path: path.to_owned(),
        source,
    }
}
