//! 16 kHz mono 16-bit PCM WAV, the encoding the Wispr transcriber takes. Larger than Opus, but
//! the engine sends it the way the desktop app does.

use std::io::Cursor;

use crate::SttError;
use crate::ogg_opus::{EncodedAudio, STEM_RATE_HZ, hex_sha256};

/// `samples`, 16 kHz mono, as a WAV file.
///
/// # Errors
///
/// Returns `SttError::Encode` when the WAV cannot be written.
pub(super) fn encode(samples: &[i16]) -> Result<EncodedAudio, SttError> {
    let encode_error = |detail: String| SttError::Encode { detail };
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: STEM_RATE_HZ,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::new());
    let mut writer = hound::WavWriter::new(&mut cursor, spec)
        .map_err(|error| encode_error(error.to_string()))?;
    for &sample in samples {
        writer
            .write_sample(sample)
            .map_err(|error| encode_error(error.to_string()))?;
    }
    writer
        .finalize()
        .map_err(|error| encode_error(error.to_string()))?;
    let bytes = cursor.into_inner();
    let sha256 = hex_sha256(&bytes);
    Ok(EncodedAudio { bytes, sha256 })
}
