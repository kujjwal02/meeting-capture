//! `mc-capture video`: the second process the app supervises. It records one window per segment
//! through ScreenCaptureKit, encodes it in hardware, and writes 2 s HLS fragments with
//! `video.json`, in the `capture-protocol` shapes: commands on stdin, reports on stdout, logs on
//! stderr. Closing stdin stops the segment, then the process exits.
//!
//! One thread owns the stream, the encoder, the muxer and the segment writer. The frameworks'
//! callbacks only send to channels, which the loop drains every 20 ms. Frames carry their host
//! time. When no new frame comes for 200 ms, the loop repeats the last one on the 15 fps grid, so
//! a hidden or missing window leaves a still picture and a gap in `video.json`, not a hole.
//!
//! The app stops the segment when the audio process reports a sleep, and starts the next one at
//! wake, so the stream never runs across a sleep.

use std::collections::VecDeque;
use std::io::{BufRead as _, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use capture::{PermissionService, PermissionState};
use capture_protocol::{
    Platform, VideoCommand, VideoErrorKind, VideoGapCause, VideoReport, WindowChangeCause,
    WindowHint, WindowRecord,
};
use video_capture::mac::encoder::{Encoder, EncoderOutput, EncoderSink};
use video_capture::mac::muxer::{Muxer, MuxerAppend, MuxerOutput, MuxerSink};
use video_capture::mac::stream::{
    CapturedFrame, OutputSize, STREAM_CALL_LIMIT, SharedWindow, StreamEvent, StreamSink,
    StreamStop, WindowStream,
};
use video_capture::mac::window::{LOOKUP_LIMIT, WindowLookup, look_up_window};
use video_capture::{
    FrameCounts, ListedWindow, SEGMENT_CLOSE_LIMIT, SegmentStart, VideoCodec, VideoGapRecord,
    VideoSegmentWriter, VideoWriterError, WindowChangeRecord,
};

const TICK: Duration = Duration::from_millis(20);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// No callback for this long after a complete frame, while the window is on screen, means the
/// stream hangs (Loom). A hidden window is checked on the same timer, but never rebuilt.
const STALL_LIMIT: Duration = Duration::from_secs(5);
const REBUILD_LIMIT: u32 = 4;
/// Frames flowing this long after a rebuild end the episode, so the count starts again.
const REBUILD_EPISODE_RESET: Duration = Duration::from_secs(30);
const LOOKUP_INTERVAL: Duration = Duration::from_secs(1);
/// A new window size must hold this long before the stream follows it, so a drag makes one part.
const RESIZE_SETTLE: Duration = Duration::from_secs(1);
/// How long a part switch waits for the muxer's last fragment. A close shares its own budget.
const PART_FINISH_LIMIT: Duration = Duration::from_secs(5);

const FRAME_INTERVAL_NS: u64 = 1_000_000_000 / 15;
/// No new frame for three intervals starts repeats. Two would repeat on normal jitter. A window
/// whose picture does not change gets no new frame at all, so repeats are routine, not a gap.
const REPEAT_AFTER_NS: u64 = FRAME_INTERVAL_NS * 3;
/// A frame further than this past the last one means the process stood still. One sample would
/// then last the whole span, and a fragment over the playlist's 4 s target breaks players, so the
/// part ends at the last frame instead.
const TIME_JUMP_LIMIT_NS: u64 = 1_000_000_000;
/// The stream may deliver up to 16 fps (see `mac::stream`); a frame this soon after the last
/// waits for the next slot, so the output stays at 15 fps.
const MIN_FRAME_SPACING_NS: u64 = FRAME_INTERVAL_NS - 10_000_000;
/// Fragments cut at the first keyframe past 2 s, so a keyframe is forced on that grid.
const KEYFRAME_INTERVAL_NS: u64 = 2_000_000_000;

const MAX_WIDTH_PX: f64 = 1280.0;
const MAX_HEIGHT_PX: f64 = 720.0;
/// The hardware encoders' widest frame.
const MAX_SIDE_PX: f64 = 4096.0;

/// What reaches the serving loop from stdin, with when it arrived: a stop's close deadline runs
/// from then, however long the loop took to read it.
enum HelperInput {
    Command {
        command: VideoCommand,
        received: Instant,
        /// `StopsRead` before this line.
        stops_before: u64,
    },
    StdinClosed {
        received: Instant,
    },
}

/// How many stops the stdin thread has read, stdin's close included. A segment compares it with
/// the count at its start, so a stop still queued behind a slow call skips the next call.
type StopsRead = Arc<AtomicU64>;

/// The stops read so far, against the count when the segment's `start` was read.
#[derive(Clone, Copy)]
struct StopCount<'a> {
    stops: &'a StopsRead,
    stops_at_start: u64,
}

impl StopCount<'_> {
    /// A stop or stdin's close waits in the queue for the loop.
    fn is_stop_read(self) -> bool {
        self.stops.load(Ordering::Acquire) > self.stops_at_start
    }
}

/// Numbers and writes report lines. The stdin thread and the recording loop share it.
#[derive(Clone)]
struct ReportWriter {
    next_seq: Arc<Mutex<u64>>,
}

impl ReportWriter {
    fn new() -> Self {
        Self {
            next_seq: Arc::new(Mutex::new(1)),
        }
    }

    fn send(&self, report: &VideoReport) -> anyhow::Result<()> {
        // The lock spans the write, so lines leave in seq order.
        let mut next_seq = self.next_seq.lock().unwrap_or_else(PoisonError::into_inner);
        let line = capture_protocol::encode(*next_seq, report)?;
        *next_seq += 1;
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{line}").context("write a report to stdout")?;
        stdout.flush().context("flush stdout")?;
        Ok(())
    }

    fn error(&self, kind: VideoErrorKind, message: String) -> anyhow::Result<()> {
        tracing::error!(error.type = %kind, exception.message = message, "video.error");
        self.send(&VideoReport::Error { kind, message })
    }
}

