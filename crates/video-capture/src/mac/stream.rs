//! One ScreenCaptureKit stream of one window, through `SCContentFilter(desktopIndependentWindow:)`
//! so windows that cover it do not show. Frames arrive on the stream's own serial queue and go to
//! the caller's sink with their host time and the window's size in points; a stop arrives the
//! same way, with Loom's split of the stop codes.
//!
//! Unsafe code: the output and delegate classes this module defines are called from Objective-C,
//! and `SharedWindow` and `CapturedFrame` move framework objects across threads.
#![allow(unsafe_code)]
#![expect(
    clippy::transmute_ptr_to_ptr,
    clippy::useless_transmute,
    reason = "cidre's `define_obj_type` and `add_methods` macros transmute object references"
)]

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use cidre::sc::{StreamDelegate as _, StreamOutput as _};
use cidre::{arc, cf, cg, cm, define_obj_type, dispatch, ns, objc, sc};

/// Start, stop and a configuration change each get this long (Loom, Fathom).
pub const STREAM_CALL_LIMIT: Duration = Duration::from_secs(5);

/// Stop codes from `SCStreamErrorDomain`.
const USER_STOPPED_CODE: isize = -3817;
const SYSTEM_STOPPED_CODE: isize = -3821;
const NO_SOURCE_CODE: isize = -3815;

/// A window from a shareable-content snapshot. `SCWindow` is an immutable snapshot object, so it
/// may move to the thread that starts the stream.
#[derive(Clone)]
pub struct SharedWindow(arc::R<sc::Window>);

// SAFETY: SCWindow holds only values read when the snapshot was taken and has no mutating API.
unsafe impl Send for SharedWindow {}

impl SharedWindow {
    pub(crate) fn new(window: &sc::Window) -> Self {
        Self(window.retained())
    }

    #[must_use]
    pub fn id(&self) -> u32 {
        self.0.id()
    }
}

/// One complete frame. The sample buffer is retained until the encoder takes it.
pub struct CapturedFrame {
    sample: arc::R<cm::SampleBuf>,
    /// Host time of the frame, on the same clock as the audio stems.
    pub host_time_ns: u64,
    pub width_px: u32,
    pub height_px: u32,
    /// The window's size in points, from the frame's content rect and scale.
    pub content_size_pt: Option<(f64, f64)>,
}

// SAFETY: a CMSampleBuffer is safe to retain and read from any thread, and its pixel buffer is
// not written after ScreenCaptureKit hands it over.
unsafe impl Send for CapturedFrame {}

impl CapturedFrame {
    pub(crate) fn image(&self) -> Option<&cidre::cv::ImageBuf> {
        self.sample.image_buf()
    }
}

/// Why a stream stopped on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamStop {
    /// The user ended it through the system's sharing controls. Video ends for the segment.
    ByUser,
    /// The system stopped it. A rebuild may work.
    BySystem,
    /// The window went away.
    NoSource,
    Other {
        code: isize,
        message: String,
    },
}

impl StreamStop {
    fn from_error(error: &ns::Error) -> Self {
        match error.code() {
            USER_STOPPED_CODE => Self::ByUser,
            SYSTEM_STOPPED_CODE => Self::BySystem,
            NO_SOURCE_CODE => Self::NoSource,
            code => Self::Other {
                code,
                message: error.localized_desc().to_string(),
            },
        }
    }
}

impl std::fmt::Display for StreamStop {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ByUser => formatter.write_str("stopped by the user (-3817)"),
            Self::BySystem => formatter.write_str("stopped by the system (-3821)"),
            Self::NoSource => formatter.write_str("the window is gone (-3815)"),
            Self::Other { code, message } => write!(formatter, "stopped with {code}: {message}"),
        }
    }
}

/// What the stream hands its sink.
pub enum StreamEvent {
    Frame(CapturedFrame),
    /// A frame with nothing new: the window is there and unchanged. Not a hang.
    Idle,
    /// The stream delivered a blank or suspended frame: the window is hidden or the screen is
    /// locked.
    Hidden,
    Stopped(StreamStop),
}

