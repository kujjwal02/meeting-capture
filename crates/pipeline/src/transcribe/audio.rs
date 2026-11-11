//! The call's stems as 100 ms frames on the call's clock: which frames hold speech, the
//! 5-minute windows the call is cut into, the loudest voiced clips, and the trimmed audio of a
//! window or clip with the map that puts its times back on the call's clock.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use call_finalize::{CallTimeline, PlacedStem};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use stem_timing::Stem;

use super::TranscribeError;

const RATE_HZ: u64 = stem_writer::OUTPUT_RATE_HZ as u64;
/// 100 ms.
const FRAME_SAMPLES: u64 = RATE_HZ / 10;
const FRAMES_PER_S: f64 = 10.0;

/// A frame louder than this holds speech. Room noise and a quiet line sit well below it.
const VOICE_GATE_DBFS: f32 = -40.0;
/// Frames kept on each side of speech, so soft word starts and ends stay in the audio sent.
const HANG_FRAMES: usize = 3;
/// A silence up to this long stays in the audio sent; a longer one is cut out, since the
/// provider bills every second.
const MAX_KEPT_SILENCE_FRAMES: usize = 10;

/// Two frames further apart on the call's clock than in the file start a new piece.
const MAX_CLOCK_SLIP_S: f64 = 0.01;

/// The length of the windows the call is cut into. Each window and stem is one request.
pub(super) const WINDOW_S: f64 = 300.0;
/// The length of each clip the language check sends.
const CLIP_FRAMES: usize = 300;
/// Clip candidates start this many frames apart.
const CLIP_STEP_FRAMES: usize = 50;
/// A clip candidate needs at least this share of speech: a silent clip gives no signal.
const MIN_CLIP_VOICED_SHARE: f64 = 0.5;
/// Clips sent per stem.
const CLIPS_PER_STEM: usize = 2;

/// One 100 ms frame of a stem file.
struct Frame {
    file_sample: u64,
    /// Seconds from the call's start, the clock the captions use.
    call_s: f64,
    is_voiced: bool,
    /// Speech, or a silence short enough to stay in the audio sent.
    is_kept: bool,
    loudness_dbfs: f32,
}

/// One stem file, framed.
pub(super) struct StemAudio {
    pub stem: Stem,
    pub file: PathBuf,
    /// The file's size when it was read, which a saved reply records.
    pub file_bytes: u64,
    frames: Vec<Frame>,
}

/// Every stem file of the call, framed.
pub(super) struct CallAudio {
    pub stems: Vec<StemAudio>,
}

/// A stretch of call time, in seconds from the call's start.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct CallSpan {
    pub from_s: f64,
    pub to_s: f64,
}

impl CallSpan {
    /// Window `index` of the call. The first reaches back to `audio_start_s` when the audio
    /// starts before the captions' second 0, as when the recorder heard the call before the
    /// call log began, so that speech falls in a window too.
    pub(super) fn window(index: usize, audio_start_s: f64) -> Self {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a call has far fewer than 2^52 windows"
        )]
        let from_s = index as f64 * WINDOW_S;
        Self {
            from_s: if index == 0 {
                audio_start_s.min(0.0)
            } else {
                from_s
            },
            to_s: from_s + WINDOW_S,
        }
    }

    pub(super) fn contains(self, call_s: f64) -> bool {
        call_s >= self.from_s && call_s < self.to_s
    }

    pub(super) fn overlaps(self, other: Self) -> bool {
        self.from_s < other.to_s && other.from_s < self.to_s
    }
}

/// One contiguous run of samples of one stem file, and where it starts on the call's clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct CutPiece {
    pub file_name: String,
    pub file_sample: u64,
    pub sample_count: u64,
    pub call_s: f64,
}

impl CutPiece {
    #[expect(
        clippy::cast_precision_loss,
        reason = "sample counts stay far below 2^52"
    )]
    fn end_call_s(&self) -> f64 {
        self.call_s + self.sample_count as f64 / RATE_HZ as f64
    }
}

