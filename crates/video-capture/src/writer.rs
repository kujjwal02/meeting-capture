//! Writes one video segment: `NNN-video/` with each part's init segment, the 2 s fragments, the
//! playlist that lists them, and the segment's entry in `video.json`.
//!
//! The order of each write is what repair relies on. A fragment goes to `.NAME.partial` and is
//! fsynced, then the playlist line is appended and fsynced, then the file is renamed to `NAME`.
//! So a partial file the playlist lists is complete, and any other partial file is not.
//! `video.json` catches up every 5 fragments and at every event; repair reads the rest from the
//! playlist.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use call_folder::FolderLock;
use jiff::{SignedDuration, Timestamp};
use sources::Platform;
use stem_timing::UtcAnchor;

use crate::manifest::{
    ClockRecord, FragmentRecord, FrameCounts, PartRecord, VideoCodec, VideoGapRecord,
    VideoManifest, VideoSegmentRecord, VideoSegmentState, VideoSource, WindowChangeRecord,
    WindowRecord,
};
use crate::playlist::{self, ENDLIST_LINE, PLAYLIST_FILE_NAME, PlaylistEntry};
use crate::{
    LOCK_FILE_NAME, SEGMENT_CLOSE_LIMIT, VideoWriterError, highest_segment_number, ms_to_ns,
    segment_folder_name,
};

/// How long a segment start waits for the folder's lock. A helper the app stopped holds it while
/// it closes, for up to `SEGMENT_CLOSE_LIMIT`, and a repair holds it for a moment.
const LOCK_WAIT: Duration = SEGMENT_CLOSE_LIMIT.saturating_add(Duration::from_secs(1));
/// `video.json` catches up after this many fragments, about 10 s of video.
const FRAGMENTS_PER_CHECKPOINT: u32 = 5;

/// What a segment start needs.
pub struct SegmentStart<'a> {
    pub folder: &'a Path,
    pub number: u32,
    /// The audio segment open now, which this one pairs with.
    pub audio_segment: u32,
    pub call_id: &'a str,
    pub platform: Platform,
    pub codec: VideoCodec,
    pub source: SegmentSource,
}

/// What records the segment.
pub enum SegmentSource {
    /// A window through ScreenCaptureKit, with the host clock read against UTC now.
    Window {
        window: WindowRecord,
        clock: UtcAnchor,
    },
    /// mc-meet's tab capture. Chrome stamps frames with UTC, so the segment needs no clock.
    MeetTab,
}

/// When a part's or a fragment's first frame was captured, on the clock its source stamps.
#[derive(Debug, Clone, Copy)]
pub enum FrameTime {
    HostTime(u64),
    Utc(Timestamp),
}

/// One fragment as it landed on disk.
#[derive(Debug, Clone)]
pub struct FragmentWritten {
    pub file: String,
    pub bytes: u64,
    pub duration_ms: f64,
}

/// Holds `video.lock` for the segment's whole life.
pub struct VideoSegmentWriter {
    folder: PathBuf,
    segment_dir: PathBuf,
    manifest: VideoManifest,
    /// The host clock reading and its UTC. `None` for a Meet tab segment.
    clock: Option<(UtcAnchor, Timestamp)>,
    playlist: File,
    next_fragment: u32,
    fragments_since_checkpoint: u32,
    _lock: FolderLock,
}