pub type StreamSink = Arc<dyn Fn(StreamEvent) + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("{operation} did not finish within {limit_ms} ms")]
    TimedOut {
        operation: &'static str,
        limit_ms: u128,
    },
    #[error("{operation} dropped its completion without calling it")]
    NoAnswer { operation: &'static str },
    #[error("{operation} failed with {code}: {message}")]
    Failed {
        operation: &'static str,
        code: isize,
        message: String,
    },
    #[error("could not add the stream output: {message}")]
    OutputNotAdded { message: String },
}

impl StreamError {
    /// Whether the stream refused for the reason the user's own stop gives.
    #[must_use]
    pub fn is_user_stop(&self) -> bool {
        matches!(self, Self::Failed { code, .. } if *code == USER_STOPPED_CODE)
    }

    /// The call did not answer in time, so the stream may still run.
    #[must_use]
    pub fn is_timed_out(&self) -> bool {
        matches!(self, Self::TimedOut { .. })
    }
}

/// The frame size the stream delivers and the encoder takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputSize {
    pub width_px: u32,
    pub height_px: u32,
}

pub struct FrameSinkState {
    sink: StreamSink,
}

define_obj_type!(
    FrameSink + sc::StreamOutputImpl,
    FrameSinkState,
    MC_VIDEO_FRAME_SINK
);

impl sc::StreamOutput for FrameSink {}

#[objc::add_methods]
impl sc::StreamOutputImpl for FrameSink {
    extern "C" fn impl_stream_did_output_sample_buf(
        &mut self,
        _cmd: Option<&objc::Sel>,
        _stream: &sc::Stream,
        sample_buf: &mut cm::SampleBuf,
        kind: sc::OutputType,
    ) {
        match kind {
            sc::OutputType::Screen => {}
            sc::OutputType::Audio | sc::OutputType::Mic => return,
        }
        let event = read_frame(sample_buf);
        (self.inner().sink)(event);
    }
}

pub struct StopWatchState {
    sink: StreamSink,
}

define_obj_type!(
    StopWatch + sc::StreamDelegateImpl,
    StopWatchState,
    MC_VIDEO_STOP_WATCH
);

impl sc::StreamDelegate for StopWatch {}

#[objc::add_methods]
impl sc::StreamDelegateImpl for StopWatch {
    extern "C" fn impl_stream_did_stop_with_err(
        &mut self,
        _cmd: Option<&objc::Sel>,
        _stream: &sc::Stream,
        error: &ns::Error,
    ) {
        (self.inner().sink)(StreamEvent::Stopped(StreamStop::from_error(error)));
    }
}

/// The frame's status and, for a complete frame, the frame itself.
fn read_frame(sample_buf: &cm::SampleBuf) -> StreamEvent {
    let info = sample_buf
        .attaches(false)
        .and_then(|attachments| attachments.iter().next());
    let status = info
        .and_then(|info| info.get(sc::FrameInfo::status().as_cf()))
        .and_then(cf::Plist::try_as_number)
        .and_then(cf::Number::to_i64);
    // The frame status values, in SCFrameStatus order.
    match status {
        Some(0) => {}
        Some(2 | 3) => return StreamEvent::Hidden,
        // Idle, started and stopped frames carry no picture; a missing status is unreadable.
        Some(_) | None => return StreamEvent::Idle,
    }
    let Some(image) = sample_buf.image_buf() else {
        return StreamEvent::Idle;
    };
    let size = image.encoded_size();
    let content_rect = info
        .and_then(|info| info.get(sc::FrameInfo::content_rect().as_cf()))
        .and_then(cf::Plist::try_as_raw_dictionary)
        .and_then(cg::Rect::from_dictionary_representation);
    let content_scale = info
        .and_then(|info| info.get(sc::FrameInfo::content_scale().as_cf()))
        .and_then(cf::Plist::try_as_number)
        .and_then(cf::Number::to_f64)
        .filter(|scale| *scale > 0.0);
    let content_size_pt = content_rect
        .zip(content_scale)
        .map(|(rect, scale)| (rect.size.width / scale, rect.size.height / scale));
    StreamEvent::Frame(CapturedFrame {
        sample: sample_buf.retained(),
        host_time_ns: host_time_ns(sample_buf.pts()),
        width_px: whole_px(size.width),
        height_px: whole_px(size.height),
        content_size_pt,
    })
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "frame sizes are small positive numbers"
)]
fn whole_px(size: f64) -> u32 {
    size.max(0.0).round() as u32
}