/// Trimmed audio of one stem kind, as the pieces sent one after another. A saved reply keeps
/// it, so its times map back to the call's clock with no audio read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct AudioCut {
    pub pieces: Vec<CutPiece>,
    /// Each stem file's size when the cut was made. A saved reply whose file changed since is
    /// out of date.
    pub file_bytes: Vec<(String, u64)>,
}

impl AudioCut {
    pub(super) fn sample_count(&self) -> u64 {
        self.pieces.iter().map(|piece| piece.sample_count).sum()
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "sample counts stay far below 2^52"
    )]
    pub(super) fn seconds(&self) -> f64 {
        self.sample_count() as f64 / RATE_HZ as f64
    }

    /// The pieces in order, with the samples of `other` after its own.
    pub(super) fn join(mut self, other: Self) -> Self {
        self.pieces.extend(other.pieces);
        for entry in other.file_bytes {
            if !self.file_bytes.contains(&entry) {
                self.file_bytes.push(entry);
            }
        }
        self
    }

    /// The call time of `sent_s` seconds into the audio sent.
    #[expect(
        clippy::cast_precision_loss,
        reason = "sample counts stay far below 2^52"
    )]
    pub(super) fn call_s(&self, sent_s: f64) -> f64 {
        let rate = RATE_HZ as f64;
        let mut piece_start_s = 0.0;
        let mut last_call_end_s = 0.0;
        for piece in &self.pieces {
            let piece_s = piece.sample_count as f64 / rate;
            if sent_s < piece_start_s + piece_s {
                return piece.call_s + (sent_s - piece_start_s).max(0.0);
            }
            piece_start_s += piece_s;
            last_call_end_s = piece.call_s + piece_s;
        }
        last_call_end_s + (sent_s - piece_start_s).max(0.0)
    }

    /// Whether the stem files still have the sizes they had when this cut was made. A cheap
    /// first test: a file rewritten to the same size passes it, so callers compare
    /// `samples_sha256` too.
    pub(super) fn has_current_sizes(&self, folder: &Path) -> bool {
        self.file_bytes.iter().all(|(file_name, bytes)| {
            std::fs::metadata(folder.join(file_name)).is_ok_and(|metadata| metadata.len() == *bytes)
        })
    }

    /// The samples of every piece, one after another.
    pub(super) fn samples(&self, folder: &Path) -> Result<Vec<i16>, TranscribeError> {
        let mut samples = Vec::new();
        for piece in &self.pieces {
            samples.extend(stt::ogg_opus::read_stem_samples(
                &folder.join(&piece.file_name),
                piece.file_sample,
                piece.sample_count,
            )?);
        }
        Ok(samples)
    }
}

impl CallAudio {
    /// Frames every placed stem with samples. `call_offset_s` is the call time of the
    /// timeline's start.
    pub(super) fn read(
        timeline: &CallTimeline,
        call_offset_s: f64,
        min_stem_samples: u64,
    ) -> Result<Self, TranscribeError> {
        let mut stems = Vec::new();
        for placed in &timeline.stems {
            if file_extent(placed) < min_stem_samples {
                continue;
            }
            stems.push(read_stem(placed, call_offset_s)?);
        }
        Ok(Self { stems })
    }

    /// Seconds from the call's start to its first frame. Negative when the audio starts before
    /// the captions.
    pub(super) fn start_s(&self) -> f64 {
        self.stems
            .iter()
            .filter_map(|stem| stem.frames.first())
            .map(|frame| frame.call_s)
            .fold(0.0, f64::min)
    }

    /// Seconds from the call's start to the end of its last frame.
    pub(super) fn end_s(&self) -> f64 {
        self.stems
            .iter()
            .filter_map(|stem| stem.frames.last())
            .map(|frame| frame.call_s + 1.0 / FRAMES_PER_S)
            .fold(0.0, f64::max)
    }

