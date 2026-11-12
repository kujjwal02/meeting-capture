//! A WAV stem as lossless FLAC and back. A stem is 16 kHz mono 16-bit PCM behind the stem
//! writer's 44-byte header, so its samples and their count rebuild the file byte for byte. Only
//! a stem whose header is the one the stem writer would write for its length is encoded.
//!
//! Encoding streams frame by frame, so a long stem never sits in memory. The stream header is
//! written last, over a placeholder, once the frame sizes are known.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read as _, Seek as _, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr as _, StreamInfo};
use flacenc::error::Verify as _;
use flacenc::source::{Fill as _, FrameBuf};
use md5::{Digest as _, Md5};
use stem_writer::{OUTPUT_RATE_HZ, STEM_WAV_HEADER_BYTES, stem_wav_header};

const BYTES_PER_SAMPLE: usize = 2;
const BITS_PER_SAMPLE: usize = 16;
const CHANNEL_COUNT: usize = 1;
const STREAM_MARKER: &[u8; 4] = b"fLaC";
const STREAM_INFO_BYTES: usize = 34;
// The stream info is the last metadata block: type 0 with the last-block bit set.
const STREAM_INFO_BLOCK_HEADER: [u8; 4] = [0x80, 0x00, 0x00, 0x22];

#[derive(Debug, thiserror::Error)]
pub enum FlacError {
    #[error("could not read or write {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The stem's header is not the one the stem writer writes for its length, so FLAC could
    /// not rebuild it byte for byte.
    #[error("{} is not a stem as the stem writer writes it: {reason}", path.display())]
    NotAStem { path: PathBuf, reason: &'static str },
    #[error("could not encode {} as FLAC: {message}", path.display())]
    Encode { path: PathBuf, message: String },
    #[error("could not decode {}: {message}", path.display())]
    Decode { path: PathBuf, message: String },
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> FlacError + '_ {
    move |source| FlacError::Io {
        path: path.to_owned(),
        source,
    }
}

/// The byte count of the stem's samples, after checking its header is the stem writer's.
///
/// # Errors
///
/// Returns `FlacError::NotAStem` for any other header or an odd sample length.
pub(crate) fn stem_data_bytes(wav_path: &Path) -> Result<u32, FlacError> {
    let not_a_stem = |reason| FlacError::NotAStem {
        path: wav_path.to_owned(),
        reason,
    };
    let mut file = File::open(wav_path).map_err(io_error(wav_path))?;
    let file_bytes = file.metadata().map_err(io_error(wav_path))?.len();
    let data_bytes = file_bytes
        .checked_sub(u64::from(STEM_WAV_HEADER_BYTES))
        .ok_or_else(|| not_a_stem("shorter than its header"))?;
    let data_bytes =
        u32::try_from(data_bytes).map_err(|_| not_a_stem("longer than a WAV can count"))?;
    if data_bytes % 2 != 0 {
        return Err(not_a_stem("a partial sample at the end"));
    }
    let mut header = [0_u8; STEM_WAV_HEADER_BYTES as usize];
    file.read_exact(&mut header).map_err(io_error(wav_path))?;
    if header != stem_wav_header(data_bytes) {
        return Err(not_a_stem("its header does not count its samples"));
    }
    Ok(data_bytes)
}

/// Encodes the stem to `flac_path` through a partial file, synced, then renamed.
///
/// # Errors
///
/// Returns `FlacError` when the stem is not as the stem writer writes it, or a file fails.
pub(crate) fn encode_stem(wav_path: &Path, flac_path: &Path) -> Result<(), FlacError> {
    let data_bytes = stem_data_bytes(wav_path)?;
    let encode_error = |message: String| FlacError::Encode {
        path: wav_path.to_owned(),
        message,
    };
    // Without the `par` feature the encoder runs on this thread alone.
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, error)| encode_error(error.to_string()))?;
    let block_samples = config.block_size;
    let total_samples = data_bytes as usize / BYTES_PER_SAMPLE;
    let rate_hz = OUTPUT_RATE_HZ as usize;
    let mut stream_header = StreamInfo::new(rate_hz, CHANNEL_COUNT, BITS_PER_SAMPLE)
        .map_err(|error| encode_error(error.to_string()))?;
    stream_header
        .set_block_sizes(block_samples, block_samples)
        .map_err(|error| encode_error(error.to_string()))?;

    let mut wav = BufReader::new(File::open(wav_path).map_err(io_error(wav_path))?);
    wav.seek(SeekFrom::Start(u64::from(STEM_WAV_HEADER_BYTES)))
        .map_err(io_error(wav_path))?;
    let partial_path = partial_path_of(flac_path);
    let mut flac = BufWriter::new(File::create(&partial_path).map_err(io_error(&partial_path))?);
    flac.write_all(STREAM_MARKER)
        .and_then(|()| flac.write_all(&STREAM_INFO_BLOCK_HEADER))
        .and_then(|()| flac.write_all(&[0_u8; STREAM_INFO_BYTES]))
        .map_err(io_error(&partial_path))?;

    let mut frame_buf = FrameBuf::with_size(CHANNEL_COUNT, block_samples)
        .map_err(|error| encode_error(error.to_string()))?;
    let mut block = vec![0_u8; block_samples * BYTES_PER_SAMPLE];
    let mut sample_md5 = Md5::new();
    let mut frame_sink = ByteSink::new();
    let mut frame_bytes_range: Option<(usize, usize)> = None;
    let mut read_samples = 0_usize;
    let mut frame_number = 0_usize;
    while read_samples < total_samples {
        let samples = block_samples.min(total_samples - read_samples);
        let block = &mut block[..samples * BYTES_PER_SAMPLE];
        wav.read_exact(block).map_err(io_error(wav_path))?;
        // FLAC's MD5 is of the samples as little-endian bytes, which for a 16-bit mono stem
        // is the WAV's data chunk.
        sample_md5.update(&*block);
        frame_buf
            .fill_le_bytes(block, BYTES_PER_SAMPLE)
            .map_err(|error| encode_error(error.to_string()))?;
        let frame =
            flacenc::encode_fixed_size_frame(&config, &frame_buf, frame_number, &stream_header)
                .map_err(|error| encode_error(error.to_string()))?;
        frame_sink.clear();
        frame
            .write(&mut frame_sink)
            .map_err(|error| encode_error(error.to_string()))?;
        let frame_bytes = frame_sink.as_slice().len();
        frame_bytes_range = Some(
            frame_bytes_range.map_or((frame_bytes, frame_bytes), |range| {
                (range.0.min(frame_bytes), range.1.max(frame_bytes))
            }),
        );
        flac.write_all(frame_sink.as_slice())
            .map_err(io_error(&partial_path))?;
        read_samples += samples;
        frame_number += 1;
    }

    // Zero frame sizes mean unknown, which a stem with no samples has.
    let (min_frame_bytes, max_frame_bytes) = frame_bytes_range.unwrap_or((0, 0));
    stream_header
        .set_frame_sizes(min_frame_bytes, max_frame_bytes)
        .map_err(|error| encode_error(error.to_string()))?;
    stream_header.set_total_samples(total_samples);
    stream_header.set_md5_digest(&sample_md5.finalize().into());
    let mut header_sink = ByteSink::new();
    stream_header
        .write(&mut header_sink)
        .map_err(|error| encode_error(error.to_string()))?;
    let mut flac = flac
        .into_inner()
        .map_err(|error| io_error(&partial_path)(error.into_error()))?;
    flac.seek(SeekFrom::Start(
        (STREAM_MARKER.len() + STREAM_INFO_BLOCK_HEADER.len()) as u64,
    ))
    .and_then(|_| flac.write_all(header_sink.as_slice()))
    .and_then(|()| flac.sync_all())
    .map_err(io_error(&partial_path))?;
    drop(flac);
    std::fs::rename(&partial_path, flac_path).map_err(io_error(flac_path))?;
    if let Some(folder) = flac_path.parent() {
        call_folder::sync_folder(folder).map_err(io_error(folder))?;
    }
    Ok(())
}