/// Serves commands until stdin closes. Returns an error, and so exits non-zero, when a segment
/// fails: the app then restarts the process with the call's next segment number.
pub(crate) fn serve() -> anyhow::Result<()> {
    video_capture::mac::window::connect_window_server();
    let writer = ReportWriter::new();
    let (sender, inputs) = mpsc::channel();
    let reader_writer = writer.clone();
    let stops: StopsRead = Arc::new(AtomicU64::new(0));
    let reader_stops = Arc::clone(&stops);
    std::thread::Builder::new()
        .name("stdin".to_owned())
        .spawn(move || read_commands(&sender, &reader_writer, &reader_stops))
        .context("start the stdin thread")?;
    #[cfg(debug_assertions)]
    crate::inject::start(|_| {});
    tracing::info!(process.pid = std::process::id(), "video.ready");
    loop {
        let Ok(input) = inputs.recv() else {
            return Ok(());
        };
        let (command, stops_before) = match input {
            HelperInput::Command {
                command,
                stops_before,
                ..
            } => (command, stops_before),
            HelperInput::StdinClosed { .. } => return Ok(()),
        };
        match command {
            VideoCommand::Stop => writer.error(
                VideoErrorKind::BadCommand,
                "stop arrived with no segment open".to_owned(),
            )?,
            // The app keeps the latest hint and sends it with the next start.
            VideoCommand::Window { window_hint } => tracing::info!(
                video.window_id = window_hint.window_id,
                "video.window_hint_between_segments"
            ),
            VideoCommand::Start {
                call_id,
                folder,
                segment,
                audio_segment,
                platform,
                window_hint,
            } => {
                let state = capture::permission_state(PermissionService::ScreenRecording);
                writer.send(&VideoReport::Permission {
                    service: PermissionService::ScreenRecording,
                    state,
                })?;
                if state != PermissionState::Granted {
                    writer.error(
                        VideoErrorKind::PermissionDenied,
                        format!(
                            "screen recording is {state}: video stays off for segment {segment}"
                        ),
                    )?;
                    continue;
                }
                let request = SegmentRequest {
                    folder,
                    number: segment,
                    audio_segment,
                    call_id,
                    platform,
                    hint: window_hint,
                };
                let stop_count = StopCount {
                    stops: &stops,
                    stops_at_start: stops_before,
                };
                match record_segment(&request, &writer, &inputs, stop_count) {
                    Ok(SegmentEnd::Stopped) => {}
                    Ok(SegmentEnd::StdinClosed) => return Ok(()),
                    Err(error) => {
                        let kind = match &error {
                            SegmentError::Start(_) => VideoErrorKind::SegmentStartFailed,
                            SegmentError::Encoder(_) => VideoErrorKind::EncoderFailed,
                            SegmentError::Write(_) => VideoErrorKind::WriteFailed,
                            SegmentError::Close(_) => VideoErrorKind::SegmentCloseFailed,
                            SegmentError::RebuildLimit { .. } => VideoErrorKind::StreamRebuildLimit,
                            SegmentError::StreamStopTimedOut(_) => {
                                VideoErrorKind::StreamStopTimedOut
                            }
                            SegmentError::Report(_) => return Err(error.into()),
                        };
                        // The exit code tells the app too, so a failed report adds nothing.
                        let _unheard = writer.error(kind, error.to_string());
                        return Err(error.into());
                    }
                }
            }
        }
    }
}

/// Parses each stdin line once. A bad line is reported and skipped.
fn read_commands(sender: &Sender<HelperInput>, writer: &ReportWriter, stops: &StopsRead) {
    for line in std::io::stdin().lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(exception.message = %error, "stdin.read_failed");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match capture_protocol::decode::<VideoCommand>(&line) {
            Ok(command) => {
                tracing::info!(message.id = command.seq, "video.command");
                let stops_before = match command.message {
                    // Counted before the send, so the loop sees the count no later than the line.
                    VideoCommand::Stop => stops.fetch_add(1, Ordering::AcqRel),
                    VideoCommand::Start { .. } | VideoCommand::Window { .. } => {
                        stops.load(Ordering::Acquire)
                    }
                };
                let input = HelperInput::Command {
                    command: command.message,
                    received: Instant::now(),
                    stops_before,
                };
                if sender.send(input).is_err() {
                    return;
                }
            }
            Err(error) => {
                let _unheard = writer.error(
                    VideoErrorKind::BadCommand,
                    format!("not a valid command: {error}"),
                );
            }
        }
    }
    tracing::info!("stdin.closed");
    stops.fetch_add(1, Ordering::AcqRel);
    let _unheard = sender.send(HelperInput::StdinClosed {
        received: Instant::now(),
    });
}

struct SegmentRequest {
    folder: std::path::PathBuf,
    number: u32,
    audio_segment: u32,
    call_id: String,
    platform: Platform,
    hint: WindowHint,
}

enum SegmentEnd {
    Stopped,
    StdinClosed,
}

