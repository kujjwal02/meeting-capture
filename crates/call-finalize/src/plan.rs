//! Where each recorded piece of a call sits on the one timeline `call.mp4` plays on. Portable: it
//! reads `audio.json` and `video.json` and opens no media.
//!
//! The timeline is UTC, from the earliest piece. Inside a segment pair the offset between video
//! and stems is a host time difference: a window capture part starts at its first frame's host
//! time, read through the paired audio segment's clock reading, the same reading that places the
//! stems. Segments that do not pair, and separate pairs, sit apart by their own clock readings,
//! since host time stands still while the Mac sleeps.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use capture_protocol::finalize::{InputTrack, SkipReason, SkippedInput};
use jiff::Timestamp;
use stem_timing::Stem;
use stem_writer::{AudioManifest, SegmentState, StemRecord};
use video_capture::{ClockRecord, VideoManifest, VideoSegmentRecord, VideoSegmentState};

use crate::FinalizeError;

/// The mix keeps the stems' rate: resampling up would add bytes and no speech.
pub const MIX_RATE_HZ: u32 = stem_writer::OUTPUT_RATE_HZ;

/// Outside a run ahead, a stem's file and host time advance together, so a 1 s chunk at its own
/// host time places every sample in it.
const STEM_CHUNK_SAMPLES: u64 = MIX_RATE_HZ as u64;

/// Inside a run ahead, steering moves the file against host time by up to 1 ms per second. A
/// 10 ms chunk at its own host time then sits at most 10 us from where a 1 s one would leave
/// a 1 ms hole or a 1 ms double.
const STEERED_CHUNK_SAMPLES: u64 = MIX_RATE_HZ as u64 / 100;

/// A hole this short between two chunks of one stem is steering, not lost audio: the later
/// chunk starts that many samples earlier in its file instead, so the mix has no click. Real
/// losses are silence in the file itself.
const MAX_FILLED_HOLE_SAMPLES: u64 = MIX_RATE_HZ as u64 / 1000;

/// Readings whose UTC minus host time agree this closely come from one boot with no sleep
/// between, so one of them places them all. A sleep stops host time for at least seconds, and
/// a reboot restarts it.
const SAME_CLOCK_RUN_NS: i128 = 500_000_000;

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// When a video part's first frame was captured: the input finalize places each part by.
#[derive(Debug, Clone, Copy)]
pub enum PartStart {
    /// Window capture: frames carry the host clock the stems use. `own_clock` is the video
    /// segment's reading, used only when no audio segment pairs with it.
    HostTime {
        first_host_time_ns: u64,
        own_clock: Option<ClockRecord>,
    },
    /// Meet tab capture: Chrome stamps each frame with UTC as it arrives. The remote stem hears
    /// the tab `remote_trail_ms` later, the playback latency mc-meet logs at capture, so the
    /// paired remote stem moves that much earlier.
    BrowserUtc {
        first_frame_at: Timestamp,
        remote_trail_ms: f64,
    },
}

/// One run of video fragments that share an init segment.
#[derive(Debug, Clone)]
pub struct VideoPartInput {
    pub segment: u32,
    pub part: u32,
    /// The audio segment recorded beside it.
    pub audio_segment: Option<u32>,
    pub start: PartStart,
    pub init: PathBuf,
    pub fragments: Vec<PathBuf>,
    pub length_ms: f64,
}

/// A video part at its place on the timeline.
#[derive(Debug, Clone)]
pub struct PlacedPart {
    pub segment: u32,
    pub part: u32,
    pub init: PathBuf,
    pub fragments: Vec<PathBuf>,
    /// Where its first frame plays, from the timeline start.
    pub start_ns: u64,
    pub length_ns: u64,
}

/// A run of stem samples that sits at one place on the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StemChunk {
    pub file_sample: u64,
    pub mix_sample: u64,
    pub sample_count: u64,
}

#[derive(Debug, Clone)]
pub struct PlacedStem {
    pub segment: u32,
    pub stem: Stem,
    pub file: PathBuf,
    /// In file order.
    pub chunks: Vec<StemChunk>,
}