/// A host-clock `CMTime` in nanoseconds.
pub(crate) fn host_time_ns(time: cm::Time) -> u64 {
    if time.scale <= 0 {
        return 0;
    }
    let nanos = i128::from(time.value) * 1_000_000_000 / i128::from(time.scale);
    u64::try_from(nanos).unwrap_or(0)
}

/// A host time in nanoseconds as a `CMTime` on the host clock's nanosecond scale.
pub(crate) fn host_cm_time(host_time_ns: u64) -> cm::Time {
    cm::Time::new(
        i64::try_from(host_time_ns).unwrap_or(i64::MAX),
        1_000_000_000,
    )
}

/// The stream and the objects it calls back, which must live as long as it runs.
struct StreamParts {
    stream: arc::R<sc::Stream>,
    _output: arc::R<FrameSink>,
    _stop_watch: arc::R<StopWatch>,
    _queue: arc::R<dispatch::Queue>,
}

impl StreamParts {
    /// Stops a stream nobody waits for, and keeps its objects until the stop completes, so no
    /// callback reaches a freed object.
    fn stop_unwatched(self) {
        let stream = self.stream.clone();
        let mut kept = Some(self);
        stream.stop_with_ch(move |error| {
            if let Some(error) = error {
                tracing::debug!(exception.message = %error.localized_desc(), "video.unwatched_stop_failed");
            }
            drop(kept.take());
        });
    }
}

/// A running stream. Dropping it leaves the stream running; call `stop`.
pub struct WindowStream {
    parts: StreamParts,
    pub size: OutputSize,
}

impl WindowStream {
    /// Starts a stream of `window` at `size`, 15 fps, 420v, no audio, no cursor.
    ///
    /// # Errors
    ///
    /// Fails when the output cannot be added, or the start fails or takes over 5 s. A start that
    /// completes after the 5 s is stopped then.
    pub fn start(
        window: &SharedWindow,
        size: OutputSize,
        sink: &StreamSink,
    ) -> Result<Self, StreamError> {
        let filter = sc::ContentFilter::with_desktop_independent_window(&window.0);
        let configuration = configuration(size);
        let stop_watch = StopWatch::with(StopWatchState {
            sink: Arc::clone(sink),
        });
        let stream = sc::Stream::with_delegate(&filter, &configuration, stop_watch.as_ref());
        let output = FrameSink::with(FrameSinkState {
            sink: Arc::clone(sink),
        });
        let queue = dispatch::Queue::serial_with_ar_pool();
        stream
            .add_stream_output(output.as_ref(), sc::OutputType::Screen, Some(&queue))
            .map_err(|error| StreamError::OutputNotAdded {
                message: error.localized_desc().to_string(),
            })?;
        let parts = StreamParts {
            stream,
            _output: output,
            _stop_watch: stop_watch,
            _queue: queue,
        };
        // Holds the stream once the wait gives up, so a late start can stop it. The completion
        // locks it before it answers, so the two sides cannot miss each other.
        let abandoned: Arc<Mutex<Option<StreamParts>>> = Arc::new(Mutex::new(None));
        let late = Arc::clone(&abandoned);
        let (done, outcome) = mpsc::sync_channel(1);
        parts.stream.start_with_ch(move |error| {
            let mut late = late.lock().unwrap_or_else(PoisonError::into_inner);
            let _unheard = done.try_send(error.map(failure));
            if let Some(parts) = late.take()
                && error.is_none()
            {
                tracing::warn!("video.late_start_stopped");
                parts.stop_unwatched();
            }
        });
        let answer = match outcome.recv_timeout(STREAM_CALL_LIMIT) {
            Ok(answer) => Some(answer),
            Err(RecvTimeoutError::Timeout) => {
                let mut slot = abandoned.lock().unwrap_or_else(PoisonError::into_inner);
                // The completion may have answered between the timeout and the lock.
                let answer = outcome.try_recv().ok();
                if answer.is_none() {
                    *slot = Some(parts);
                    return Err(StreamError::TimedOut {
                        operation: "start",
                        limit_ms: STREAM_CALL_LIMIT.as_millis(),
                    });
                }
                answer
            }
            Err(RecvTimeoutError::Disconnected) => None,
        };
        match answer {
            Some(None) => Ok(Self { parts, size }),
            Some(Some((code, message))) => Err(StreamError::Failed {
                operation: "start",
                code,
                message,
            }),
            None => Err(StreamError::NoAnswer { operation: "start" }),
        }
    }