#[derive(Debug, thiserror::Error)]
enum SegmentError {
    #[error("the segment could not start: {0}")]
    Start(#[source] VideoWriterError),
    #[error("{0}")]
    Encoder(String),
    #[error("a write failed: {0}")]
    Write(#[source] VideoWriterError),
    #[error("the segment could not close: {0}")]
    Close(#[source] VideoWriterError),
    #[error("the stream needed rebuild {count} within one episode; the limit is {REBUILD_LIMIT}")]
    RebuildLimit { count: u32 },
    #[error("{0}; the process exits so the capture ends with it")]
    StreamStopTimedOut(String),
    #[error(transparent)]
    Report(#[from] anyhow::Error),
}

/// Why the loop ends a segment, and when the close must be done by.
#[derive(Debug, Clone, Copy)]
struct SegmentStop {
    cause: StopCause,
    close_by: Instant,
}

impl SegmentStop {
    fn new(cause: StopCause, received: Instant) -> Self {
        Self {
            cause,
            close_by: received + SEGMENT_CLOSE_LIMIT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopCause {
    Command,
    StdinClosed,
    /// The system's sharing controls ended the capture.
    User,
}

impl StopCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::Command => "stop",
            Self::StdinClosed => "stdin_closed",
            Self::User => "user",
        }
    }
}

/// What a running lookup is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LookupPurpose {
    /// No window is recorded: the segment's first, or one that closed.
    Find,
    /// The stream stopped or failed to start, so a new one needs a fresh window.
    Rebuild,
    /// The stream went quiet after a complete frame; the lookup tells a hidden window from a
    /// hung stream.
    StallCheck,
    /// The stream went quiet after a hidden status. The lookup tells a hidden window from a
    /// closed one and never rebuilds: a locked screen lists its windows on screen.
    PresenceCheck,
}

/// The status of the stream's last callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameStatus {
    /// None since the stream started.
    NotYet,
    Complete,
    /// Nothing new. A still window sends one and then nothing until its picture changes.
    Idle,
    /// Blank or suspended: the window is hidden or the screen is locked.
    Hidden,
}

struct LookupState {
    /// The id, purpose and start of the lookup in flight.
    in_flight: Option<(u64, LookupPurpose, Instant)>,
    next_id: u64,
    /// When the next lookup may start, for the purpose waiting on it.
    due: Option<(Instant, LookupPurpose)>,
    count: u64,
}

/// A span with no new frame, open until frames come back.
#[derive(Clone, Copy)]
struct OpenGap {
    start_host_time_ns: u64,
    cause: VideoGapCause,
}

/// The window the segment records.
struct RecordedWindow {
    listed: ListedWindow,
    stream: Option<WindowStream>,
    stream_generation: u64,
}

/// The encoder and muxer of the current part, and what the part needs to start.
struct Part {
    encoder: Encoder,
    muxer: Option<Muxer>,
    next_keyframe_ns: u64,
    /// A frame was lost, so the next encode is a keyframe.
    is_keyframe_wanted: bool,
    /// A frame was lost, so the frames after it reference a hole until the next keyframe.
    is_awaiting_keyframe: bool,
}

/// What a part's init segment is written with. The muxer hands the init over with its first
/// fragment, which may come after the part ended, so this waits in a queue until then.
struct PartStart {
    codec: VideoCodec,
    size: OutputSize,
    first_host_time_ns: u64,
}

/// A record for `video.json` that came before the writer, which starts at the first frame.
enum PendingNote {
    Gap(VideoGapRecord),
    WindowChange(WindowChangeRecord),
    StreamRebuild,
}

struct Channels {
    stream: Sender<(u64, StreamEvent)>,
    stream_rx: Receiver<(u64, StreamEvent)>,
    encoded: Sender<EncoderOutput>,
    encoded_rx: Receiver<EncoderOutput>,
    muxed: Sender<MuxerOutput>,
    muxed_rx: Receiver<MuxerOutput>,
    lookups: Sender<(u64, WindowLookup)>,
    lookups_rx: Receiver<(u64, WindowLookup)>,
}

impl Channels {
    fn new() -> Self {
        let (stream, stream_rx) = mpsc::channel();
        let (encoded, encoded_rx) = mpsc::channel();
        let (muxed, muxed_rx) = mpsc::channel();
        let (lookups, lookups_rx) = mpsc::channel();
        Self {
            stream,
            stream_rx,
            encoded,
            encoded_rx,
            muxed,
            muxed_rx,
            lookups,
            lookups_rx,
        }
    }
}

struct Segment<'a> {
    request: &'a SegmentRequest,
    /// Skips a new stream call once a stop is read, so a loop pass that chains calls still
    /// leaves the close its budget.
    stop_count: StopCount<'a>,
    /// The request's hint, or the latest the app sent since.
    hint: WindowHint,
    reports: &'a ReportWriter,
    channels: Channels,
    started: Instant,
    writer: Option<VideoSegmentWriter>,
    pending_notes: Vec<PendingNote>,
    window: Option<RecordedWindow>,
    /// The last window recorded, kept while none is, to tell a return from a replacement.
    last_window_id: Option<u32>,
    part: Option<Part>,
    parts_awaiting_init: VecDeque<PartStart>,
    last_frame: Option<CapturedFrame>,
    last_encoded_ns: Option<u64>,
    /// Host time of the last new frame from the stream, repeats not counted.
    last_captured_ns: Option<u64>,
    last_status: FrameStatus,
    counts: FrameCounts,
    fragments_written: u64,
    is_recording_reported: bool,
    gap: Option<OpenGap>,
    last_callback: Instant,
    rebuilds_in_episode: u32,
    last_rebuild: Option<Instant>,
    lookup: LookupState,
    /// A size the window's frames ask for, and since when.
    pending_size: Option<(OutputSize, Instant)>,
    next_heartbeat: Duration,
    /// Tags each stream's callbacks, so a stopped stream's late frames are ignored.
    next_stream_generation: u64,
}

fn record_segment(
    request: &SegmentRequest,
    reports: &ReportWriter,
    inputs: &Receiver<HelperInput>,
    stop_count: StopCount<'_>,
) -> Result<SegmentEnd, SegmentError> {
    let now = Instant::now();
    let mut segment = Segment {
        request,
        stop_count,
        hint: request.hint.clone(),
        reports,
        channels: Channels::new(),
        started: now,
        writer: None,
        pending_notes: Vec::new(),
        window: None,
        last_window_id: None,
        part: None,
        parts_awaiting_init: VecDeque::new(),
        last_frame: None,
        last_encoded_ns: None,
        last_captured_ns: None,
        last_status: FrameStatus::NotYet,
        counts: FrameCounts::default(),
        fragments_written: 0,
        is_recording_reported: false,
        gap: None,
        last_callback: now,
        rebuilds_in_episode: 0,
        last_rebuild: None,
        lookup: LookupState {
            in_flight: None,
            next_id: 1,
            due: Some((now, LookupPurpose::Find)),
            count: 0,
        },
        pending_size: None,
        next_heartbeat: HEARTBEAT_INTERVAL,
        next_stream_generation: 0,
    };
    tracing::info!(
        segment.number = request.number,
        call.id = request.call_id,
        video.window_hint.bundle_id = request.hint.bundle_id,
        video.window_hint.window_id = request.hint.window_id,
        "video.segment_requested"
    );
    let outcome = segment.run(inputs);
    let stop = outcome.as_ref().ok().copied();
    let close_by = stop.map_or_else(
        || Instant::now() + SEGMENT_CLOSE_LIMIT,
        |stop| stop.close_by,
    );
    let closed = segment.close(stop.map(|stop| stop.cause), close_by);
    match (outcome.map(|stop| stop.cause), closed) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(StopCause::StdinClosed), Ok(())) => Ok(SegmentEnd::StdinClosed),
        (Ok(StopCause::Command), Ok(())) => Ok(SegmentEnd::Stopped),
        (Ok(StopCause::User), Ok(())) => {
            reports.error(
                VideoErrorKind::StoppedByUser,
                "the capture was stopped through the system's sharing controls".to_owned(),
            )?;
            Ok(SegmentEnd::Stopped)
        }
    }
}