#[derive(Debug, Clone)]
pub struct CallTimeline {
    /// UTC at the timeline's start: the earliest frame or sample.
    pub started_at: Option<Timestamp>,
    pub length_ns: u64,
    /// By start.
    pub parts: Vec<PlacedPart>,
    pub stems: Vec<PlacedStem>,
    pub skipped: Vec<SkippedInput>,
}

/// Reads both manifests of a call folder and places every closed or recovered segment.
///
/// # Errors
///
/// Fails when a manifest is unreadable or from a newer schema.
pub fn plan_folder(folder: &Path) -> Result<CallTimeline, FinalizeError> {
    let audio = if folder.join(stem_writer::MANIFEST_FILE_NAME).exists() {
        AudioManifest::load(folder)?
    } else {
        AudioManifest {
            schema_version: stem_writer::MANIFEST_SCHEMA_VERSION,
            segments: Vec::new(),
        }
    };
    let video = VideoManifest::load_or_new(folder)?;
    let mut skipped = Vec::new();
    let mut parts = Vec::new();
    for segment in &video.segments {
        match window_capture_parts(folder, segment) {
            Ok(segment_parts) => parts.extend(segment_parts),
            Err(reason) => skipped.push(SkippedInput {
                segment: segment.number,
                track: InputTrack::Video,
                reason,
            }),
        }
    }
    let mut timeline = plan(folder, &audio, &parts);
    timeline.skipped.splice(0..0, skipped);
    Ok(timeline)
}

/// The parts of one segment `mc-capture video` wrote.
fn window_capture_parts(
    folder: &Path,
    segment: &VideoSegmentRecord,
) -> Result<Vec<VideoPartInput>, SkipReason> {
    match segment.state {
        VideoSegmentState::Open => return Err(SkipReason::StillOpen),
        VideoSegmentState::Closed | VideoSegmentState::Recovered => {}
    }
    // A segment repair rebuilt from its files has no part records, so no host times.
    if segment.parts.is_empty() {
        return Err(if segment.fragments.is_empty() {
            SkipReason::Empty
        } else {
            SkipReason::NoClock
        });
    }
    let segment_dir = folder.join(&segment.folder);
    let parts: Vec<VideoPartInput> = segment
        .parts
        .iter()
        .filter_map(|part| {
            let fragments: Vec<_> = segment
                .fragments
                .iter()
                .filter(|fragment| fragment.part == part.number)
                .collect();
            (!fragments.is_empty()).then(|| VideoPartInput {
                segment: segment.number,
                part: part.number,
                audio_segment: segment.audio_segment,
                start: PartStart::HostTime {
                    first_host_time_ns: part.first_host_time_ns,
                    own_clock: segment.clock,
                },
                init: segment_dir.join(&part.init),
                fragments: fragments
                    .iter()
                    .map(|fragment| segment_dir.join(&fragment.file))
                    .collect(),
                length_ms: fragments.iter().map(|fragment| fragment.duration_ms).sum(),
            })
        })
        .collect();
    if parts.is_empty() {
        Err(SkipReason::Empty)
    } else {
        Ok(parts)
    }
}

/// A clock reading as nanoseconds: host time and UTC at one instant.
#[derive(Debug, Clone, Copy)]
struct ClockReading {
    host_time_ns: u64,
    utc_ns: i128,
}

impl ClockReading {
    fn utc_ns_at(self, host_time_ns: u64) -> i128 {
        self.utc_ns + i128::from(host_time_ns) - i128::from(self.host_time_ns)
    }

    fn utc_minus_host_ns(self) -> i128 {
        self.utc_ns - i128::from(self.host_time_ns)
    }
}

/// The first reading of each run of segments recorded within one boot with no sleep between.
/// Each reading has its own error and the clocks drift apart slowly, so placing a run's
/// segments by one reading keeps their host time spacing exact.
#[derive(Default)]
struct ClockRuns {
    firsts: Vec<ClockReading>,
}