impl VideoSegmentWriter {
    /// Repairs the folder, then opens the segment: its folder, its playlist and its entry in
    /// `video.json`.
    ///
    /// # Errors
    ///
    /// Fails when another process holds the folder for longer than `LOCK_WAIT`, the number is
    /// taken, the folder cannot be written, or `video.json` is unreadable or from a newer schema.
    pub fn start(start: SegmentStart<'_>) -> Result<Self, VideoWriterError> {
        let folder = start.folder;
        let io_error = |path: &Path| {
            let path = path.to_owned();
            move |source| VideoWriterError::Io { path, source }
        };
        std::fs::create_dir_all(folder).map_err(io_error(folder))?;
        let lock = FolderLock::take_within(folder, LOCK_FILE_NAME, LOCK_WAIT)
            .map_err(io_error(&folder.join(LOCK_FILE_NAME)))?
            .ok_or_else(|| VideoWriterError::FolderBusy {
                folder: folder.to_owned(),
            })?;
        if let Err(error) = crate::repair::repair_locked(folder) {
            tracing::warn!(
                file.directory = %folder.display(),
                exception.message = %error,
                "video.repair_failed"
            );
        }
        let mut manifest = VideoManifest::load_or_new(folder)?;
        let highest = highest_segment_number(folder, &manifest)?;
        if start.number <= highest {
            return Err(VideoWriterError::SegmentNumberTaken {
                folder: folder.to_owned(),
                number: start.number,
                highest,
            });
        }
        let folder_name = segment_folder_name(start.number);
        let segment_dir = folder.join(&folder_name);
        std::fs::create_dir(&segment_dir).map_err(io_error(&segment_dir))?;
        let playlist_path = segment_dir.join(PLAYLIST_FILE_NAME);
        let mut playlist = File::options()
            .create_new(true)
            .append(true)
            .open(&playlist_path)
            .map_err(io_error(&playlist_path))?;
        playlist
            .write_all(playlist::header().as_bytes())
            .and_then(|()| playlist.sync_data())
            .map_err(io_error(&playlist_path))?;
        crate::sync_folder(&segment_dir)?;
        let (source, window, clock) = match start.source {
            SegmentSource::Window { window, clock } => {
                let clock_utc = utc_timestamp(clock.utc)?;
                (VideoSource::Window, Some(window), Some((clock, clock_utc)))
            }
            SegmentSource::MeetTab => (VideoSource::MeetTab, None, None),
        };
        manifest.segments.push(VideoSegmentRecord {
            number: start.number,
            audio_segment: Some(start.audio_segment),
            call_id: Some(start.call_id.to_owned()),
            platform: start.platform,
            source,
            state: VideoSegmentState::Open,
            folder: folder_name,
            playlist: PLAYLIST_FILE_NAME.to_owned(),
            codec: start.codec,
            clock: clock.map(|(anchor, utc)| clock_record(anchor, utc)),
            closing_clock: None,
            closed_at: None,
            window,
            first_host_time_ns: None,
            started_at: None,
            last_host_time_ns: None,
            parts: Vec::new(),
            fragments: Vec::new(),
            gaps: Vec::new(),
            window_changes: Vec::new(),
            frames: FrameCounts::default(),
            stream_rebuilds: 0,
            stopped_by: None,
            playback_latency_ms: None,
            repair: None,
        });
        manifest.store(folder)?;
        tracing::info!(
            file.directory = %folder.display(),
            segment.number = start.number,
            call.id = start.call_id,
            video.source = source.as_str(),
            "video.segment_started"
        );
        Ok(Self {
            folder: folder.to_owned(),
            segment_dir,
            manifest,
            clock,
            playlist,
            next_fragment: 1,
            fragments_since_checkpoint: 0,
            _lock: lock,
        })
    }

    #[must_use]
    pub fn number(&self) -> u32 {
        self.record().number
    }

    #[must_use]
    pub fn segment_dir(&self) -> &Path {
        &self.segment_dir
    }

    #[must_use]
    pub fn fragment_count(&self) -> usize {
        self.record().fragments.len()
    }

    fn record(&self) -> &VideoSegmentRecord {
        // `start` pushed the segment last, and nothing else adds one while the lock is held.
        self.manifest
            .segments
            .last()
            .unwrap_or_else(|| unreachable!("the writer's segment is in its manifest"))
    }

    fn record_mut(&mut self) -> &mut VideoSegmentRecord {
        self.manifest
            .segments
            .last_mut()
            .unwrap_or_else(|| unreachable!("the writer's segment is in its manifest"))
    }