impl Segment<'_> {
    fn run(&mut self, inputs: &Receiver<HelperInput>) -> Result<SegmentStop, SegmentError> {
        loop {
            match self.channels.stream_rx.recv_timeout(TICK) {
                Ok((generation, event)) => {
                    if let Some(stop) = self.stream_event(generation, event)? {
                        return Ok(stop);
                    }
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
            }
            while let Ok((generation, event)) = self.channels.stream_rx.try_recv() {
                if let Some(stop) = self.stream_event(generation, event)? {
                    return Ok(stop);
                }
            }
            self.drain_encoded()?;
            self.drain_muxed()?;
            while let Ok((id, lookup)) = self.channels.lookups_rx.try_recv() {
                if let Some(stop) = self.lookup_done(id, lookup)? {
                    return Ok(stop);
                }
            }
            self.tick()?;
            #[cfg(debug_assertions)]
            Self::apply_injected_events();
            match inputs.try_recv() {
                Ok(HelperInput::Command {
                    command: VideoCommand::Stop,
                    received,
                    ..
                }) => return Ok(SegmentStop::new(StopCause::Command, received)),
                Ok(HelperInput::Command {
                    command: VideoCommand::Window { window_hint },
                    ..
                }) => self.follow_hint(window_hint),
                Ok(HelperInput::Command {
                    command: VideoCommand::Start { segment, .. },
                    ..
                }) => {
                    self.reports.error(
                        VideoErrorKind::BadCommand,
                        format!(
                            "start of segment {segment} arrived while segment {} is open",
                            self.request.number
                        ),
                    )?;
                }
                Err(TryRecvError::Empty) => {}
                Ok(HelperInput::StdinClosed { received }) => {
                    return Ok(SegmentStop::new(StopCause::StdinClosed, received));
                }
                Err(TryRecvError::Disconnected) => {
                    return Ok(SegmentStop::new(StopCause::StdinClosed, Instant::now()));
                }
            }
        }
    }

    /// Handles one stream callback. `Some` ends the segment.
    fn stream_event(
        &mut self,
        generation: u64,
        event: StreamEvent,
    ) -> Result<Option<SegmentStop>, SegmentError> {
        let Some(window) = &self.window else {
            return Ok(None);
        };
        if window.stream.is_none() || window.stream_generation != generation {
            return Ok(None);
        }
        match event {
            StreamEvent::Frame(frame) => {
                self.last_callback = Instant::now();
                self.last_status = FrameStatus::Complete;
                self.last_captured_ns = Some(frame.host_time_ns);
                self.counts.captured += 1;
                self.end_gap()?;
                self.follow_size(&frame);
                self.encode_new(frame)?;
            }
            StreamEvent::Idle => {
                self.last_callback = Instant::now();
                self.last_status = FrameStatus::Idle;
                self.counts.idle += 1;
                self.end_gap()?;
            }
            StreamEvent::Hidden => {
                self.last_callback = Instant::now();
                self.last_status = FrameStatus::Hidden;
                self.open_gap(VideoGapCause::WindowHidden);
            }
            StreamEvent::Stopped(stop) => {
                tracing::warn!(video.stream_stop = %stop, "video.stream_stopped");
                match stop {
                    StreamStop::ByUser => {
                        return Ok(Some(SegmentStop::new(StopCause::User, Instant::now())));
                    }
                    StreamStop::NoSource => self.window_closed()?,
                    StreamStop::BySystem | StreamStop::Other { .. } => {
                        // The stream is already stopped, so dropping it is enough.
                        if let Some(window) = &mut self.window {
                            window.stream = None;
                        }
                        self.rebuild(None)?;
                    }
                }
            }
        }
        Ok(None)
    }

    /// The app saw the huddle window reopen. A lookup in flight asks for the old id, so its
    /// answer is dropped and a new one starts.
    fn follow_hint(&mut self, hint: WindowHint) {
        let is_recorded = self
            .window
            .as_ref()
            .is_some_and(|window| window.listed.window_id == hint.window_id);
        tracing::info!(
            video.window_id = hint.window_id,
            video.is_recorded = is_recorded,
            "video.window_hint_changed"
        );
        self.hint = hint;
        if is_recorded {
            return;
        }
        self.lookup.in_flight = None;
        self.lookup.due = Some((Instant::now(), LookupPurpose::Find));
    }

    /// A new frame from the stream, encoded at its own host time.
    fn encode_new(&mut self, frame: CapturedFrame) -> Result<(), SegmentError> {
        let host_time_ns = frame.host_time_ns;
        // A repeat already took this spot on the grid, or the frame came too soon after the
        // last. Its picture still goes out with the next repeat.
        if self
            .last_encoded_ns
            .is_some_and(|last| host_time_ns < last + MIN_FRAME_SPACING_NS)
        {
            self.last_frame = Some(frame);
            return Ok(());
        }
        self.encode(&frame, host_time_ns)?;
        self.last_frame = Some(frame);
        Ok(())
    }

    fn encode(&mut self, frame: &CapturedFrame, host_time_ns: u64) -> Result<(), SegmentError> {
        if let Some(last_ns) = self.last_encoded_ns
            && host_time_ns.saturating_sub(last_ns) > FRAME_INTERVAL_NS + TIME_JUMP_LIMIT_NS
        {
            self.end_part_at_jump(last_ns, host_time_ns)?;
        }
        let size = OutputSize {
            width_px: frame.width_px,
            height_px: frame.height_px,
        };
        if self
            .part
            .as_ref()
            .is_some_and(|part| part.encoder.size != size)
        {
            self.finish_part(PART_FINISH_LIMIT)?;
        }
        if self.part.is_none() {
            let sink_channel = self.channels.encoded.clone();
            let sink: EncoderSink = Arc::new(move |output| {
                let _unheard = sink_channel.send(output);
            });
            let encoder = Encoder::new(size, sink)
                .map_err(|error| SegmentError::Encoder(error.to_string()))?;
            self.ensure_writer(encoder.readback.codec)?;
            self.part = Some(Part {
                encoder,
                muxer: None,
                next_keyframe_ns: host_time_ns,
                is_keyframe_wanted: false,
                is_awaiting_keyframe: false,
            });
        }
        let Some(part) = &mut self.part else {
            return Ok(());
        };
        let is_keyframe_forced = part.is_keyframe_wanted || host_time_ns >= part.next_keyframe_ns;
        while part.next_keyframe_ns <= host_time_ns {
            part.next_keyframe_ns += KEYFRAME_INTERVAL_NS;
        }
        if let Err(error) = part.encoder.encode(frame, host_time_ns, is_keyframe_forced) {
            tracing::warn!(exception.message = %error, "video.encode_failed");
            // The frame never reached the encoder, so nothing references it; a keyframe it
            // was to be still has to come.
            part.is_keyframe_wanted = is_keyframe_forced;
            self.counts.dropped += 1;
            return Ok(());
        }
        part.is_keyframe_wanted = false;
        self.counts.encoded += 1;
        self.last_encoded_ns = Some(host_time_ns);
        if !self.is_recording_reported {
            self.is_recording_reported = true;
            let codec = part.encoder.readback.codec;
            let window = self.window_record();
            self.reports.send(&VideoReport::Recording {
                segment: self.request.number,
                window,
                width_px: size.width_px,
                height_px: size.height_px,
                codec,
                first_host_time_ns: host_time_ns,
            })?;
        }
        Ok(())
    }

    /// The loop stood still: the part ends at the last frame and the next frame starts a new
    /// one, so no sample lasts the whole span. An open gap already covers the span.
    fn end_part_at_jump(&mut self, last_ns: u64, host_time_ns: u64) -> Result<(), SegmentError> {
        let start_host_time_ns = last_ns + FRAME_INTERVAL_NS;
        #[expect(
            clippy::cast_precision_loss,
            reason = "a stall is far shorter than the 2^52 ns where f64 loses whole nanoseconds"
        )]
        let length_ms = host_time_ns.saturating_sub(start_host_time_ns) as f64 / 1e6;
        tracing::warn!(video.gap_ms = length_ms, "video.process_stalled");
        self.finish_part(PART_FINISH_LIMIT)?;
        if self.gap.is_none() {
            self.record_gap(VideoGapRecord {
                start_host_time_ns,
                length_ms,
                cause: VideoGapCause::ProcessStalled,
            })?;
        }
        Ok(())
    }

    /// Starts the segment's writer at its first frame, so a window that never shows leaves no
    /// segment. What came before it goes in now.
    fn ensure_writer(&mut self, codec: VideoCodec) -> Result<(), SegmentError> {
        if self.writer.is_some() {
            return Ok(());
        }
        let writer = VideoSegmentWriter::start(SegmentStart {
            folder: &self.request.folder,
            number: self.request.number,
            audio_segment: self.request.audio_segment,
            call_id: &self.request.call_id,
            platform: self.request.platform,
            codec,
            window: self.window_record(),
            clock: capture::utc_anchor(),
        })
        .map_err(SegmentError::Start)?;
        self.writer = Some(writer);
        for note in std::mem::take(&mut self.pending_notes) {
            self.note(note)?;
        }
        Ok(())
    }

    /// Lists a record in `video.json`, or keeps it until the writer starts.
    fn note(&mut self, note: PendingNote) -> Result<(), SegmentError> {
        let Some(writer) = &mut self.writer else {
            self.pending_notes.push(note);
            return Ok(());
        };
        let stored = match note {
            PendingNote::Gap(gap) => writer.note_gap(gap),
            PendingNote::WindowChange(change) => writer.note_window_change(change),
            PendingNote::StreamRebuild => writer.note_stream_rebuild(),
        };
        if let Err(error) = stored {
            self.reports
                .error(VideoErrorKind::CheckpointFailed, error.to_string())?;
        }
        Ok(())
    }

    fn window_record(&self) -> WindowRecord {
        self.window.as_ref().map_or_else(
            || WindowRecord {
                window_id: self.last_window_id.unwrap_or(self.hint.window_id),
                bundle_id: Some(self.hint.bundle_id.clone()),
                title: self.hint.title.clone(),
                pid: self.hint.pid.unwrap_or(0),
            },
            |window| record_of(&window.listed),
        )
    }

    fn drain_encoded(&mut self) -> Result<(), SegmentError> {
        while let Ok(output) = self.channels.encoded_rx.try_recv() {
            let Some(part) = &mut self.part else {
                continue;
            };
            let frame = match output {
                EncoderOutput::Frame(frame) => frame,
                EncoderOutput::Dropped { status } => {
                    tracing::warn!(video.status = status, "video.frame_dropped_by_encoder");
                    self.counts.dropped += 1;
                    part.is_keyframe_wanted = true;
                    part.is_awaiting_keyframe = true;
                    continue;
                }
            };
            self.counts.output += 1;
            if part.is_awaiting_keyframe {
                if !frame.is_keyframe {
                    self.counts.dropped += 1;
                    continue;
                }
                part.is_awaiting_keyframe = false;
            }
            if part.muxer.is_none() {
                let sink_channel = self.channels.muxed.clone();
                let sink: MuxerSink = Arc::new(move |output| {
                    let _unheard = sink_channel.send(output);
                });
                let muxer = Muxer::start(&frame, sink)
                    .map_err(|error| SegmentError::Encoder(error.to_string()))?;
                self.parts_awaiting_init.push_back(PartStart {
                    codec: part.encoder.readback.codec,
                    size: part.encoder.size,
                    first_host_time_ns: frame.host_time_ns,
                });
                part.muxer = Some(muxer);
            }
            let Some(muxer) = &mut part.muxer else {
                continue;
            };
            let appended = muxer
                .append(&frame)
                .map_err(|error| SegmentError::Encoder(error.to_string()))?;
            match appended {
                MuxerAppend::Appended => {}
                MuxerAppend::Refused => {
                    tracing::warn!("video.frame_refused_by_muxer");
                    self.counts.dropped += 1;
                    part.is_keyframe_wanted = true;
                    part.is_awaiting_keyframe = true;
                }
            }
        }
        Ok(())
    }

    fn drain_muxed(&mut self) -> Result<(), SegmentError> {
        while let Ok(output) = self.channels.muxed_rx.try_recv() {
            let Some(writer) = &mut self.writer else {
                continue;
            };
            match output {
                MuxerOutput::Init(bytes) => {
                    let Some(start) = self.parts_awaiting_init.pop_front() else {
                        tracing::warn!("video.init_without_part");
                        continue;
                    };
                    writer
                        .start_part(
                            &bytes,
                            start.codec,
                            start.size.width_px,
                            start.size.height_px,
                            start.first_host_time_ns,
                        )
                        .map_err(SegmentError::Write)?;
                }
                MuxerOutput::Fragment {
                    bytes,
                    first_host_time_ns,
                    duration_ms,
                } => {
                    writer.set_frame_counts(self.counts);
                    let written = writer
                        .write_fragment(&bytes, first_host_time_ns, duration_ms)
                        .map_err(SegmentError::Write)?;
                    self.fragments_written += 1;
                    tracing::debug!(
                        file.name = written.file,
                        file.size = written.bytes,
                        video.duration_ms = written.duration_ms,
                        "video.fragment_written"
                    );
                }
            }
        }
        Ok(())
    }

    /// Ends the current part: every frame through the encoder and the muxer, and its last
    /// fragment on disk. The encoder and the muxer share at most `limit` for it.
    fn finish_part(&mut self, limit: Duration) -> Result<(), SegmentError> {
        let Some(part) = &self.part else {
            return Ok(());
        };
        let finish_by = Instant::now() + limit;
        // A hung encoder loses its last frames; the muxer still ends the part on what it has.
        if let Err(error) = part.encoder.flush(limit) {
            tracing::warn!(exception.message = %error, "video.encoder_flush_failed");
        }
        self.drain_encoded()?;
        if let Some(part) = self.part.take()
            && let Some(muxer) = part.muxer
        {
            muxer
                .finish(finish_by.saturating_duration_since(Instant::now()))
                .map_err(|error| SegmentError::Encoder(error.to_string()))?;
        }
        self.drain_muxed()?;
        // A muxer that wrote no init leaves its part's start queued; the next part's init must
        // not take it.
        self.parts_awaiting_init.clear();
        Ok(())
    }

    /// Repeats, the stall watch, lookups, size changes and heartbeats.
    fn tick(&mut self) -> Result<(), SegmentError> {
        self.repeat_last_frame()?;
        let now = Instant::now();
        let has_stream = self
            .window
            .as_ref()
            .is_some_and(|window| window.stream.is_some());
        if has_stream
            && now.duration_since(self.last_callback) >= STALL_LIMIT
            && self.lookup.in_flight.is_none()
            && self.lookup.due.is_none()
        {
            let purpose = match self.last_status {
                // ScreenCaptureKit answers a start at once with a frame or a hidden status, so
                // silence from the start is a hang too.
                FrameStatus::NotYet | FrameStatus::Complete => Some(LookupPurpose::StallCheck),
                FrameStatus::Hidden => Some(LookupPurpose::PresenceCheck),
                // A still window, or a huddle with every camera off, says idle once. A stream
                // that hangs after its idle status looks the same, so it goes unnoticed until
                // the window changes: rebuilding every still window would cut the recording.
                FrameStatus::Idle => None,
            };
            if let Some(purpose) = purpose {
                tracing::info!(
                    video.quiet_ms = now.duration_since(self.last_callback).as_millis(),
                    video.lookup_purpose = ?purpose,
                    "video.stream_quiet"
                );
                self.lookup.due = Some((now, purpose));
            }
        }
        if self.rebuilds_in_episode > 0
            && self
                .last_rebuild
                .is_some_and(|at| now.duration_since(at) >= REBUILD_EPISODE_RESET)
            && self.is_stream_healthy(now)
        {
            tracing::info!(
                video.rebuilds = self.rebuilds_in_episode,
                "video.rebuild_episode_ended"
            );
            self.rebuilds_in_episode = 0;
        }
        self.run_due_lookup(now);
        if let Some((size, since)) = self.pending_size
            && now.duration_since(since) >= RESIZE_SETTLE
        {
            self.pending_size = None;
            self.resize_stream(size)?;
        }
        if self.started.elapsed() >= self.next_heartbeat {
            self.next_heartbeat += HEARTBEAT_INTERVAL;
            self.reports.send(&VideoReport::Heartbeat {
                segment: self.request.number,
                frames_captured: self.counts.captured,
                frames_idle: self.counts.idle,
                frames_encoded: self.counts.encoded,
                frames_output: self.counts.output,
                fragments_written: self.fragments_written,
                window_lookups: self.lookup.count,
            })?;
        }
        Ok(())
    }

    /// Whether the stream works since its last rebuild: complete frames lately, or an idle
    /// status, which a still window sends once and then nothing.
    fn is_stream_healthy(&self, now: Instant) -> bool {
        match self.last_status {
            FrameStatus::Complete => now.duration_since(self.last_callback) < STALL_LIMIT,
            FrameStatus::Idle => self
                .last_rebuild
                .is_some_and(|rebuilt| self.last_callback >= rebuilt),
            FrameStatus::NotYet | FrameStatus::Hidden => false,
        }
    }

    /// Fills the 15 fps grid with the last frame while no new one comes.
    fn repeat_last_frame(&mut self) -> Result<(), SegmentError> {
        let Some(last_ns) = self.last_encoded_ns else {
            return Ok(());
        };
        let now_ns = capture::host_now_ns();
        if now_ns < last_ns + REPEAT_AFTER_NS {
            return Ok(());
        }
        let Some(frame) = self.last_frame.take() else {
            return Ok(());
        };
        let mut next_ns = last_ns + FRAME_INTERVAL_NS;
        // The loop stood still, in a slow framework call on this thread or a sleep the app did
        // not stop video for. One repeat just before now goes out, and `encode` ends the part at
        // the jump; filling the span would encode it all at once.
        if now_ns.saturating_sub(next_ns) > TIME_JUMP_LIMIT_NS {
            next_ns = now_ns - FRAME_INTERVAL_NS;
        }
        let mut outcome = Ok(());
        // Up to the frame before now, so a real frame arriving now still fits after them.
        while next_ns + FRAME_INTERVAL_NS <= now_ns {
            outcome = self.encode(&frame, next_ns);
            if outcome.is_err() {
                break;
            }
            self.counts.repeated += 1;
            next_ns += FRAME_INTERVAL_NS;
        }
        self.last_frame = Some(frame);
        outcome
    }

    fn open_gap(&mut self, cause: VideoGapCause) {
        let start_host_time_ns = self
            .last_encoded_ns
            .map_or_else(capture::host_now_ns, |last| last + FRAME_INTERVAL_NS);
        self.open_gap_at(cause, start_host_time_ns);
    }

    fn open_gap_at(&mut self, cause: VideoGapCause, start_host_time_ns: u64) {
        if self.gap.is_none() {
            tracing::debug!(video.gap_cause = %cause, "video.gap_started");
            self.gap = Some(OpenGap {
                start_host_time_ns,
                cause,
            });
        }
    }

    fn end_gap(&mut self) -> Result<(), SegmentError> {
        let Some(gap) = self.gap.take() else {
            return Ok(());
        };
        let end_ns = capture::host_now_ns();
        #[expect(
            clippy::cast_precision_loss,
            reason = "a gap is far shorter than the 2^52 ns where f64 loses whole nanoseconds"
        )]
        let length_ms = end_ns.saturating_sub(gap.start_host_time_ns) as f64 / 1e6;
        self.record_gap(VideoGapRecord {
            start_host_time_ns: gap.start_host_time_ns,
            length_ms,
            cause: gap.cause,
        })
    }

    fn record_gap(&mut self, record: VideoGapRecord) -> Result<(), SegmentError> {
        tracing::info!(
            video.gap_cause = %record.cause,
            video.gap_ms = record.length_ms,
            "video.gap_ended"
        );
        self.note(PendingNote::Gap(record))?;
        self.reports.send(&VideoReport::Gap {
            segment: self.request.number,
            start_host_time_ns: record.start_host_time_ns,
            length_ms: record.length_ms,
            cause: record.cause,
        })?;
        Ok(())
    }

    /// Asks for the stream size the window's frames call for, once it holds for 1 s.
    fn follow_size(&mut self, frame: &CapturedFrame) {
        let Some((width_pt, height_pt)) = frame.content_size_pt else {
            return;
        };
        let Some(current) = self
            .window
            .as_ref()
            .and_then(|window| window.stream.as_ref())
            .map(|stream| stream.size)
        else {
            return;
        };
        let wanted = output_size(width_pt, height_pt);
        if wanted == current {
            self.pending_size = None;
            return;
        }
        match self.pending_size {
            Some((size, _)) if size == wanted => {}
            Some(_) | None => self.pending_size = Some((wanted, Instant::now())),
        }
    }

    fn resize_stream(&mut self, size: OutputSize) -> Result<(), SegmentError> {
        let Some(window) = &mut self.window else {
            return Ok(());
        };
        let Some(stream) = &mut window.stream else {
            return Ok(());
        };
        tracing::info!(
            video.width_px = size.width_px,
            video.height_px = size.height_px,
            "video.stream_resizing"
        );
        if let Err(error) = stream.resize(size) {
            tracing::warn!(exception.message = %error, "video.resize_failed");
            return self.rebuild(None);
        }
        let record = record_of(&window.listed);
        self.note_window_change(WindowChangeCause::Resized, Some(record), Some(size))
    }

    fn note_window_change(
        &mut self,
        cause: WindowChangeCause,
        window: Option<WindowRecord>,
        size: Option<OutputSize>,
    ) -> Result<(), SegmentError> {
        tracing::info!(video.window_change = %cause, "video.window_changed");
        self.note(PendingNote::WindowChange(WindowChangeRecord {
            host_time_ns: capture::host_now_ns(),
            cause,
            window: window.clone(),
            content_width_pt: None,
            content_height_pt: None,
        }))?;
        self.reports.send(&VideoReport::WindowChanged {
            segment: self.request.number,
            cause,
            window,
            width_px: size.map(|size| size.width_px),
            height_px: size.map(|size| size.height_px),
        })?;
        Ok(())
    }

    fn window_closed(&mut self) -> Result<(), SegmentError> {
        if let Some(window) = self.window.take() {
            self.last_window_id = Some(window.listed.window_id);
            stop_stream(window.stream, STREAM_CALL_LIMIT)?;
        }
        self.open_gap(VideoGapCause::WindowMissing);
        self.lookup.due = Some((Instant::now(), LookupPurpose::Find));
        self.note_window_change(WindowChangeCause::Closed, None, None)
    }

    /// Counts a rebuild and asks for a fresh window to build the stream on. A stall's gap
    /// starts at `gap_start_ns`, its last new frame; any other at the next grid slot.
    fn rebuild(&mut self, gap_start_ns: Option<u64>) -> Result<(), SegmentError> {
        self.rebuilds_in_episode += 1;
        self.last_rebuild = Some(Instant::now());
        tracing::warn!(
            video.rebuilds = self.rebuilds_in_episode,
            "video.stream_rebuild"
        );
        if self.rebuilds_in_episode > REBUILD_LIMIT {
            return Err(SegmentError::RebuildLimit {
                count: self.rebuilds_in_episode,
            });
        }
        // The close stops the stream within its own budget.
        if !self.stop_count.is_stop_read()
            && let Some(window) = &mut self.window
        {
            stop_stream(window.stream.take(), STREAM_CALL_LIMIT)?;
        }
        // A stall check in flight would answer for the stream just dropped, and count a second
        // rebuild for the same fault.
        self.lookup.in_flight = None;
        self.note(PendingNote::StreamRebuild)?;
        match gap_start_ns {
            Some(start_ns) => self.open_gap_at(VideoGapCause::StreamRestart, start_ns),
            None => self.open_gap(VideoGapCause::StreamRestart),
        }
        // Spread the rebuilds, so a stream that fails at once does not use all four in a second.
        self.lookup.due = Some((
            Instant::now() + LOOKUP_INTERVAL * (self.rebuilds_in_episode - 1),
            LookupPurpose::Rebuild,
        ));
        Ok(())
    }

    fn run_due_lookup(&mut self, now: Instant) {
        if let Some((id, purpose, since)) = self.lookup.in_flight {
            if now.duration_since(since) < LOOKUP_LIMIT {
                return;
            }
            tracing::warn!(video.lookup_id = id, "video.window_lookup_timed_out");
            self.lookup.in_flight = None;
            self.lookup.due = Some((now, purpose));
        }
        let Some((at, purpose)) = self.lookup.due else {
            return;
        };
        if now < at {
            return;
        }
        self.lookup.due = None;
        let id = self.lookup.next_id;
        self.lookup.next_id += 1;
        self.lookup.count += 1;
        self.lookup.in_flight = Some((id, purpose, now));
        let channel = self.channels.lookups.clone();
        look_up_window(self.hint.clone(), move |lookup| {
            let _unheard = channel.send((id, lookup));
        });
    }

    fn lookup_done(
        &mut self,
        id: u64,
        lookup: WindowLookup,
    ) -> Result<Option<SegmentStop>, SegmentError> {
        let Some((in_flight, purpose, _)) = self.lookup.in_flight else {
            return Ok(None);
        };
        if in_flight != id {
            return Ok(None);
        }
        self.lookup.in_flight = None;
        match (purpose, lookup) {
            (_, WindowLookup::Failed { message }) => {
                tracing::warn!(exception.message = message, "video.window_lookup_failed");
                self.lookup.due = Some((Instant::now() + LOOKUP_INTERVAL, purpose));
            }
            (LookupPurpose::Find, WindowLookup::NotFound) => {
                self.lookup.due = Some((Instant::now() + LOOKUP_INTERVAL, LookupPurpose::Find));
            }
            (
                LookupPurpose::Rebuild | LookupPurpose::StallCheck | LookupPurpose::PresenceCheck,
                WindowLookup::NotFound,
            ) => {
                self.window_closed()?;
            }
            (LookupPurpose::StallCheck, WindowLookup::Found { listed, .. })
                if !listed.is_on_screen =>
            {
                // A minimised window or one on another Space sends nothing; that is no hang.
                self.open_gap(VideoGapCause::WindowHidden);
                self.last_callback = Instant::now();
            }
            (LookupPurpose::StallCheck, WindowLookup::Found { .. }) => {
                let gap_start_ns = self.last_captured_ns.map(|last| last + FRAME_INTERVAL_NS);
                self.rebuild(gap_start_ns)?;
            }
            // Still there; the next check comes after another quiet spell.
            (LookupPurpose::PresenceCheck, WindowLookup::Found { .. }) => {
                self.last_callback = Instant::now();
            }
            (
                LookupPurpose::Find | LookupPurpose::Rebuild,
                WindowLookup::Found { window, listed },
            ) => {
                return self.start_stream(&window, listed);
            }
        }
        Ok(None)
    }

    fn start_stream(
        &mut self,
        window: &SharedWindow,
        listed: ListedWindow,
    ) -> Result<Option<SegmentStop>, SegmentError> {
        // A start now would only be stopped again by the close, and spend its budget.
        if self.stop_count.is_stop_read() {
            return Ok(None);
        }
        let was_missing = self.window.is_none();
        let previous_id = self
            .window
            .as_ref()
            .map(|window| window.listed.window_id)
            .or(self.last_window_id);
        if let Some(old) = self.window.take() {
            stop_stream(old.stream, STREAM_CALL_LIMIT)?;
        }
        if self.stop_count.is_stop_read() {
            return Ok(None);
        }
        let size = output_size(listed.width_pt, listed.height_pt);
        self.next_stream_generation += 1;
        let generation = self.next_stream_generation;
        let channel = self.channels.stream.clone();
        let sink: StreamSink = Arc::new(move |event| {
            #[cfg(debug_assertions)]
            let Some(event) = crate::inject::pass_video_event(event) else {
                return;
            };
            let _unheard = channel.send((generation, event));
        });
        tracing::info!(
            video.window_id = listed.window_id,
            video.title = listed.title.as_deref(),
            video.is_on_screen = listed.is_on_screen,
            video.width_px = size.width_px,
            video.height_px = size.height_px,
            "video.stream_starting"
        );
        let started = WindowStream::start(window, size, &sink);
        let record = record_of(&listed);
        self.window = Some(RecordedWindow {
            listed,
            stream: None,
            stream_generation: generation,
        });
        self.last_window_id = Some(record.window_id);
        self.last_callback = Instant::now();
        self.last_status = FrameStatus::NotYet;
        match started {
            Ok(stream) => {
                if let Some(window) = &mut self.window {
                    window.stream = Some(stream);
                }
            }
            Err(error) if error.is_user_stop() => {
                return Ok(Some(SegmentStop::new(StopCause::User, Instant::now())));
            }
            Err(error) => {
                tracing::warn!(exception.message = %error, "video.stream_start_failed");
                self.rebuild(None)?;
                return Ok(None);
            }
        }
        let is_other_window = previous_id.is_some_and(|previous| previous != record.window_id);
        if was_missing || is_other_window {
            let cause = match previous_id {
                None => return Ok(None),
                Some(previous) if previous == record.window_id => WindowChangeCause::Returned,
                Some(_) => WindowChangeCause::Replaced,
            };
            self.note_window_change(cause, Some(record), Some(size))?;
        }
        Ok(None)
    }

    /// Closes the segment, if one started, by `close_by`. `cause` is `None` after an error.
    fn close(&mut self, cause: Option<StopCause>, close_by: Instant) -> Result<(), SegmentError> {
        let remaining = || close_by.saturating_duration_since(Instant::now());
        let stopped = self.window.take().map_or(Ok(()), |window| {
            stop_stream(window.stream, remaining().min(STREAM_CALL_LIMIT))
        });
        // The part and the manifest still close after a stop that timed out; the error then
        // ends the process.
        let ended = self
            .end_gap()
            .and_then(|()| self.finish_part(remaining()))
            .and(stopped);
        let Some(mut writer) = self.writer.take() else {
            if cause.is_some() {
                // No window showed, so no segment exists; the app still hears the stop.
                self.reports.send(&VideoReport::Stopped {
                    segment: self.request.number,
                })?;
            }
            return ended;
        };
        writer.set_frame_counts(self.counts);
        let stopped_by = cause.map_or("error", StopCause::as_str).to_owned();
        let record = writer
            .close(Some(stopped_by), capture::utc_anchor())
            .map_err(SegmentError::Close)?;
        tracing::info!(
            segment.number = record.number,
            video.frames_captured = self.counts.captured,
            video.frames_encoded = self.counts.encoded,
            video.frames_output = self.counts.output,
            video.frames_repeated = self.counts.repeated,
            video.frames_dropped = self.counts.dropped,
            video.fragment_count = record.fragments.len(),
            video.stream_rebuilds = record.stream_rebuilds,
            "video.segment_stopped"
        );
        if cause.is_some() {
            self.reports.send(&VideoReport::Stopped {
                segment: self.request.number,
            })?;
        }
        ended
    }

    #[cfg(debug_assertions)]
    fn apply_injected_events() {
        use crate::inject::{DeviceEvent, VideoFeed};
        for event in crate::inject::take_due_device_events() {
            tracing::info!(inject.event = %event, "inject.video");
            match event {
                DeviceEvent::VideoStall => crate::inject::set_video_feed(VideoFeed::Stalled),
                DeviceEvent::VideoStill => crate::inject::set_video_feed(VideoFeed::still()),
                DeviceEvent::VideoResume => crate::inject::set_video_feed(VideoFeed::Live),
                DeviceEvent::DefaultInput
                | DeviceEvent::DefaultOutput
                | DeviceEvent::DeviceList
                | DeviceEvent::MicLost
                | DeviceEvent::MicZeros
                | DeviceEvent::MicSound
                | DeviceEvent::LidClosed
                | DeviceEvent::LidOpen
                | DeviceEvent::CandidateNoBuffers
                | DeviceEvent::TapSilentOpen
                | DeviceEvent::TapFormat
                | DeviceEvent::TapStall
                | DeviceEvent::TapStallQuiet
                | DeviceEvent::TapDead
                | DeviceEvent::HalProbe => {
                    tracing::warn!(inject.event = %event, "inject.not_for_video");
                }
            }
        }
    }
}