impl ClockRuns {
    fn run_reading(&mut self, reading: ClockReading) -> ClockReading {
        let offset_ns = reading.utc_minus_host_ns();
        if let Some(first) = self
            .firsts
            .iter()
            .find(|first| (first.utc_minus_host_ns() - offset_ns).abs() <= SAME_CLOCK_RUN_NS)
        {
            return *first;
        }
        self.firsts.push(reading);
        reading
    }
}

impl From<ClockRecord> for ClockReading {
    fn from(clock: ClockRecord) -> Self {
        Self {
            host_time_ns: clock.host_time_ns,
            utc_ns: clock.utc.as_nanosecond(),
        }
    }
}

impl From<stem_writer::ClockRecord> for ClockReading {
    fn from(clock: stem_writer::ClockRecord) -> Self {
        Self {
            host_time_ns: clock.host_time_ns,
            utc_ns: clock.utc.as_nanosecond(),
        }
    }
}

/// A chunk before the timeline start is known: its UTC start.
struct UtcChunk {
    file_sample: u64,
    utc_ns: i128,
    sample_count: u64,
}

/// Places the parts and every closed or recovered stem of `audio`.
#[must_use]
pub fn plan(folder: &Path, audio: &AudioManifest, parts: &[VideoPartInput]) -> CallTimeline {
    let mut skipped = Vec::new();
    let mut clock_runs = ClockRuns::default();
    let mut audio_segments: Vec<_> = audio
        .segments
        .iter()
        .filter(|segment| {
            matches!(
                segment.state,
                SegmentState::Closed | SegmentState::Recovered
            )
        })
        .collect();
    audio_segments.sort_by_key(|segment| segment.number);
    let audio_clocks: BTreeMap<u32, ClockReading> = audio_segments
        .into_iter()
        .filter_map(|segment| {
            let reading = clock_runs.run_reading(ClockReading::from(segment.clock?));
            Some((segment.number, reading))
        })
        .collect();

    let (part_starts, remote_trails_ns) =
        start_parts(parts, &audio_clocks, &mut clock_runs, &mut skipped);
    let utc_stems = place_stems_in_utc(audio, &audio_clocks, &remote_trails_ns, &mut skipped);

    let origin_ns = part_starts
        .iter()
        .map(|(_, start)| *start)
        .chain(
            utc_stems
                .iter()
                .filter_map(|(_, _, chunks)| chunks.first().map(|chunk| chunk.utc_ns)),
        )
        .min();
    let Some(origin_ns) = origin_ns else {
        return CallTimeline {
            started_at: None,
            length_ns: 0,
            parts: Vec::new(),
            stems: Vec::new(),
            skipped,
        };
    };

    let mut length_ns = 0_u64;
    let mut placed_parts: Vec<PlacedPart> = part_starts
        .into_iter()
        .map(|(part, start_utc_ns)| {
            let start_ns = nonnegative_ns(start_utc_ns - origin_ns);
            let part_length_ns = nonnegative_ns(ms_to_ns(part.length_ms));
            length_ns = length_ns.max(start_ns + part_length_ns);
            PlacedPart {
                segment: part.segment,
                part: part.part,
                init: part.init.clone(),
                fragments: part.fragments.clone(),
                start_ns,
                length_ns: part_length_ns,
            }
        })
        .collect();
    placed_parts.sort_by_key(|part| part.start_ns);

    let stems = utc_stems
        .into_iter()
        .map(|(segment, stem, chunks)| {
            let chunks = mix_chunks(&chunks, origin_ns);
            if let Some(last) = chunks.last() {
                length_ns = length_ns.max(mix_samples_to_ns(last.mix_sample + last.sample_count));
            }
            PlacedStem {
                segment,
                stem: stem.stem,
                file: folder.join(&stem.file),
                chunks,
            }
        })
        .collect();

    CallTimeline {
        started_at: Timestamp::from_nanosecond(origin_ns).ok(),
        length_ns,
        parts: placed_parts,
        stems,
        skipped,
    }
}

