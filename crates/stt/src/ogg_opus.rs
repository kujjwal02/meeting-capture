//! Ogg Opus encoding of a stretch of a 16 kHz mono stem, as RFC 7845 lays it out. About 24 kbps
//! against the WAV's 256 kbps, so an upload on a weak link takes a tenth of the time.

use std::path::Path;

use sha2::{Digest as _, Sha256};

use crate::SttError;

/// The rate of the stem files.
pub const STEM_RATE_HZ: u32 = 16_000;

/// Speech stays clear for speech-to-text at this rate, at a tenth of the WAV's size.
const BITRATE_BPS: i32 = 24_000;
/// 20 ms, the frame length Opus is tuned for.
const FRAME_SAMPLES: usize = STEM_RATE_HZ as usize / 50;
/// Ogg Opus counts granule positions at 48 kHz whatever the input rate.
const GRANULE_RATE_HZ: u64 = 48_000;
const GRANULES_PER_SAMPLE: u64 = GRANULE_RATE_HZ / STEM_RATE_HZ as u64;
/// Larger than any packet at this bitrate; libopus recommends 4000 bytes.
const MAX_PACKET_BYTES: usize = 4_000;
/// One logical stream per file, so any serial number does.
const STREAM_SERIAL: u32 = 1;
/// Pages hold about a second of audio, so a cut file loses little.
const PACKETS_PER_PAGE: usize = 50;
const VENDOR: &str = "meeting-capture";

/// One stretch of a stem, encoded.
pub struct EncodedAudio {
    pub bytes: Vec<u8>,
    /// Hex SHA-256 of `bytes`: a saved reply names the exact audio it answers.
    pub sha256: String,
}

/// The samples `start_sample..start_sample + sample_count` of the 16 kHz mono 16-bit WAV at
/// `wav_path`, fewer when the file ends sooner.
///
/// # Errors
///
/// Returns `SttError::ReadStem` when the file cannot be read or is not 16 kHz mono 16-bit.
pub fn read_stem_samples(
    wav_path: &Path,
    start_sample: u64,
    sample_count: u64,
) -> Result<Vec<i16>, SttError> {
    let read_error = |detail: String| SttError::ReadStem {
        path: wav_path.to_path_buf(),
        source: std::io::Error::other(detail),
    };
    let mut reader = open_stem(wav_path)?;
    let start = u32::try_from(start_sample)
        .map_err(|_| read_error("start past 2^32 samples".to_owned()))?;
    if start >= reader.duration() {
        return Ok(Vec::new());
    }
    reader
        .seek(start)
        .map_err(|error| read_error(error.to_string()))?;
    let count = usize::try_from(sample_count).unwrap_or(usize::MAX);
    reader
        .samples::<i16>()
        .take(count)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| read_error(error.to_string()))
}

/// The loudness in dBFS of each `frame_samples` frame of the first `sample_count` samples of
/// the stem at `wav_path`, read as a stream so a long call needs little memory.
///
/// # Errors
///
/// Returns `SttError::ReadStem` when the file cannot be read or is not 16 kHz mono 16-bit.
pub fn frame_loudness_dbfs(
    wav_path: &Path,
    frame_samples: usize,
    sample_count: u64,
) -> Result<Vec<f32>, SttError> {
    let read_error = |detail: String| SttError::ReadStem {
        path: wav_path.to_path_buf(),
        source: std::io::Error::other(detail),
    };
    let mut reader = open_stem(wav_path)?;
    let count = usize::try_from(sample_count).unwrap_or(usize::MAX);
    let mut loudness = Vec::new();
    let mut energy = 0.0_f64;
    let mut filled = 0_usize;
    for sample in reader.samples::<i16>().take(count) {
        let level = f64::from(sample.map_err(|error| read_error(error.to_string()))?) / 32_768.0;
        energy += level * level;
        filled += 1;
        if filled == frame_samples {
            loudness.push(dbfs(energy, filled));
            energy = 0.0;
            filled = 0;
        }
    }
    if filled > 0 {
        loudness.push(dbfs(energy, filled));
    }
    Ok(loudness)
}

fn dbfs(energy: f64, sample_count: usize) -> f32 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a frame has far fewer than 2^52 samples"
    )]
    let mean = energy / sample_count as f64;
    if mean <= 0.0 {
        return f32::NEG_INFINITY;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "decibels of a 16-bit signal fit in f32"
    )]
    let dbfs = (10.0 * mean.log10()) as f32;
    dbfs
}

