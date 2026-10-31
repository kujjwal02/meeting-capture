//! AVAssetWriter with the Apple HLS profile, writing to no file: it hands each 2 s fragment to a
//! delegate, which passes the bytes on for the segment writer to store. One muxer makes one part,
//! with one init segment, so a new frame size means a new muxer.
//!
//! The writer is built on the content-type initializer that the macOS 27 SDK marks deprecated.
//! Its replacement needs macOS 26, and the minimum is 14.4.
//!
//! Unsafe code: the delegate class this module defines is called from Objective-C, and `Muxer`
//! moves its writer to the thread that owns the segment.
#![allow(unsafe_code)]
#![expect(
    clippy::transmute_ptr_to_ptr,
    clippy::useless_transmute,
    reason = "cidre's `define_obj_type` and `add_methods` macros transmute object references"
)]

use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use cidre::av::AssetWriterDelegate as _;
use cidre::{arc, av, cm, define_obj_type, ns, objc, ut};

use crate::mac::encoder::EncodedFrame;
use crate::mac::stream::{host_cm_time, host_time_ns};

/// The fragment length the writer aims at. A fragment closes at the first keyframe past it.
const FRAGMENT_INTERVAL_S: i64 = 2;

/// What the writer hands over.
pub enum MuxerOutput {
    /// The part's init segment: the `ftyp` and `moov` boxes.
    Init(Vec<u8>),
    Fragment {
        bytes: Vec<u8>,
        first_host_time_ns: Option<u64>,
        duration_ms: f64,
    },
}

pub type MuxerSink = Arc<dyn Fn(MuxerOutput) + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum MuxerError {
    #[error("could not create the writer: {message}")]
    Create { message: String },
    #[error("the writer refused to start: {message}")]
    Start { message: String },
    #[error("the writer refused a frame: {message}")]
    Append { message: String },
    #[error("the writer did not finish within {limit_ms} ms")]
    FinishTimedOut { limit_ms: u128 },
    #[error("the writer dropped its finish completion without calling it")]
    FinishNoAnswer,
}

/// What became of one appended frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxerAppend {
    Appended,
    /// The input was busy, so the frame is gone. Frames that reference it cannot decode.
    Refused,
}

pub struct FragmentSinkState {
    sink: MuxerSink,
}

define_obj_type!(
    FragmentSink + av::AssetWriterDelegateImpl,
    FragmentSinkState,
    MC_VIDEO_FRAGMENT_SINK
);

impl av::AssetWriterDelegate for FragmentSink {}

#[objc::add_methods]
impl av::AssetWriterDelegateImpl for FragmentSink {
    extern "C" fn impl_asset_writer_did_output_segment_data_with_report(
        &mut self,
        _cmd: Option<&objc::Sel>,
        _writer: &av::AssetWriter,
        segment_data: &ns::Data,
        segment_type: av::AssetSegmentType,
        segment_report: Option<&av::AssetSegmentReport>,
    ) {
        let bytes = segment_data.as_slice().to_vec();
        let output = match segment_type {
            av::AssetSegmentType::Initialization => MuxerOutput::Init(bytes),
            av::AssetSegmentType::Separable => {
                let track = segment_report.and_then(|report| {
                    let tracks = report.track_reports();
                    (!tracks.is_empty()).then(|| tracks.get(0).ok()).flatten()
                });
                let first_host_time_ns = track
                    .as_ref()
                    .map(|track| track.earliest_pts())
                    .filter(cm::Time::is_numeric)
                    .map(host_time_ns);
                let duration_ms = track
                    .as_ref()
                    .map(|track| track.duration())
                    .filter(cm::Time::is_numeric)
                    // Microseconds are as fine as the writer's time scale goes.
                    .map_or(0.0, |duration| (duration.as_secs() * 1e6).round() / 1e3);
                MuxerOutput::Fragment {
                    bytes,
                    first_host_time_ns,
                    duration_ms,
                }
            }
        };
        (self.inner().sink)(output);
    }
}

/// A writer for one part, started at its first frame.
pub struct Muxer {
    writer: arc::R<av::AssetWriter>,
    input: arc::R<av::AssetWriterInput>,
    _delegate: arc::R<FragmentSink>,
    pub frames_appended: u64,
    pub frames_refused: u64,
}

