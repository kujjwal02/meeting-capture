//! VideoToolbox in hardware, with the settings spike 3c.1 checked: HEVC Main, real time, no
//! frame reordering, open GOP off, BT.709, 700 kbps average with one-second bursts capped at 2x.
//! The caller forces each keyframe, so fragments cut on time. When the readback shows no hardware
//! HEVC encoder, or open GOP still on, the session falls back to H.264 High.
//!
//! Unsafe code: `EncodedFrame` moves a compressed sample buffer to the writer's thread, and
//! `FlushedSession` moves the session to the thread that flushes it.
#![allow(unsafe_code)]

use std::sync::{Arc, mpsc};
use std::time::Duration;

use cidre::{arc, cf, cm, cv, os, vt};

use crate::VideoCodec;
use crate::mac::stream::{CapturedFrame, OutputSize, host_cm_time};

pub const AVERAGE_BIT_RATE_BPS: i32 = 700_000;

pub struct EncodedFrame {
    sample: arc::R<cm::SampleBuf>,
    pub host_time_ns: u64,
    pub is_keyframe: bool,
}

// SAFETY: a finished compressed sample buffer is immutable, so any thread may read it.
unsafe impl Send for EncodedFrame {}

impl EncodedFrame {
    pub(crate) fn sample(&self) -> &cm::SampleBuf {
        &self.sample
    }
}

pub enum EncoderOutput {
    Frame(EncodedFrame),
    /// The encoder dropped the frame or failed on it.
    Dropped {
        status: i32,
    },
}