/// Decodes a stem's FLAC into `wav`, the stem writer's header first, and returns the MD5 of the
/// bytes written as lowercase hex.
///
/// # Errors
///
/// Returns `FlacError::Decode` for a FLAC that is not 16 kHz mono 16-bit, does not count its
/// samples, or holds another count than it says, and `FlacError::Io` when `wav` fails.
pub(crate) fn decode_stem(
    flac_path: &Path,
    wav: &mut impl Write,
    wav_path: &Path,
) -> Result<String, FlacError> {
    let decode_error = |message: String| FlacError::Decode {
        path: flac_path.to_owned(),
        message,
    };
    let mut reader =
        claxon::FlacReader::open(flac_path).map_err(|error| decode_error(error.to_string()))?;
    let stream_header = reader.streaminfo();
    if stream_header.sample_rate != OUTPUT_RATE_HZ
        || stream_header.channels != 1
        || stream_header.bits_per_sample != 16
    {
        return Err(decode_error(format!(
            "it is {} Hz, {} channels, {} bits; a stem is {OUTPUT_RATE_HZ} Hz, 1 channel, 16 bits",
            stream_header.sample_rate, stream_header.channels, stream_header.bits_per_sample
        )));
    }
    // FLAC writes a count of 0 as unknown, which is what an empty stem's FLAC says.
    let total_samples = match stream_header.samples {
        Some(total_samples) => total_samples,
        None => count_samples(flac_path)?,
    };
    let data_bytes = total_samples
        .checked_mul(BYTES_PER_SAMPLE as u64)
        .and_then(|bytes| u32::try_from(bytes).ok())
        .filter(|bytes| bytes.checked_add(STEM_WAV_HEADER_BYTES).is_some())
        .ok_or_else(|| decode_error(format!("{total_samples} samples do not fit a WAV")))?;
    let mut rebuilt_md5 = Md5::new();
    let header = stem_wav_header(data_bytes);
    rebuilt_md5.update(header);
    wav.write_all(&header).map_err(io_error(wav_path))?;
    let mut decoded_samples = 0_u64;
    let mut frames = reader.blocks();
    let mut buffer = Vec::new();
    let mut sample_bytes = Vec::new();
    while let Some(block) = frames
        .read_next_or_eof(buffer)
        .map_err(|error| decode_error(error.to_string()))?
    {
        sample_bytes.clear();
        for &sample in block.channel(0) {
            let sample = i16::try_from(sample)
                .map_err(|_| decode_error(format!("sample {sample} is not 16-bit")))?;
            sample_bytes.extend_from_slice(&sample.to_le_bytes());
        }
        rebuilt_md5.update(&sample_bytes);
        wav.write_all(&sample_bytes).map_err(io_error(wav_path))?;
        decoded_samples += u64::from(block.duration());
        buffer = block.into_buffer();
    }
    if decoded_samples != total_samples {
        return Err(decode_error(format!(
            "it says {total_samples} samples and holds {decoded_samples}"
        )));
    }
    Ok(crate::drive::hex(&rebuilt_md5.finalize()))
}