fn open_stem(
    wav_path: &Path,
) -> Result<hound::WavReader<std::io::BufReader<std::fs::File>>, SttError> {
    let read_error = |detail: String| SttError::ReadStem {
        path: wav_path.to_path_buf(),
        source: std::io::Error::other(detail),
    };
    let reader = hound::WavReader::open(wav_path).map_err(|error| read_error(error.to_string()))?;
    let spec = reader.spec();
    if spec.channels != 1
        || spec.sample_rate != STEM_RATE_HZ
        || spec.bits_per_sample != 16
        || spec.sample_format != hound::SampleFormat::Int
    {
        return Err(read_error(format!(
            "expected 16 kHz mono 16-bit PCM, found {} Hz, {} channels, {} bits",
            spec.sample_rate, spec.channels, spec.bits_per_sample
        )));
    }
    Ok(reader)
}

/// `samples`, 16 kHz mono, as an Ogg Opus file.
///
/// # Errors
///
/// Returns `SttError::Encode` when the encoder refuses a frame.
pub fn encode(samples: &[i16]) -> Result<EncodedAudio, SttError> {
    let encode_error = |detail: &str| SttError::Encode {
        detail: detail.to_owned(),
    };
    let mut encoder = opus_rs::OpusEncoder::new(
        i32::try_from(STEM_RATE_HZ).map_err(|_| encode_error("rate out of range"))?,
        1,
        opus_rs::Application::Voip,
    )
    .map_err(encode_error)?;
    encoder.bitrate_bps = BITRATE_BPS;
    let lookahead = encoder.lookahead();
    let pre_skip = u64::try_from(lookahead)
        .ok()
        .and_then(|samples| u16::try_from(samples * GRANULES_PER_SAMPLE).ok())
        .ok_or_else(|| encode_error("lookahead past the OpusHead field"))?;

    let mut writer = ogg::PacketWriter::new(Vec::new());
    let write_error = |error: std::io::Error| encode_error(&error.to_string());
    writer
        .write_packet(
            opus_head(pre_skip),
            STREAM_SERIAL,
            ogg::PacketWriteEndInfo::EndPage,
            0,
        )
        .map_err(write_error)?;
    writer
        .write_packet(
            opus_tags(),
            STREAM_SERIAL,
            ogg::PacketWriteEndInfo::EndPage,
            0,
        )
        .map_err(write_error)?;

    // The decoder drops `pre_skip` samples first, so the input is padded by the lookahead at
    // the end too, or its last words would be cut.
    let padded_len = samples.len() + lookahead;
    let frame_count = padded_len.div_ceil(FRAME_SAMPLES).max(1);
    // Granule of the last page: the input's own length after the pre-skip, which trims the
    // padding off the decoded end.
    let end_granule = u64::from(pre_skip) + samples.len() as u64 * GRANULES_PER_SAMPLE;
    let mut frame = vec![0_i16; FRAME_SAMPLES];
    let mut packet = vec![0_u8; MAX_PACKET_BYTES];
    for frame_index in 0..frame_count {
        let start = frame_index * FRAME_SAMPLES;
        frame.fill(0);
        if start < samples.len() {
            let end = (start + FRAME_SAMPLES).min(samples.len());
            frame[..end - start].copy_from_slice(&samples[start..end]);
        }
        let packet_len = encoder
            .encode_i16(&frame, FRAME_SAMPLES, &mut packet)
            .map_err(encode_error)?;
        let is_last = frame_index + 1 == frame_count;
        let granule = if is_last {
            end_granule
        } else {
            (frame_index as u64 + 1) * FRAME_SAMPLES as u64 * GRANULES_PER_SAMPLE
        };
        let end_info = if is_last {
            ogg::PacketWriteEndInfo::EndStream
        } else if (frame_index + 1) % PACKETS_PER_PAGE == 0 {
            ogg::PacketWriteEndInfo::EndPage
        } else {
            ogg::PacketWriteEndInfo::NormalPacket
        };
        writer
            .write_packet(
                packet[..packet_len].to_vec(),
                STREAM_SERIAL,
                end_info,
                granule,
            )
            .map_err(write_error)?;
    }
    let bytes = writer.into_inner();
    let sha256 = hex_sha256(&bytes);
    Ok(EncodedAudio { bytes, sha256 })
}

/// RFC 7845 section 5.1, for one channel at 16 kHz.
fn opus_head(pre_skip: u16) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(1); // channels
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&STEM_RATE_HZ.to_le_bytes());
    head.extend_from_slice(&0_i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family: mono or stereo, no table
    head
}

/// RFC 7845 section 5.2, with no comments.
fn opus_tags() -> Vec<u8> {
    let mut tags = Vec::with_capacity(16 + VENDOR.len());
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&u32::try_from(VENDOR.len()).unwrap_or(0).to_le_bytes());
    tags.extend_from_slice(VENDOR.as_bytes());
    tags.extend_from_slice(&0_u32.to_le_bytes());
    tags
}

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}