pub type EncoderSink = Arc<dyn Fn(EncoderOutput) + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum EncoderError {
    #[error("could not create the {codec} session at {width}x{height}: {status}")]
    Create {
        codec: VideoCodec,
        width: u32,
        height: u32,
        status: i32,
    },
    #[error("could not set {property}: {status}")]
    Property { property: &'static str, status: i32 },
    #[error("could not encode a frame: {status}")]
    Encode { status: i32 },
    #[error("the frame holds no picture")]
    NoPicture,
    #[error("the flush took over {limit_ms} ms; the frames still in the encoder are lost")]
    FlushTimedOut { limit_ms: u128 },
    #[error("could not start the flush thread: {message}")]
    FlushNotStarted { message: String },
}

/// The session, sent to the thread that waits for its flush.
struct FlushedSession(arc::R<vt::CompressionSession>);

// SAFETY: VideoToolbox lets any thread call a compression session's functions.
unsafe impl Send for FlushedSession {}

/// What the session reports about itself after setup.
#[derive(Debug, Clone)]
pub struct EncoderReadback {
    pub codec: VideoCodec,
    pub is_hardware: Option<bool>,
    pub is_open_gop_allowed: Option<bool>,
    pub encoder_id: Option<String>,
}

pub struct Encoder {
    session: arc::R<vt::CompressionSession>,
    pub size: OutputSize,
    pub readback: EncoderReadback,
    force_keyframe: arc::R<cf::Dictionary>,
    sink: EncoderSink,
}

impl Encoder {
    /// HEVC when the hardware encoder takes the settings, else H.264.
    ///
    /// # Errors
    ///
    /// Fails when neither codec gives a session.
    pub fn new(size: OutputSize, sink: EncoderSink) -> Result<Self, EncoderError> {
        match Self::with_codec(VideoCodec::Hevc, size, Arc::clone(&sink)) {
            Ok(encoder)
                if encoder.readback.is_hardware != Some(false)
                    && encoder.readback.is_open_gop_allowed != Some(true) =>
            {
                return Ok(encoder);
            }
            Ok(encoder) => tracing::warn!(
                video.is_hardware = encoder.readback.is_hardware,
                video.is_open_gop_allowed = encoder.readback.is_open_gop_allowed,
                "video.hevc_unsuitable"
            ),
            Err(error) => {
                tracing::warn!(exception.message = %error, "video.hevc_unavailable");
            }
        }
        Self::with_codec(VideoCodec::H264, size, sink)
    }

    fn with_codec(
        codec: VideoCodec,
        size: OutputSize,
        sink: EncoderSink,
    ) -> Result<Self, EncoderError> {
        let spec = cf::Dictionary::with_keys_values(
            &[vt::video_encoder_spec_keys::require_hw_accelerated_video_encoder()],
            &[cf::Boolean::value_true()],
        );
        let source_attributes = cf::Dictionary::with_keys_values(
            &[
                cv::pixel_buffer_keys::pixel_format(),
                cv::pixel_buffer_keys::width(),
                cv::pixel_buffer_keys::height(),
            ],
            &[
                &cf::Number::from_i32(i32::from_be_bytes(*b"420v")),
                &cf::Number::from_i64(i64::from(size.width_px)),
                &cf::Number::from_i64(i64::from(size.height_px)),
            ],
        );
        let codec_type = match codec {
            VideoCodec::Hevc => cm::VideoCodec::HEVC,
            VideoCodec::H264 => cm::VideoCodec::H264,
        };
        let created = vt::CompressionSession::new::<std::ffi::c_void>(
            size.width_px,
            size.height_px,
            codec_type,
            spec.as_deref(),
            source_attributes.as_deref(),
            None,
            None,
            std::ptr::null_mut(),
        );
        let mut session = created.map_err(|status| EncoderError::Create {
            codec,
            width: size.width_px,
            height: size.height_px,
            status: status.0.into(),
        })?;
        set_properties(&mut session, codec)?;
        let readback = read_back(&session, codec);
        prepare(&mut session)?;
        let force_keyframe = cf::Dictionary::with_keys_values(
            &[vt::compression::properties::frame_keys::force_key_frame()],
            &[cf::Boolean::value_true()],
        )
        .ok_or(EncoderError::Property {
            property: "ForceKeyFrame",
            status: -1,
        })?;
        tracing::info!(
            video.codec = %codec,
            video.width_px = size.width_px,
            video.height_px = size.height_px,
            video.is_hardware = readback.is_hardware,
            video.is_open_gop_allowed = readback.is_open_gop_allowed,
            video.encoder_id = readback.encoder_id.as_deref(),
            "video.encoder_ready"
        );
        Ok(Self {
            session,
            size,
            readback,
            force_keyframe,
            sink,
        })
    }

    /// Encodes the frame's picture at `host_time_ns`, which differs from the frame's own time
    /// for a repeat. The output goes to the sink.
    ///
    /// # Errors
    ///
    /// Fails when VideoToolbox refuses the frame.
    pub fn encode(
        &mut self,
        frame: &CapturedFrame,
        host_time_ns: u64,
        is_keyframe_forced: bool,
    ) -> Result<(), EncoderError> {
        let Some(image) = frame.image() else {
            return Err(EncoderError::NoPicture);
        };
        let sink = Arc::clone(&self.sink);
        let properties = is_keyframe_forced.then_some(self.force_keyframe.as_ref());
        self.session
            .enc_frame_with_output_handler(
                image,
                host_cm_time(host_time_ns),
                cm::Time::new(1, crate::mac::FRAMES_PER_SECOND),
                properties,
                std::ptr::null_mut(),
                move |status: os::Status,
                      flags: vt::EncodeInfoFlags,
                      sample: Option<&cm::SampleBuf>| {
                    let output = match sample {
                        Some(sample)
                            if status.is_ok()
                                && flags.0 & vt::EncodeInfoFlags::FRAME_DROPPED.0 == 0 =>
                        {
                            EncoderOutput::Frame(EncodedFrame {
                                sample: sample.retained(),
                                host_time_ns,
                                is_keyframe: sample.is_key_frame(),
                            })
                        }
                        Some(_) | None => EncoderOutput::Dropped { status: status.0 },
                    };
                    sink(output);
                },
            )
            .map_err(|status| EncoderError::Encode {
                status: status.0.into(),
            })
    }

    /// Hands over every frame still in the encoder, waiting at most `limit`. The flush runs on
    /// its own thread because VideoToolbox gives it no timeout, and a hung encoder must not
    /// hold the segment's close past the app's stop limit.
    ///
    /// # Errors
    ///
    /// Fails when the flush takes longer. The thread keeps the session alive until it returns.
    pub fn flush(&self, limit: Duration) -> Result<(), EncoderError> {
        let session = FlushedSession(self.session.retained());
        let (done, outcome) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("encoder-flush".to_owned())
            .spawn(move || {
                let session = session;
                if let Err(status) = session.0.complete_all() {
                    tracing::warn!(video.status = status.0, "video.encoder_flush_failed");
                }
                let _unheard = done.try_send(());
            })
            .map_err(|error| EncoderError::FlushNotStarted {
                message: error.to_string(),
            })?;
        match outcome.recv_timeout(limit) {
            // A flush thread that died answered nothing more to wait for.
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(EncoderError::FlushTimedOut {
                limit_ms: limit.as_millis(),
            }),
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.session.invalidate();
    }
}

fn set_properties(
    session: &mut vt::CompressionSession,
    codec: VideoCodec,
) -> Result<(), EncoderError> {
    use vt::compression::properties::{keys, profile_level};
    let frames_per_second = crate::mac::FRAMES_PER_SECOND;
    let profile = match codec {
        VideoCodec::Hevc => profile_level::hevc::main_auto_lvl(),
        VideoCodec::H264 => profile_level::h264::high_auto_lvl(),
    };
    let burst_bytes = cf::Number::from_i64(i64::from(AVERAGE_BIT_RATE_BPS) * 2 / 8);
    let burst_window_s = cf::Number::from_f64(1.0);
    let data_rate_limits =
        cf::Array::from_type_refs(&[burst_bytes.as_type_ref(), burst_window_s.as_type_ref()])
            .ok_or(EncoderError::Property {
                property: "DataRateLimits",
                status: -1,
            })?;
    let properties: [(&'static str, &cf::String, &cf::Type); 12] = [
        ("RealTime", keys::real_time(), cf::Boolean::value_true()),
        (
            "AllowFrameReordering",
            keys::allow_frame_reordering(),
            cf::Boolean::value_false(),
        ),
        (
            "AllowOpenGOP",
            keys::allow_open_gop(),
            cf::Boolean::value_false(),
        ),
        ("ProfileLevel", keys::profile_lvl(), profile),
        (
            "MaxKeyFrameInterval",
            keys::max_key_frame_interval(),
            &cf::Number::from_i32(frames_per_second * 2),
        ),
        (
            "MaxKeyFrameIntervalDuration",
            keys::max_key_frame_interval_duration(),
            &cf::Number::from_f64(2.0),
        ),
        (
            "ExpectedFrameRate",
            keys::expected_frame_rate(),
            &cf::Number::from_i32(frames_per_second),
        ),
        (
            "AverageBitRate",
            keys::avarage_bit_rate(),
            &cf::Number::from_i32(AVERAGE_BIT_RATE_BPS),
        ),
        (
            "DataRateLimits",
            keys::data_rate_limits(),
            &data_rate_limits,
        ),
        (
            "ColorPrimaries",
            keys::color_primaries(),
            cv::image_buf_attach::color_primaries::itu_r_709_2(),
        ),
        (
            "TransferFunction",
            keys::transfer_fn(),
            cv::image_buf_attach::transfer_fn::itu_r_709_2(),
        ),
        (
            "YCbCrMatrix",
            keys::ycbcr_matrix(),
            cv::image_buf_attach::ycbcr_matrix::itu_r_709_2(),
        ),
    ];
    for (property, key, value) in properties {
        session
            .set_prop(key, Some(value))
            .map_err(|status| EncoderError::Property {
                property,
                status: status.0.into(),
            })?;
    }
    Ok(())
}

fn prepare(session: &mut vt::CompressionSession) -> Result<(), EncoderError> {
    session.prepare().map_err(|status| EncoderError::Property {
        property: "PrepareToEncodeFrames",
        status: status.0.into(),
    })
}

fn read_back(session: &vt::CompressionSession, codec: VideoCodec) -> EncoderReadback {
    use vt::compression::properties::keys;
    let read_bool = |key: &cf::String| {
        session
            .prop(key)
            .ok()
            .flatten()
            .and_then(|value| value.try_as_boolean().map(cf::Boolean::value))
    };
    // VideoToolbox has the key, but cidre binds no accessor for it.
    let hardware_key = cf::String::from_str("UsingHardwareAcceleratedVideoEncoder");
    EncoderReadback {
        codec,
        is_hardware: read_bool(&hardware_key),
        is_open_gop_allowed: read_bool(keys::allow_open_gop()),
        encoder_id: session
            .prop(keys::encoder_id())
            .ok()
            .flatten()
            .and_then(|value| value.try_as_string().map(ToString::to_string)),
    }
}