    /// Minutes of `span` in which either stem holds speech.
    pub(super) fn voiced_minutes(&self, span: CallSpan) -> f64 {
        let slots: BTreeSet<i64> = self
            .stems
            .iter()
            .flat_map(|stem| &stem.frames)
            .filter(|frame| frame.is_voiced && span.contains(frame.call_s))
            .map(|frame| {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a call's frame count fits in i64"
                )]
                let slot = (frame.call_s * FRAMES_PER_S).floor() as i64;
                slot
            })
            .collect();
        #[expect(
            clippy::cast_precision_loss,
            reason = "a call has far fewer than 2^52 frames"
        )]
        let minutes = slots.len() as f64 / FRAMES_PER_S / 60.0;
        minutes
    }

    /// The kept frames of `stem` inside `span` and outside `excluded`, as pieces.
    pub(super) fn cut(&self, stem: Stem, span: CallSpan, excluded: &[CallSpan]) -> AudioCut {
        let mut cut = AudioCut::default();
        for stem_audio in self
            .stems
            .iter()
            .filter(|stem_audio| stem_audio.stem == stem)
        {
            let file_name = stem_audio.file_name();
            let extent = stem_audio
                .frames
                .last()
                .map_or(0, |frame| frame.file_sample + FRAME_SAMPLES);
            let mut is_used = false;
            for frame in &stem_audio.frames {
                let is_sent = frame.is_kept
                    && span.contains(frame.call_s)
                    && !excluded
                        .iter()
                        .any(|excluded| excluded.contains(frame.call_s));
                if !is_sent {
                    continue;
                }
                is_used = true;
                let sample_count = FRAME_SAMPLES.min(extent - frame.file_sample);
                match cut.pieces.last_mut() {
                    // A chunk boundary inside the file can jump on the call's clock, and a piece
                    // maps its times with one offset.
                    Some(piece)
                        if piece.file_name == file_name
                            && piece.file_sample + piece.sample_count == frame.file_sample
                            && (piece.end_call_s() - frame.call_s).abs() < MAX_CLOCK_SLIP_S =>
                    {
                        piece.sample_count += sample_count;
                    }
                    Some(_) | None => cut.pieces.push(CutPiece {
                        file_name: file_name.clone(),
                        file_sample: frame.file_sample,
                        sample_count,
                        call_s: frame.call_s,
                    }),
                }
            }
            if is_used {
                cut.file_bytes.push((file_name, stem_audio.file_bytes));
            }
        }
        cut
    }

    /// Up to two 30 s stretches of `stem` with the most and loudest speech, apart from each
    /// other, as call time.
    pub(super) fn loudest_clips(&self, stem: Stem) -> Vec<CallSpan> {
        let mut candidates: Vec<(f64, CallSpan)> = Vec::new();
        for stem_audio in self
            .stems
            .iter()
            .filter(|stem_audio| stem_audio.stem == stem)
        {
            let frames = &stem_audio.frames;
            let mut start = 0;
            while start + CLIP_FRAMES <= frames.len() {
                let clip = &frames[start..start + CLIP_FRAMES];
                let voiced: Vec<f32> = clip
                    .iter()
                    .filter(|frame| frame.is_voiced)
                    .map(|frame| frame.loudness_dbfs)
                    .collect();
                #[expect(clippy::cast_precision_loss, reason = "a clip has 300 frames")]
                let voiced_share = voiced.len() as f64 / CLIP_FRAMES as f64;
                if voiced_share >= MIN_CLIP_VOICED_SHARE {
                    #[expect(clippy::cast_precision_loss, reason = "a clip has 300 frames")]
                    let mean_dbfs = f64::from(voiced.iter().sum::<f32>()) / voiced.len() as f64;
                    let span = CallSpan {
                        from_s: clip[0].call_s,
                        to_s: clip[CLIP_FRAMES - 1].call_s + 1.0 / FRAMES_PER_S,
                    };
                    candidates.push((mean_dbfs, span));
                }
                start += CLIP_STEP_FRAMES;
            }
        }
        candidates.sort_by(|first, second| second.0.total_cmp(&first.0));
        let mut chosen: Vec<CallSpan> = Vec::new();
        for (_, span) in candidates {
            if chosen.len() == CLIPS_PER_STEM {
                break;
            }
            if chosen.iter().all(|other| !other.overlaps(span)) {
                chosen.push(span);
            }
        }
        chosen.sort_by(|first, second| first.from_s.total_cmp(&second.from_s));
        chosen
    }
}

