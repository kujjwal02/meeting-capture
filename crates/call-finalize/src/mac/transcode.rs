//! A part whose codec or size differs from the first part's cannot share its passthrough
//! track: AVFoundation shows a resized part stretched to the first part's size, and refuses an
//! H.264 part after HEVC. Such a part is decoded, letterboxed into the first part's size and
//! encoded again in its codec, in hardware, with the capture's settings.
//!
//! Unsafe code: the letterbox buffer is painted black through its locked base address.
#![allow(unsafe_code)]

use std::sync::{Arc, mpsc};
use std::time::Duration;

use cidre::{arc, cm, cv, ns, vt};
use video_capture::VideoCodec;
use video_capture::mac::encoder::{Encoder, EncoderOutput, EncoderPace, EncoderSink};
use video_capture::mac::stream::OutputSize;

/// The capture forces a keyframe this often, so a re-encoded part seeks like a copied one.
const KEYFRAME_INTERVAL_NS: i128 = 2_000_000_000;
/// Flushing hands over the few frames still in the encoder.
const FLUSH_LIMIT: Duration = Duration::from_secs(10);

/// The first part's codec and size, which every part in the track must share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TrackFormat {
    codec: VideoCodec,
    width_px: u32,
    height_px: u32,
}

impl TrackFormat {
    /// `None` for a codec the capture never writes.
    pub(super) fn of(format: &cm::FormatDesc) -> Option<Self> {
        let codec = match &format.media_sub_type().to_be_bytes() {
            b"hvc1" | b"hev1" => VideoCodec::Hevc,
            b"avc1" | b"avc3" => VideoCodec::H264,
            _ => return None,
        };
        let dims = format.dims();
        Some(Self {
            codec,
            width_px: u32::try_from(dims.width).ok()?,
            height_px: u32::try_from(dims.height).ok()?,
        })
    }
}

/// What the reader asks AVFoundation to decode a part to: the encoder's own input format.
pub(super) fn decoded_frame_settings() -> arc::R<ns::Dictionary<ns::String, ns::Id>> {
    let format = cf_pixel_format_number();
    ns::Dictionary::with_keys_values(
        &[cv::pixel_buffer_keys::pixel_format().as_ns()],
        &[format.as_ns().as_id_ref()],
    )
}

fn cf_pixel_format_number() -> arc::R<cidre::cf::Number> {
    cidre::cf::Number::from_i32(i32::from_be_bytes(*b"420v"))
}

pub(super) struct Transcoder {
    encoder: Encoder,
    transfer: arc::R<vt::PixelTransferSession>,
    encoder_outputs: mpsc::Receiver<EncoderOutput>,
    last_keyframe_ns: Option<i128>,
    pub(super) is_flushed: bool,
}

impl Transcoder {
    pub(super) fn new(format: TrackFormat) -> Result<Self, String> {
        let (sender, encoder_outputs) = mpsc::channel();
        let sink: EncoderSink = Arc::new(move |output| {
            // The receiver is gone only once the part is dropped.
            let _unheard = sender.send(output);
        });
        let size = OutputSize {
            width_px: format.width_px,
            height_px: format.height_px,
        };
        let encoder = Encoder::with_codec(format.codec, size, EncoderPace::Offline, sink)
            .map_err(|error| error.to_string())?;
        let mut transfer = vt::PixelTransferSession::new()
            .map_err(|status| format!("pixel transfer: {status:?}"))?;
        transfer
            .set_scaling_letter_box()
            .map_err(|status| format!("letterbox scaling: {status:?}"))?;
        Ok(Self {
            encoder,
            transfer,
            encoder_outputs,
            last_keyframe_ns: None,
            is_flushed: false,
        })
    }

    /// Letterboxes one decoded frame into the track's size and hands it to the encoder at its
    /// own file time, so the part keeps its spacing.
    pub(super) fn encode(&mut self, decoded: &cm::SampleBuf, pts_ns: i128) -> Result<(), String> {
        let image = decoded
            .image_buf()
            .ok_or_else(|| "a decoded frame holds no picture".to_owned())?;
        let mut boxed = self
            .encoder
            .pixel_buf_pool()
            .ok_or_else(|| "the encoder has no buffer pool".to_owned())?
            .pixel_buf()
            .map_err(|status| format!("letterbox buffer: {status:?}"))?;
        // A pool buffer holds whatever frame it carried last.
        paint_black(&mut boxed)?;
        tag_bt709(&mut boxed);
        self.transfer
            .transfer(image, &boxed)
            .map_err(|status| format!("letterbox: {status:?}"))?;
        let is_keyframe_forced = self
            .last_keyframe_ns
            .is_none_or(|last| pts_ns - last >= KEYFRAME_INTERVAL_NS);
        if is_keyframe_forced {
            self.last_keyframe_ns = Some(pts_ns);
        }
        let time_ns = u64::try_from(pts_ns).map_err(|_| "a frame before 0 s".to_owned())?;
        self.encoder
            .encode_image(&boxed, time_ns, is_keyframe_forced)
            .map_err(|error| error.to_string())
    }

    /// The next encoded frame, if the encoder has handed one over.
    pub(super) fn take_encoded(&mut self) -> Result<Option<arc::R<cm::SampleBuf>>, String> {
        match self.encoder_outputs.try_recv() {
            Ok(EncoderOutput::Frame(frame)) => Ok(Some(frame.sample().retained())),
            Ok(EncoderOutput::Dropped { status }) => {
                Err(format!("the encoder dropped a frame: status {status}"))
            }
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => Ok(None),
        }
    }

    pub(super) fn flush(&mut self) -> Result<(), String> {
        self.is_flushed = true;
        self.encoder
            .flush(FLUSH_LIMIT)
            .map_err(|error| error.to_string())
    }
}

/// The tags the capture's frames carry, so the encoder writes the same colour description.
fn tag_bt709(buffer: &mut cv::PixelBuf) {
    use cv::image_buf_attach::{color_primaries, keys, transfer_fn, ycbcr_matrix};
    let tags = [
        (keys::color_primaries(), color_primaries::itu_r_709_2()),
        (keys::transfer_fn(), transfer_fn::itu_r_709_2()),
        (keys::ycbcr_matrix(), ycbcr_matrix::itu_r_709_2()),
    ];
    for (key, value) in tags {
        buffer.set_attach(key, value, cv::AttachMode::ShouldPropagate);
    }
}

/// Video-range black, so the bars the letterbox leaves stay black.
fn paint_black(buffer: &mut cv::PixelBuf) -> Result<(), String> {
    const LUMA_BLACK: u8 = 16;
    const CHROMA_NEUTRAL: u8 = 128;
    // SAFETY: locked, the planes' memory stays put until the unlock below.
    unsafe {
        buffer
            .lock_base_addr(cv::pixel_buffer::LockFlags::DEFAULT)
            .result()
            .map_err(|status| format!("letterbox buffer lock: {status:?}"))?;
    }
    for (plane, value) in [(0, LUMA_BLACK), (1, CHROMA_NEUTRAL)] {
        let byte_count = buffer.plane_height(plane) * buffer.plane_bytes_per_row(plane);
        let base = buffer.plane_base_address(plane).cast_mut();
        // SAFETY: a locked plane spans its rows times its row stride.
        unsafe { std::ptr::write_bytes(base, value, byte_count) };
    }
    // SAFETY: the lock above, with the same flags.
    unsafe { buffer.unlock_lock_base_addr(cv::pixel_buffer::LockFlags::DEFAULT) }
        .result()
        .map_err(|status| format!("letterbox buffer unlock: {status:?}"))
}