/// The samples of a FLAC that does not count them, by decoding it.
fn count_samples(flac_path: &Path) -> Result<u64, FlacError> {
    let decode_error = |error: claxon::Error| FlacError::Decode {
        path: flac_path.to_owned(),
        message: error.to_string(),
    };
    let mut reader = claxon::FlacReader::open(flac_path).map_err(decode_error)?;
    let mut frames = reader.blocks();
    let mut buffer = Vec::new();
    let mut sample_count = 0_u64;
    while let Some(block) = frames.read_next_or_eof(buffer).map_err(decode_error)? {
        sample_count += u64::from(block.duration());
        buffer = block.into_buffer();
    }
    Ok(sample_count)
}

/// `001-mic.flac` is written as `.001-mic.flac.partial` beside it.
fn partial_path_of(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.partial"))
}

/// Writes a rebuilt stem to `wav_path` through a partial file, synced, then renamed, and
/// returns its MD5.
///
/// # Errors
///
/// As `decode_stem`, and `FlacError::Io` when the file cannot be written or renamed.
pub(crate) fn restore_stem(
    flac_path: &Path,
    wav_path: &Path,
    expected_md5: &str,
) -> Result<RestoredStem, FlacError> {
    let partial_path = partial_path_of(wav_path);
    let mut wav = BufWriter::new(File::create(&partial_path).map_err(io_error(&partial_path))?);
    let rebuilt_md5 = decode_stem(flac_path, &mut wav, &partial_path)?;
    let wav = wav
        .into_inner()
        .map_err(|error| io_error(&partial_path)(error.into_error()))?;
    if rebuilt_md5 != expected_md5 {
        drop(wav);
        std::fs::remove_file(&partial_path).map_err(io_error(&partial_path))?;
        return Ok(RestoredStem::Mismatch { rebuilt_md5 });
    }
    wav.sync_all().map_err(io_error(&partial_path))?;
    drop(wav);
    std::fs::rename(&partial_path, wav_path).map_err(io_error(wav_path))?;
    if let Some(folder) = wav_path.parent() {
        call_folder::sync_folder(folder).map_err(io_error(folder))?;
    }
    Ok(RestoredStem::Written)
}

/// Whether a rebuilt stem matched the original's MD5.
pub(crate) enum RestoredStem {
    Written,
    /// Nothing was written.
    Mismatch {
        rebuilt_md5: String,
    },
}