/// Stops a stream, waiting at most `limit`. A stop that fails leaves nothing to do.
///
/// # Errors
///
/// Fails when the stop times out: the capture may still run, and only the process's exit ends
/// it for sure.
fn stop_stream(stream: Option<WindowStream>, limit: Duration) -> Result<(), SegmentError> {
    let Some(stream) = stream else {
        return Ok(());
    };
    match stream.stop(limit) {
        Ok(()) => Ok(()),
        Err(error) if error.is_timed_out() => {
            tracing::error!(exception.message = %error, "video.stream_stop_timed_out");
            Err(SegmentError::StreamStopTimedOut(error.to_string()))
        }
        Err(error) => {
            tracing::warn!(exception.message = %error, "video.stream_stop_failed");
            Ok(())
        }
    }
}

fn record_of(listed: &ListedWindow) -> WindowRecord {
    WindowRecord {
        window_id: listed.window_id,
        bundle_id: listed.bundle_id.clone(),
        title: listed.title.clone(),
        pid: listed.pid,
    }
}

/// The frame size for a window: its point size, scaled down to fit 1280x720 keeping the
/// aspect, in even pixels, as 4:2:0 needs.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the sides are clamped to 2..=4096 first"
)]
fn output_size(width_pt: f64, height_pt: f64) -> OutputSize {
    let width_pt = width_pt.max(2.0);
    let height_pt = height_pt.max(2.0);
    let scale = (MAX_WIDTH_PX / width_pt)
        .min(MAX_HEIGHT_PX / height_pt)
        .min(1.0);
    let even = |side: f64| {
        let side = (side * scale).clamp(2.0, MAX_SIDE_PX);
        ((side / 2.0).round() * 2.0) as u32
    };
    OutputSize {
        width_px: even(width_pt),
        height_px: even(height_pt),
    }
}