/// Each part's first frame in UTC nanoseconds, and each audio segment's remote trail from its
/// browser parts.
fn start_parts<'a>(
    parts: &'a [VideoPartInput],
    audio_clocks: &BTreeMap<u32, ClockReading>,
    clock_runs: &mut ClockRuns,
    skipped: &mut Vec<SkippedInput>,
) -> (Vec<(&'a VideoPartInput, i128)>, BTreeMap<u32, i128>) {
    // Several browser parts may pair with one audio segment; the latest logged trail wins, as
    // the playback path only grows its buffer.
    let mut remote_trails_ns: BTreeMap<u32, i128> = BTreeMap::new();
    let mut part_starts: Vec<(&VideoPartInput, i128)> = Vec::new();
    for part in parts {
        let start_utc_ns = match part.start {
            PartStart::HostTime {
                first_host_time_ns,
                own_clock,
            } => {
                let paired = part
                    .audio_segment
                    .and_then(|number| audio_clocks.get(&number).copied());
                let own = own_clock.map(|clock| clock_runs.run_reading(clock.into()));
                let Some(clock) = paired.or(own) else {
                    skipped.push(SkippedInput {
                        segment: part.segment,
                        track: InputTrack::Video,
                        reason: SkipReason::NoClock,
                    });
                    continue;
                };
                clock.utc_ns_at(first_host_time_ns)
            }
            PartStart::BrowserUtc {
                first_frame_at,
                remote_trail_ms,
            } => {
                if let Some(number) = part.audio_segment {
                    remote_trails_ns.insert(number, ms_to_ns(remote_trail_ms));
                }
                first_frame_at.as_nanosecond()
            }
        };
        part_starts.push((part, start_utc_ns));
    }

    (part_starts, remote_trails_ns)
}

/// Each closed or recovered stem's chunks in UTC nanoseconds.
fn place_stems_in_utc<'a>(
    audio: &'a AudioManifest,
    audio_clocks: &BTreeMap<u32, ClockReading>,
    remote_trails_ns: &BTreeMap<u32, i128>,
    skipped: &mut Vec<SkippedInput>,
) -> Vec<(u32, &'a StemRecord, Vec<UtcChunk>)> {
    let mut utc_stems: Vec<(u32, &StemRecord, Vec<UtcChunk>)> = Vec::new();
    for segment in &audio.segments {
        for stem in &segment.stems {
            let mut skip = |reason| {
                skipped.push(SkippedInput {
                    segment: segment.number,
                    track: stem.stem.into(),
                    reason,
                });
            };
            match segment.state {
                SegmentState::Open => {
                    skip(SkipReason::StillOpen);
                    continue;
                }
                SegmentState::Closed | SegmentState::Recovered => {}
            }
            let Some(clock) = audio_clocks.get(&segment.number).copied() else {
                skip(SkipReason::NoClock);
                continue;
            };
            if stem.first_host_time_ns.is_none() || stem.samples_written == 0 {
                skip(SkipReason::Empty);
                continue;
            }
            let shift_ns = match stem.stem {
                Stem::Remote => remote_trails_ns.get(&segment.number).copied().unwrap_or(0),
                Stem::Mic => 0,
            };
            let chunks = utc_chunks(stem, clock, shift_ns);
            utc_stems.push((segment.number, stem, chunks));
        }
    }
    utc_stems
}