    /// Writes a part's init segment and lists it. Fragments after it decode with it.
    ///
    /// # Errors
    ///
    /// Fails when the file, the playlist or `video.json` cannot be written.
    pub fn start_part(
        &mut self,
        init_segment: &[u8],
        codec: VideoCodec,
        width_px: u32,
        height_px: u32,
        first_frame: FrameTime,
    ) -> Result<u32, VideoWriterError> {
        let number = u32::try_from(self.record().parts.len()).unwrap_or(u32::MAX - 1) + 1;
        let file = format!("init-{number}.mp4");
        write_durably(&self.segment_dir, &file, init_segment)?;
        let (first_host_time_ns, started_at, first_frame_at) = match first_frame {
            FrameTime::HostTime(host_time_ns) => {
                (Some(host_time_ns), self.utc_at(host_time_ns), None)
            }
            FrameTime::Utc(at) => (None, Some(at), Some(at)),
        };
        let record = self.record_mut();
        if record.parts.is_empty() {
            record.first_host_time_ns = first_host_time_ns;
            record.started_at = started_at;
        }
        record.parts.push(PartRecord {
            number,
            init: file.clone(),
            codec,
            width_px,
            height_px,
            first_host_time_ns,
            first_frame_at,
        });
        // Listed in `video.json` before the playlist, so a part the playlist names always has
        // its size on record.
        self.checkpoint()?;
        self.append_playlist(&playlist::entry_lines(
            &PlaylistEntry::Init { file: file.clone() },
            number == 1,
        ))?;
        tracing::info!(
            segment.number = self.number(),
            video.part = number,
            video.width_px = width_px,
            video.height_px = height_px,
            file.name = file,
            "video.part_started"
        );
        Ok(number)
    }

    /// Writes one fragment of the current part, durably, and lists it.
    ///
    /// # Errors
    ///
    /// Fails when no part started, or the file or the playlist cannot be written.
    pub fn write_fragment(
        &mut self,
        fragment: &[u8],
        first_frame: Option<FrameTime>,
        duration_ms: f64,
    ) -> Result<FragmentWritten, VideoWriterError> {
        let part = u32::try_from(self.record().parts.len()).unwrap_or(u32::MAX);
        if part == 0 {
            return Err(VideoWriterError::NoPart);
        }
        let file = format!("{:05}.m4s", self.next_fragment);
        let partial = partial_name(&file);
        let partial_path = self.segment_dir.join(&partial);
        write_synced(&partial_path, fragment)?;
        self.append_playlist(&playlist::entry_lines(
            &PlaylistEntry::Fragment {
                file: file.clone(),
                duration_ms,
            },
            false,
        ))?;
        // The playlist names the file now, so a failed rename below must not reuse the name.
        self.next_fragment += 1;
        let final_path = self.segment_dir.join(&file);
        std::fs::rename(&partial_path, &final_path).map_err(|source| VideoWriterError::Io {
            path: final_path,
            source,
        })?;
        crate::sync_folder(&self.segment_dir)?;
        let bytes = u64::try_from(fragment.len()).unwrap_or(u64::MAX);
        let (first_host_time_ns, first_frame_at) = match first_frame {
            Some(FrameTime::HostTime(host_time_ns)) => (Some(host_time_ns), None),
            Some(FrameTime::Utc(at)) => (None, Some(at)),
            None => (None, None),
        };
        let record = self.record_mut();
        record.fragments.push(FragmentRecord {
            file: file.clone(),
            part,
            first_host_time_ns,
            first_frame_at,
            duration_ms,
            bytes,
        });
        if let Some(first_ns) = first_host_time_ns {
            let end_ns = first_ns.saturating_add(ms_to_ns(duration_ms));
            record.last_host_time_ns = Some(
                record
                    .last_host_time_ns
                    .map_or(end_ns, |last| last.max(end_ns)),
            );
        }
        self.fragments_since_checkpoint += 1;
        if self.fragments_since_checkpoint >= FRAGMENTS_PER_CHECKPOINT {
            self.checkpoint()?;
        }
        Ok(FragmentWritten {
            file,
            bytes,
            duration_ms,
        })
    }

    /// Lists a gap the writer filled with repeats.
    ///
    /// # Errors
    ///
    /// Fails when `video.json` cannot be stored. The gap stays listed for the next store.
    pub fn note_gap(&mut self, gap: VideoGapRecord) -> Result<(), VideoWriterError> {
        self.record_mut().gaps.push(gap);
        self.checkpoint()
    }

    /// Lists a window change.
    ///
    /// # Errors
    ///
    /// As `note_gap`.
    pub fn note_window_change(
        &mut self,
        change: WindowChangeRecord,
    ) -> Result<(), VideoWriterError> {
        self.record_mut().window_changes.push(change);
        self.checkpoint()
    }

    /// Counts a stream rebuild.
    ///
    /// # Errors
    ///
    /// As `note_gap`.
    pub fn note_stream_rebuild(&mut self) -> Result<(), VideoWriterError> {
        self.record_mut().stream_rebuilds += 1;
        self.checkpoint()
    }

    /// The frame counts the next store writes.
    pub fn set_frame_counts(&mut self, counts: FrameCounts) {
        self.record_mut().frames = counts;
    }

