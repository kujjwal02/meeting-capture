//! The app's writer for mc-meet's tab video. mc-meet encodes and muxes the Meet tab in Chrome and
//! sends each fMP4 fragment with its part's init segment and its first frame's UTC. This writes
//! them into the same numbered segments, playlist and `video.json` entries that `mc-capture video`
//! writes, so repair, finalize and the purge read both alike.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use jiff::{SignedDuration, Timestamp};
use sources::Platform;

use crate::manifest::{FrameCounts, VideoCodec, VideoSegmentRecord};
use crate::writer::{FragmentWritten, FrameTime, SegmentSource, SegmentStart, VideoSegmentWriter};
use crate::{VideoWriterError, next_segment_number};

/// One fragment as mc-meet sent it, its base64 decoded.
#[derive(Debug, Clone)]
pub struct TabFragment {
    /// New for each stream mc-meet opens.
    pub capture_id: String,
    /// Counts from 1 within the capture. A new part has its own init segment and decode times.
    pub part: u32,
    pub codec: VideoCodec,
    pub width_px: u32,
    pub height_px: u32,
    /// The part's init segment. Every fragment carries it, so nothing depends on an earlier
    /// message reaching the app.
    pub init_segment: Vec<u8>,
    /// Counts from 1 within the capture.
    pub index: u64,
    /// As mc-meet placed it: its part's clock offset plus monotonic time.
    pub first_frame_at: Timestamp,
    /// UTC minus the browser's monotonic clock, fixed at the part's first frame. `None` from an
    /// older mc-meet.
    pub part_clock_offset_ms: Option<f64>,
    /// The same offset when the fragment was sent.
    pub clock_offset_ms: Option<f64>,
    pub duration_ms: f64,
    pub frame_count: u32,
    /// How long mc-meet's playback of the tab audio lags its frames. `None` for a tab with no
    /// audio track.
    pub playback_latency_ms: Option<f64>,
    /// The playback's own base latency, logged beside `playback_latency_ms` to tell the
    /// output device's share apart.
    pub playback_base_latency_ms: Option<f64>,
    /// One `moof` and its `mdat`, from a keyframe.
    pub bytes: Vec<u8>,
}

impl TabFragment {
    /// When its last frame ends. A fragment too long to add counts as never ending.
    #[must_use]
    pub fn end(&self) -> Timestamp {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a fragment lasts about 2 s, far inside i64 nanoseconds"
        )]
        let duration = SignedDuration::from_nanos((self.duration_ms * 1e6).round() as i64);
        self.first_frame_at
            .checked_add(duration)
            .unwrap_or(Timestamp::MAX)
    }
}

/// The audio a fragment pairs with, as the audio process reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPairing {
    pub segment: u32,
    /// Sleeps the audio process reported so far.
    pub sleep_count: u64,
    /// When the audio last started, at the call start or a wake.
    pub awake_since: Timestamp,
}

/// A clock offset step larger than this between parts is logged.
const CLOCK_STEP_LOG_MS: f64 = 1000.0;

/// The browser clock offset the writer places parts by, and the audio it held for.
#[derive(Debug, Clone, Copy)]
struct ClockAnchor {
    offset_ms: f64,
    audio: AudioPairing,
}

/// What became of one fragment.
#[derive(Debug, Clone)]
pub enum TabFragmentOutcome {
    Written {
        segment: u32,
        /// The fragment opened this segment.
        is_segment_new: bool,
        fragment: FragmentWritten,
    },
    /// mc-meet sent it again after a reconnect, and it is already on disk.
    Repeat,
    /// It belongs to a capture that a later one replaced, and came late.
    FromEndedCapture,
    /// Its part began before a sleep or clock step and it arrived after the audio's wake, so
    /// its UTC is on the old clock and must not anchor the new audio pairing.
    StaleClock { step_ms: f64 },
    /// It ended before the audio last started, so it was held across a sleep, and would open
    /// or anchor the wake's segment with frames from before the wake.
    BeforeSegment,
}

/// Why the open segment closed. Written to `video.json` as `stopped_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabSegmentEnd {
    /// The audio slept; the next fragment after the wake opens a segment on its new number.
    Sleep,
    /// The call stopped recording.
    Stopped,
    /// A fragment came for another audio segment.
    AudioSegmentChanged,
}

impl TabSegmentEnd {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::Stopped => "stopped",
            Self::AudioSegmentChanged => "audio_segment_changed",
        }
    }
}