/// Places a stem's chunks on the mix, in file order. Where a run ahead starts, the file holds
/// audio twice for one span of host time: the later copy loses its head, so nothing plays
/// doubled. A steering hole too short to be a loss is filled from the file.
fn mix_chunks(chunks: &[UtcChunk], origin_ns: i128) -> Vec<StemChunk> {
    let mut placed: Vec<StemChunk> = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let mut mix_chunk = StemChunk {
            file_sample: chunk.file_sample,
            mix_sample: ns_to_mix_samples(chunk.utc_ns - origin_ns),
            sample_count: chunk.sample_count,
        };
        if let Some(last) = placed.last() {
            let last_end = last.mix_sample + last.sample_count;
            if mix_chunk.mix_sample < last_end {
                let overlap = last_end - mix_chunk.mix_sample;
                if overlap >= mix_chunk.sample_count {
                    continue;
                }
                mix_chunk.file_sample += overlap;
                mix_chunk.mix_sample = last_end;
                mix_chunk.sample_count -= overlap;
            } else {
                let hole = mix_chunk.mix_sample - last_end;
                if hole <= MAX_FILLED_HOLE_SAMPLES && hole <= mix_chunk.file_sample {
                    mix_chunk.file_sample -= hole;
                    mix_chunk.mix_sample = last_end;
                    mix_chunk.sample_count += hole;
                }
            }
        }
        placed.push(mix_chunk);
    }
    placed
}

/// Cuts the stem into chunks, each at the UTC of its own first sample: 1 s chunks where file
/// and host time move together, 10 ms ones inside a run ahead. Cuts fall on each run's start
/// and end, where the file jumps against host time.
fn utc_chunks(stem: &StemRecord, clock: ClockReading, shift_ns: i128) -> Vec<UtcChunk> {
    let to_sample = |ms: f64| -> u64 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to the file's sample range first"
        )]
        let sample = (ms.max(0.0) * f64::from(MIX_RATE_HZ) / 1e3).round() as u64;
        sample.min(stem.samples_written)
    };
    let steered: Vec<(u64, u64)> = stem
        .runs_ahead
        .iter()
        .map(|span| {
            let end = span.end_ms.map_or(stem.samples_written, to_sample);
            (to_sample(span.start_ms), end)
        })
        .collect();
    let mut cuts: Vec<u64> = steered
        .iter()
        .flat_map(|(start, end)| [*start, *end])
        .chain([stem.samples_written])
        .collect();
    cuts.sort_unstable();
    cuts.dedup();

    let mut chunks = Vec::new();
    let mut file_sample = 0_u64;
    while file_sample < stem.samples_written {
        let next_cut = cuts
            .iter()
            .copied()
            .find(|cut| *cut > file_sample)
            .unwrap_or(stem.samples_written);
        let is_steered = steered
            .iter()
            .any(|(start, end)| (*start..*end).contains(&file_sample));
        let chunk_length = if is_steered {
            STEERED_CHUNK_SAMPLES
        } else {
            STEM_CHUNK_SAMPLES
        };
        let sample_count = chunk_length.min(next_cut - file_sample);
        #[expect(
            clippy::cast_precision_loss,
            reason = "sample counts stay far below 2^52"
        )]
        let file_ms = file_sample as f64 * 1e3 / f64::from(MIX_RATE_HZ);
        if let Some(host_time_ns) = stem.host_time_at(file_ms) {
            chunks.push(UtcChunk {
                file_sample,
                utc_ns: clock.utc_ns_at(host_time_ns) - shift_ns,
                sample_count,
            });
        }
        file_sample += sample_count;
    }
    chunks
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "latencies are milliseconds, far inside i128 nanoseconds"
)]
fn ms_to_ns(ms: f64) -> i128 {
    (ms * 1e6).round() as i128
}

fn nonnegative_ns(ns: i128) -> u64 {
    u64::try_from(ns.max(0)).unwrap_or(u64::MAX)
}

/// The mix sample nearest a timeline position.
fn ns_to_mix_samples(ns: i128) -> u64 {
    let rate = i128::from(MIX_RATE_HZ);
    let samples = (ns.max(0) * rate + NANOS_PER_SECOND / 2) / NANOS_PER_SECOND;
    u64::try_from(samples).unwrap_or(u64::MAX)
}

#[must_use]
pub fn mix_samples_to_ns(samples: u64) -> u64 {
    let ns = u128::from(samples) * 1_000_000_000 / u128::from(MIX_RATE_HZ);
    u64::try_from(ns).unwrap_or(u64::MAX)
}