    /// The tab audio playback latency the next store writes. The latest wins: the playback
    /// path only grows its buffer.
    pub fn set_playback_latency_ms(&mut self, latency_ms: f64) {
        self.record_mut().playback_latency_ms = Some(latency_ms);
    }

    /// Stores `video.json`.
    ///
    /// # Errors
    ///
    /// Fails when it cannot be stored. The next store tries again.
    pub fn checkpoint(&mut self) -> Result<(), VideoWriterError> {
        self.fragments_since_checkpoint = 0;
        self.manifest.store(&self.folder)
    }

    /// Ends the playlist and marks the segment closed. `closing` is a second host clock
    /// reading, `None` for a Meet tab segment.
    ///
    /// # Errors
    ///
    /// Fails when the playlist or `video.json` cannot be written. Launch repair then finds the
    /// segment open and ends it.
    pub fn close(
        mut self,
        stopped_by: Option<String>,
        closing: Option<UtcAnchor>,
    ) -> Result<VideoSegmentRecord, VideoWriterError> {
        self.append_playlist(&format!("{ENDLIST_LINE}\n"))?;
        let closing_clock = match closing {
            Some(anchor) => Some(clock_record(anchor, utc_timestamp(anchor.utc)?)),
            None => None,
        };
        let record = self.record_mut();
        record.state = VideoSegmentState::Closed;
        record.closed_at = Some(closing_clock.map_or_else(Timestamp::now, |clock| clock.utc));
        record.closing_clock = closing_clock;
        record.stopped_by = stopped_by;
        self.checkpoint()?;
        let record = self.record().clone();
        tracing::info!(
            file.directory = %self.folder.display(),
            segment.number = record.number,
            video.fragment_count = record.fragments.len(),
            video.frames_encoded = record.frames.encoded,
            video.frames_repeated = record.frames.repeated,
            "video.segment_closed"
        );
        Ok(record)
    }

    fn append_playlist(&mut self, lines: &str) -> Result<(), VideoWriterError> {
        self.playlist
            .write_all(lines.as_bytes())
            .and_then(|()| self.playlist.sync_data())
            .map_err(|source| VideoWriterError::Io {
                path: self.segment_dir.join(PLAYLIST_FILE_NAME),
                source,
            })
    }

    /// A host time in UTC, through the segment's clock reading. `None` in a Meet tab segment.
    #[must_use]
    pub fn utc_at(&self, host_time_ns: u64) -> Option<Timestamp> {
        let (anchor, anchor_utc) = self.clock?;
        let offset_ns = i128::from(host_time_ns) - i128::from(anchor.host_time_ns);
        let offset = SignedDuration::from_nanos(i64::try_from(offset_ns).ok()?);
        anchor_utc.checked_add(offset).ok()
    }
}

pub(crate) fn partial_name(file: &str) -> String {
    format!(".{file}.partial")
}

/// The fragment a partial file stands for, as `00007.m4s` for `.00007.m4s.partial`.
pub(crate) fn final_name_of_partial(partial: &str) -> Option<&str> {
    partial.strip_prefix('.')?.strip_suffix(".partial")
}

/// On macOS `sync_all` is `F_FULLFSYNC`, so the bytes reach the disk, not only its cache.
fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), VideoWriterError> {
    File::create(path)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
        .map_err(|source| VideoWriterError::Io {
            path: path.to_owned(),
            source,
        })
}

/// Writes a file that is either whole or absent after a crash.
fn write_durably(dir: &Path, file: &str, bytes: &[u8]) -> Result<(), VideoWriterError> {
    let partial_path = dir.join(partial_name(file));
    write_synced(&partial_path, bytes)?;
    let final_path = dir.join(file);
    std::fs::rename(&partial_path, &final_path).map_err(|source| VideoWriterError::Io {
        path: final_path,
        source,
    })?;
    crate::sync_folder(dir)
}

fn clock_record(clock: UtcAnchor, utc: Timestamp) -> ClockRecord {
    ClockRecord {
        host_time_ns: clock.host_time_ns,
        utc,
        read_gap_ns: clock.read_gap_ns,
    }
}

fn utc_timestamp(utc: SystemTime) -> Result<Timestamp, VideoWriterError> {
    Timestamp::try_from(utc).map_err(|_| VideoWriterError::ClockOutOfRange)
}