/// One segment the writer has open.
struct OpenTabSegment {
    writer: VideoSegmentWriter,
    audio_segment: u32,
    /// The capture and part whose init segment the open part holds.
    part: Option<(String, u32)>,
    frames: FrameCounts,
}

/// Writes one call's tab video. Holds `video.lock` while a segment is open.
pub struct TabVideoWriter {
    folder: PathBuf,
    call_id: String,
    open: Option<OpenTabSegment>,
    /// The capture whose fragments the writer takes now, and the last index it wrote.
    current_capture: Option<(String, u64)>,
    /// Captures a later one replaced. Their late fragments are dropped, since their part would
    /// land among the newer capture's frames.
    ended_captures: HashSet<String>,
    /// The audio's UTC comes from one clock reading per audio segment, so a wall clock step
    /// with no sleep moves the browser's UTC but not the audio's. Parts keep this offset until
    /// the audio sleeps or changes segment, when its next reading takes in the step too.
    clock_anchor: Option<ClockAnchor>,
}

impl TabVideoWriter {
    #[must_use]
    pub fn new(folder: &Path, call_id: &str) -> Self {
        Self {
            folder: folder.to_owned(),
            call_id: call_id.to_owned(),
            open: None,
            current_capture: None,
            ended_captures: HashSet::new(),
            clock_anchor: None,
        }
    }

    /// The open segment's number.
    #[must_use]
    pub fn open_segment(&self) -> Option<u32> {
        self.open.as_ref().map(|open| open.writer.number())
    }

    /// Writes one fragment into the segment that pairs with `audio_segment`, opening the
    /// segment and the part as needed.
    ///
    /// # Errors
    ///
    /// Fails when a segment cannot start or a file or `video.json` cannot be written. A failed
    /// fragment is not counted as written, so a resend of it is written.
    pub fn write(
        &mut self,
        fragment: &TabFragment,
        audio: AudioPairing,
    ) -> Result<TabFragmentOutcome, VideoWriterError> {
        let audio_segment = audio.segment;
        if self.ended_captures.contains(&fragment.capture_id) {
            return Ok(TabFragmentOutcome::FromEndedCapture);
        }
        if self
            .current_capture
            .as_ref()
            .is_some_and(|(capture_id, index)| {
                *capture_id == fragment.capture_id && fragment.index <= *index
            })
        {
            return Ok(TabFragmentOutcome::Repeat);
        }
        if self.is_before_segment(fragment, audio) {
            return Ok(TabFragmentOutcome::BeforeSegment);
        }
        if let Some(step_ms) = self.stale_step_ms(fragment, audio) {
            return Ok(TabFragmentOutcome::StaleClock { step_ms });
        }
        // Finalize moves a paired remote stem by the segment's playback latency, so a segment
        // holds video of one audio segment only.
        if self
            .open
            .as_ref()
            .is_some_and(|open| open.audio_segment != audio_segment)
        {
            self.close_segment(TabSegmentEnd::AudioSegmentChanged)?;
        }
        let is_segment_new = self.open.is_none();
        let open = match &mut self.open {
            Some(open) => open,
            empty @ None => empty.insert(Self::start_segment(
                &self.folder,
                &self.call_id,
                fragment.codec,
                audio_segment,
            )?),
        };
        let part_key = (fragment.capture_id.clone(), fragment.part);
        let is_part_new = open.part.as_ref() != Some(&part_key);
        let first_frame_at = Self::place(&mut self.clock_anchor, fragment, audio, is_part_new);
        if is_part_new {
            open.writer.start_part(
                &fragment.init_segment,
                fragment.codec,
                fragment.width_px,
                fragment.height_px,
                FrameTime::Utc(first_frame_at),
            )?;
            open.part = Some(part_key);
        }
        let written = open.writer.write_fragment(
            &fragment.bytes,
            Some(FrameTime::Utc(first_frame_at)),
            fragment.duration_ms,
        )?;
        let frame_count = u64::from(fragment.frame_count);
        open.frames.captured += frame_count;
        open.frames.encoded += frame_count;
        open.frames.output += frame_count;
        open.writer.set_frame_counts(open.frames);
        if let Some(latency_ms) = fragment.playback_latency_ms {
            open.writer.set_playback_latency_ms(latency_ms);
        }
        let segment = open.writer.number();
        match self.current_capture.take() {
            Some((capture_id, _)) if capture_id != fragment.capture_id => {
                self.ended_captures.insert(capture_id);
            }
            Some(_) | None => {}
        }
        self.current_capture = Some((fragment.capture_id.clone(), fragment.index));
        Ok(TabFragmentOutcome::Written {
            segment,
            is_segment_new,
            fragment: written,
        })
    }

