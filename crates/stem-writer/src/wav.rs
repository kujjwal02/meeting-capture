//! A 16 kHz mono 16-bit WAV that can be made durable mid-write: samples reach the disk before the
//! header that counts them.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{OUTPUT_RATE_HZ, StemWriterError};

pub(crate) const HEADER_BYTES: u32 = 44;
pub(crate) const BYTES_PER_SAMPLE: u32 = 2;

pub(crate) struct StemWav {
    path: PathBuf,
    file: BufWriter<File>,
    /// A second handle on the same file, used only to rewrite the header. Its cursor is its own,
    /// so a failed header write never leaves the sample handle pointing into the header. Windows
    /// `seek_write` moves the cursor, so a positional write on one handle would not do that.
    header_file: File,
    samples_written: u64,
    /// Samples the header counted at the last sync.
    synced_samples: u64,
    level: OutputLevel,
}

/// The level of the samples written since the last take: what the file holds, inserted silence
/// included. Summed in the write loop, so measuring costs no pass and no allocation.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutputLevel {
    sum_of_squares: f64,
    sample_count: u64,
    peak: f32,
}

/// The floor for digital silence, whose level in dBFS is minus infinity. A 16-bit file's own
/// floor is about -96 dBFS.
pub const SILENCE_DBFS: f64 = -120.0;

impl OutputLevel {
    /// The RMS level in dBFS. `None` when nothing was written.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "a second of 16 kHz samples is far below 2^52"
    )]
    pub fn rms_dbfs(&self) -> Option<f64> {
        (self.sample_count > 0)
            .then(|| dbfs((self.sum_of_squares / self.sample_count as f64).sqrt()))
    }

    /// The peak sample magnitude in dBFS. `None` when nothing was written.
    #[must_use]
    pub fn peak_dbfs(&self) -> Option<f64> {
        (self.sample_count > 0).then(|| dbfs(f64::from(self.peak)))
    }
}

fn dbfs(magnitude: f64) -> f64 {
    if magnitude > 0.0 {
        (20.0 * magnitude.log10()).max(SILENCE_DBFS)
    } else {
        SILENCE_DBFS
    }
}

impl StemWav {
    /// Creates the file. Fails when it exists: a finished segment is never opened for writing
    /// again.
    pub(crate) fn create(path: &Path) -> Result<Self, StemWriterError> {
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|source| io_error(path, source))?;
        let header_file = File::options()
            .write(true)
            .open(path)
            .map_err(|source| io_error(path, source))?;
        let mut wav = Self {
            path: path.to_owned(),
            file: BufWriter::new(file),
            header_file,
            samples_written: 0,
            synced_samples: 0,
            level: OutputLevel::default(),
        };
        let header = wav_header(0);
        wav.file
            .write_all(&header)
            .map_err(|source| io_error(path, source))?;
        Ok(wav)
    }

    /// The level since the last take, and starts the next.
    pub(crate) fn take_level(&mut self) -> OutputLevel {
        std::mem::take(&mut self.level)
    }

    pub(crate) fn samples_written(&self) -> u64 {
        self.samples_written
    }

    pub(crate) fn synced_samples(&self) -> u64 {
        self.synced_samples
    }

    /// Appends samples. Fails with `WavFull` before the data would pass the u32 size fields.
    pub(crate) fn write(&mut self, samples: &[f32]) -> Result<(), StemWriterError> {
        let total = self.samples_written + samples.len() as u64;
        if data_bytes(total).is_none() {
            return Err(StemWriterError::WavFull {
                path: self.path.clone(),
            });
        }
        for &sample in samples {
            let pcm = pcm16(sample);
            // The level is the file's: the sample after clamping and rounding, so an
            // overdriven or NaN input cannot read above 0 dBFS or poison the second's sum.
            let written = f32::from(pcm) / f32::from(i16::MAX);
            self.level.sum_of_squares += f64::from(written) * f64::from(written);
            self.level.peak = self.level.peak.max(written.abs());
            self.file
                .write_all(&pcm.to_le_bytes())
                .map_err(|source| io_error(&self.path, source))?;
        }
        self.samples_written += samples.len() as u64;
        self.level.sample_count += samples.len() as u64;
        Ok(())
    }

    /// Makes every sample written so far durable, then the header that counts them: flush and
    /// fsync the samples, rewrite the RIFF and data sizes, fsync again. A crash between the two
    /// leaves an older header over durable samples, never a header over missing ones.
    pub(crate) fn sync(&mut self) -> Result<(), StemWriterError> {
        let path = self.path.clone();
        let samples = self.samples_written;
        let data_bytes =
            data_bytes(samples).ok_or(StemWriterError::WavFull { path: path.clone() })?;
        self.file
            .flush()
            .and_then(|()| self.file.get_ref().sync_data())
            .map_err(|source| io_error(&path, source))?;
        let header = wav_header(data_bytes);
        self.header_file
            .seek(SeekFrom::Start(0))
            .and_then(|_| self.header_file.write_all(&header))
            .and_then(|()| self.header_file.sync_data())
            .map_err(|source| io_error(&path, source))?;
        self.synced_samples = samples;
        Ok(())
    }
}

/// The data chunk's size in bytes, when the header's u32 fields can hold it.
pub(crate) fn data_bytes(samples: u64) -> Option<u32> {
    u32::try_from(samples * u64::from(BYTES_PER_SAMPLE))
        .ok()
        .filter(|bytes| bytes.checked_add(HEADER_BYTES).is_some())
}

pub(crate) fn wav_header(data_bytes: u32) -> [u8; HEADER_BYTES as usize] {
    let byte_rate = OUTPUT_RATE_HZ * BYTES_PER_SAMPLE;
    let mut header = [0_u8; HEADER_BYTES as usize];
    let fields: [&[u8]; 13] = [
        b"RIFF",
        &(data_bytes + HEADER_BYTES - 8).to_le_bytes(),
        b"WAVE",
        b"fmt ",
        &16_u32.to_le_bytes(),
        // PCM, one channel.
        &1_u16.to_le_bytes(),
        &1_u16.to_le_bytes(),
        &OUTPUT_RATE_HZ.to_le_bytes(),
        &byte_rate.to_le_bytes(),
        // Block align, then bits per sample.
        &2_u16.to_le_bytes(),
        &16_u16.to_le_bytes(),
        b"data",
        &data_bytes.to_le_bytes(),
    ];
    let mut offset = 0;
    for field in fields {
        header[offset..offset + field.len()].copy_from_slice(field);
        offset += field.len();
    }
    header
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the value is clamped to the i16 range first"
)]
fn pcm16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

fn io_error(path: &Path, source: std::io::Error) -> StemWriterError {
    StemWriterError::Io {
        path: path.to_owned(),
        source,
    }
}