impl StemAudio {
    pub(super) fn file_name(&self) -> String {
        self.file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// SHA-256 of `samples` as little-endian bytes, in hex: what a saved reply records of the audio
/// it answers.
pub(super) fn samples_sha256(samples: &[i16]) -> String {
    let mut hasher = Sha256::new();
    for sample in samples {
        hasher.update(sample.to_le_bytes());
    }
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Samples from the file's start to the end of its last placed chunk.
fn file_extent(placed: &PlacedStem) -> u64 {
    placed
        .chunks
        .iter()
        .map(|chunk| chunk.file_sample + chunk.sample_count)
        .max()
        .unwrap_or(0)
}

fn read_stem(placed: &PlacedStem, call_offset_s: f64) -> Result<StemAudio, TranscribeError> {
    let saved_error = |source| TranscribeError::Saved {
        path: placed.file.clone(),
        source,
    };
    let file_bytes = std::fs::metadata(&placed.file).map_err(saved_error)?.len();
    let loudness = stt::ogg_opus::frame_loudness_dbfs(
        &placed.file,
        usize::try_from(FRAME_SAMPLES).unwrap_or(usize::MAX),
        file_extent(placed),
    )?;
    let mut frames: Vec<Frame> = loudness
        .into_iter()
        .enumerate()
        .map(|(index, loudness_dbfs)| {
            let file_sample = index as u64 * FRAME_SAMPLES;
            #[expect(
                clippy::cast_precision_loss,
                reason = "sample counts stay far below 2^52"
            )]
            let file_s = file_sample as f64 / RATE_HZ as f64;
            Frame {
                file_sample,
                call_s: call_offset_s + timeline_s(placed, file_s),
                is_voiced: loudness_dbfs > VOICE_GATE_DBFS,
                is_kept: false,
                loudness_dbfs,
            }
        })
        .collect();
    mark_kept(&mut frames);
    Ok(StemAudio {
        stem: placed.stem,
        file: placed.file.clone(),
        file_bytes,
        frames,
    })
}

/// Keeps speech, `HANG_FRAMES` around it, and silences up to `MAX_KEPT_SILENCE_FRAMES`.
fn mark_kept(frames: &mut [Frame]) {
    let voiced: Vec<bool> = frames.iter().map(|frame| frame.is_voiced).collect();
    for (index, frame) in frames.iter_mut().enumerate() {
        let from = index.saturating_sub(HANG_FRAMES);
        let to = (index + HANG_FRAMES + 1).min(voiced.len());
        frame.is_kept = voiced[from..to].iter().any(|is_voiced| *is_voiced);
    }
    // A short silence between kept frames stays, so a pause inside a sentence is not cut.
    let mut index = 0;
    while index < frames.len() {
        if frames[index].is_kept {
            index += 1;
            continue;
        }
        let gap_start = index;
        while index < frames.len() && !frames[index].is_kept {
            index += 1;
        }
        let is_inner = gap_start > 0 && index < frames.len();
        if is_inner && index - gap_start <= MAX_KEPT_SILENCE_FRAMES {
            for frame in &mut frames[gap_start..index] {
                frame.is_kept = true;
            }
        }
    }
}

/// Seconds from the timeline start for a time in the stem's file: the chunk holding that file
/// sample, else the last chunk before it.
#[expect(
    clippy::cast_precision_loss,
    reason = "sample counts stay far below 2^52, where f64 stops being exact"
)]
fn timeline_s(stem: &PlacedStem, file_s: f64) -> f64 {
    let rate = f64::from(call_finalize::plan::MIX_RATE_HZ);
    let file_sample = file_s * rate;
    let chunk = stem
        .chunks
        .iter()
        .take_while(|chunk| chunk.file_sample as f64 <= file_sample)
        .last()
        .or_else(|| stem.chunks.first());
    match chunk {
        Some(chunk) => {
            (chunk.mix_sample as f64 + (file_sample - chunk.file_sample as f64).max(0.0)) / rate
        }
        None => file_s,
    }
}