    fn is_anchor_new(&self, audio: AudioPairing) -> bool {
        self.clock_anchor
            .as_ref()
            .is_none_or(|anchor| anchor.audio != audio)
    }

    /// A fragment that would open a segment or take an anchor, but ended before the audio last
    /// started. Checked on its own UTC, so it holds whatever path delayed the fragment.
    fn is_before_segment(&self, fragment: &TabFragment, audio: AudioPairing) -> bool {
        let is_segment_new = self
            .open
            .as_ref()
            .is_none_or(|open| open.audio_segment != audio.segment);
        (is_segment_new || self.is_anchor_new(audio)) && fragment.end() < audio.awake_since
    }

    /// The clock step inside a fragment's part, when the fragment would take a new anchor. A
    /// fragment encoded before a sleep can arrive after the wake; its part offset is from before
    /// the sleep, and anchoring on it would place all later video early by the sleep.
    fn stale_step_ms(&self, fragment: &TabFragment, audio: AudioPairing) -> Option<f64> {
        let is_anchor_new = self.is_anchor_new(audio);
        let step_ms = fragment.clock_offset_ms? - fragment.part_clock_offset_ms?;
        (is_anchor_new && step_ms.abs() > CLOCK_STEP_LOG_MS).then_some(step_ms)
    }

    /// The fragment's first frame on the anchored offset. A new anchor is taken at the first
    /// fragment and after the audio slept or changed segment.
    fn place(
        anchor: &mut Option<ClockAnchor>,
        fragment: &TabFragment,
        audio: AudioPairing,
        is_part_new: bool,
    ) -> Timestamp {
        let Some(part_offset_ms) = fragment.part_clock_offset_ms else {
            return fragment.first_frame_at;
        };
        let kept = match anchor {
            Some(kept) if kept.audio == audio => *kept,
            Some(_) | None => {
                if let Some(previous) = anchor.as_ref()
                    && (part_offset_ms - previous.offset_ms).abs() > CLOCK_STEP_LOG_MS
                {
                    tracing::info!(
                        video.capture_id = fragment.capture_id,
                        video.part = fragment.part,
                        video.clock_step_ms = part_offset_ms - previous.offset_ms,
                        "video.tab_clock_reanchored"
                    );
                }
                *anchor = Some(ClockAnchor {
                    offset_ms: part_offset_ms,
                    audio,
                });
                return fragment.first_frame_at;
            }
        };
        let step_ms = part_offset_ms - kept.offset_ms;
        if is_part_new && step_ms.abs() > CLOCK_STEP_LOG_MS {
            tracing::warn!(
                video.capture_id = fragment.capture_id,
                video.part = fragment.part,
                video.clock_step_ms = step_ms,
                video.clock_offset_ms = fragment.clock_offset_ms,
                "video.tab_clock_step_kept"
            );
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a clock step in milliseconds is far inside i64 nanoseconds"
        )]
        let step = SignedDuration::from_nanos((step_ms * 1e6).round() as i64);
        fragment
            .first_frame_at
            .checked_sub(step)
            .unwrap_or(fragment.first_frame_at)
    }

    fn start_segment(
        folder: &Path,
        call_id: &str,
        codec: VideoCodec,
        audio_segment: u32,
    ) -> Result<OpenTabSegment, VideoWriterError> {
        let number = next_segment_number(folder)?.max(audio_segment);
        let writer = VideoSegmentWriter::start(SegmentStart {
            folder,
            number,
            audio_segment,
            call_id,
            platform: Platform::Meet,
            codec,
            source: SegmentSource::MeetTab,
        })?;
        Ok(OpenTabSegment {
            writer,
            audio_segment,
            part: None,
            frames: FrameCounts::default(),
        })
    }

    /// Closes the open segment, if any. The next fragment opens a new one.
    ///
    /// # Errors
    ///
    /// Fails when the playlist or `video.json` cannot be written. Launch repair then ends the
    /// segment.
    pub fn close_segment(
        &mut self,
        end: TabSegmentEnd,
    ) -> Result<Option<VideoSegmentRecord>, VideoWriterError> {
        let Some(open) = self.open.take() else {
            return Ok(None);
        };
        open.writer
            .close(Some(end.as_str().to_owned()), None)
            .map(Some)
    }
}