    /// Changes the frame size the stream delivers.
    ///
    /// # Errors
    ///
    /// Fails when the change fails or takes over 5 s.
    pub fn resize(&mut self, size: OutputSize) -> Result<(), StreamError> {
        let (done, outcome) = mpsc::sync_channel(1);
        self.parts
            .stream
            .update_cfg_ch(&configuration(size), move |error| {
                let _unheard = done.try_send(error.map(failure));
            });
        wait("configuration change", &outcome, STREAM_CALL_LIMIT)?;
        self.size = size;
        Ok(())
    }

    /// Stops the stream, waiting at most `limit`.
    ///
    /// # Errors
    ///
    /// Fails when the stop fails or takes longer. The stream's objects live until the stop
    /// completes either way.
    pub fn stop(self, limit: Duration) -> Result<(), StreamError> {
        let (done, outcome) = mpsc::sync_channel(1);
        let stream = self.parts.stream.clone();
        let mut kept = Some(self.parts);
        stream.stop_with_ch(move |error| {
            let _unheard = done.try_send(error.map(failure));
            drop(kept.take());
        });
        wait("stop", &outcome, limit)
    }
}

fn failure(error: &ns::Error) -> (isize, String) {
    (error.code(), error.localized_desc().to_string())
}

fn wait(
    operation: &'static str,
    outcome: &mpsc::Receiver<Option<(isize, String)>>,
    limit: Duration,
) -> Result<(), StreamError> {
    match outcome.recv_timeout(limit) {
        Ok(None) => Ok(()),
        Ok(Some((code, message))) => Err(StreamError::Failed {
            operation,
            code,
            message,
        }),
        Err(RecvTimeoutError::Timeout) => Err(StreamError::TimedOut {
            operation,
            limit_ms: limit.as_millis(),
        }),
        Err(RecvTimeoutError::Disconnected) => Err(StreamError::NoAnswer { operation }),
    }
}

fn configuration(size: OutputSize) -> arc::R<sc::StreamCfg> {
    let mut configuration = sc::StreamCfg::new();
    configuration.set_width(size.width_px as usize);
    configuration.set_height(size.height_px as usize);
    // SCK delivers on display refreshes at or past the interval. At exactly 1/15 s, a frame due
    // on the 4th 60 Hz refresh often just misses it and waits for the 5th: 13.4 fps measured.
    // 1/16 s keeps it on the 4th.
    configuration.set_minimum_frame_interval(cm::Time::new(1, crate::mac::FRAMES_PER_SECOND + 1));
    configuration.set_queue_depth(8);
    configuration.set_pixel_format(cidre::cv::PixelFormat::_420V);
    configuration.set_shows_cursor(false);
    configuration.set_captures_audio(false);
    configuration.set_scales_to_fit(true);
    // SCK hands over point-size frames, not Retina pixels, so it scales less (Loom).
    configuration.set_capture_resolution(sc::CaptureResolution::Nominal);
    configuration
}