// SAFETY: one thread owns the muxer and makes every call on it. AVAssetWriter has no thread
// affinity; it only forbids calls from two threads at once.
unsafe impl Send for Muxer {}

impl Muxer {
    /// Starts a writer whose input takes `first`'s format, with its session at `first`'s time.
    ///
    /// # Errors
    ///
    /// Fails when AVFoundation refuses the writer, its input or the start.
    pub fn start(first: &EncodedFrame, sink: MuxerSink) -> Result<Self, MuxerError> {
        let create_error = |message: String| MuxerError::Create { message };
        let mut writer = av::AssetWriter::with_content_type(ut::Type::mpeg4movie())
            .map_err(|exception| create_error(exception_reason(exception)))?;
        writer.set_output_file_type_profile(Some(av::FileTypeProfile::mpeg4_apple_hls()));
        writer.set_preferred_output_segment_interval(cm::Time::new(FRAGMENT_INTERVAL_S, 1));
        let start = host_cm_time(first.host_time_ns);
        writer.set_initial_segment_start_time(start);
        let delegate = FragmentSink::with(FragmentSinkState { sink });
        writer.set_delegate(Some(delegate.as_ref()));
        let format = first.sample().format_desc();
        let mut input = av::AssetWriterInput::with_media_type_output_settings_source_format_hint(
            av::MediaType::video(),
            None,
            format,
        )
        .map_err(|exception| create_error(exception_reason(exception)))?;
        input.set_expects_media_data_in_real_time(true);
        writer
            .add_input(&input)
            .map_err(|exception| create_error(exception_reason(exception)))?;
        if !writer.start_writing() {
            return Err(MuxerError::Start {
                message: writer_error(&writer),
            });
        }
        writer.start_session_at_src_time(start);
        Ok(Self {
            writer,
            input,
            _delegate: delegate,
            frames_appended: 0,
            frames_refused: 0,
        })
    }

    /// Appends one compressed frame. A busy input drops the frame, which real-time capture
    /// prefers to waiting.
    ///
    /// # Errors
    ///
    /// Fails when the writer has failed.
    pub fn append(&mut self, frame: &EncodedFrame) -> Result<MuxerAppend, MuxerError> {
        if !self.input.is_ready_for_more_media_data() {
            self.frames_refused += 1;
            return Ok(MuxerAppend::Refused);
        }
        match self.input.append_sample_buf(frame.sample()) {
            Ok(true) => {
                self.frames_appended += 1;
                Ok(MuxerAppend::Appended)
            }
            Ok(false) => Err(MuxerError::Append {
                message: writer_error(&self.writer),
            }),
            Err(exception) => Err(MuxerError::Append {
                message: exception_reason(exception),
            }),
        }
    }

    /// Finishes the part. The delegate gets the last fragment before this returns.
    ///
    /// # Errors
    ///
    /// Fails when the writer takes longer than `limit`. The writer then lives until it finishes.
    pub fn finish(mut self, limit: Duration) -> Result<(), MuxerError> {
        self.input.mark_as_finished();
        let (done, finished) = mpsc::sync_channel(1);
        let mut writer = self.writer.clone();
        let mut kept = Some(self);
        writer.finish_writing_with_ch(move || {
            let _unheard = done.try_send(());
            drop(kept.take());
        });
        match finished.recv_timeout(limit) {
            Ok(()) => Ok(()),
            Err(RecvTimeoutError::Timeout) => Err(MuxerError::FinishTimedOut {
                limit_ms: limit.as_millis(),
            }),
            Err(RecvTimeoutError::Disconnected) => Err(MuxerError::FinishNoAnswer),
        }
    }
}

fn writer_error(writer: &av::AssetWriter) -> String {
    writer.error().map_or_else(
        || format!("status {:?}", writer.status()),
        |error| error.localized_desc().to_string(),
    )
}

fn exception_reason(exception: &ns::Exception) -> String {
    exception
        .reason()
        .map_or_else(|| exception.name().to_string(), |reason| reason.to_string())
}
