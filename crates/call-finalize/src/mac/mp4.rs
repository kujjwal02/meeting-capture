//! Writes `call.mp4` with `AVAssetWriter`. The video passes through as VideoToolbox encoded it,
//! each frame moved to its place on the call timeline. The stem mix goes in as 16-bit PCM and
//! AVFoundation encodes it to AAC.
//!
//! `AVAssetReader` cannot read an HLS playlist, so each part's init segment and fragments are
//! joined into one fragmented MP4 in the call folder while the part is read, then deleted.
//!
//! Unsafe code: `CoreMedia` builds sample buffers from raw timing and size arrays, and a track the
//! asset loads on a framework queue moves to this thread.
#![allow(unsafe_code)]

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cidre::{arc, av, cat, cf, cm, ns, os};

use super::transcode::{TrackFormat, Transcoder, decoded_frame_settings};
use crate::mix::StemMix;
use crate::plan::{CallTimeline, MIX_RATE_HZ, PlacedPart, mix_samples_to_ns};
use crate::{
    FinalizeError, JOINED_PART_PREFIX, MIX_BIT_RATE_BPS, PARTIAL_FILE_NAME, remove_if_present,
};

/// Fine enough that moving a frame by any nanosecond offset rounds by under 12 µs.
const VIDEO_TICKS_PER_SECOND: i32 = 90_000;
/// Half a second of mix per sample buffer.
const MIX_BLOCK_SAMPLES: usize = MIX_RATE_HZ as usize / 2;
const TRACK_LOAD_LIMIT: Duration = Duration::from_secs(10);
/// Finishing writes the movie header after the last sample, under a second for an hour of call.
const FINISH_LIMIT: Duration = Duration::from_secs(600);
const INPUT_WAIT: Duration = Duration::from_millis(2);
/// The writer takes a frame or a mix block every few milliseconds. This long with neither means
/// it hangs.
const STALL_LIMIT: Duration = Duration::from_secs(60);
/// The capture's frame period, for a frame whose duration the file does not state.
const FALLBACK_FRAME_NS: u64 = 1_000_000_000 / 15;

pub(crate) struct WrittenTracks {
    pub video_frame_count: u64,
    pub parts_shifted: u64,
    pub parts_reencoded: u64,
    /// The file's length: the timeline's, or longer when a shifted part ends past it.
    pub length_ns: u64,
}

/// Writes the timeline to a partial file beside `output`, then renames it into place.
pub(crate) fn write_call_mp4(
    folder: &Path,
    output: &Path,
    timeline: &CallTimeline,
) -> Result<WrittenTracks, FinalizeError> {
    let partial = folder.join(PARTIAL_FILE_NAME);
    let mp4_error = |message: String| FinalizeError::Mp4Write {
        path: output.to_owned(),
        message,
    };
    remove_if_present(&partial)?;
    let mut video = VideoFrames::new(folder, &timeline.parts);
    video.advance()?;
    let Some(first_format) = video
        .pending
        .as_ref()
        .and_then(|(sample, _)| sample.format_desc().map(cm::FormatDesc::retained))
    else {
        return Err(mp4_error("no video part holds a frame".to_owned()));
    };
    let mut mix = StemMix::open(&timeline.stems)?;
    let pcm_format = pcm_format().map_err(|status| mp4_error(format!("PCM format: {status:?}")))?;

    let OpenWriter {
        writer,
        mut video_input,
        mut audio_input,
    } = open_writer(&partial, &first_format).map_err(mp4_error)?;
    let mut writer = CancelUnfinished(writer);
    writer.start_session_at_src_time(cm::Time::zero());

    let mix_end_sample = timeline
        .length_ns
        .div_ceil(1_000_000_000 / u64::from(MIX_RATE_HZ));
    let mut mix_sample = 0_u64;
    let mut block = vec![0_i16; MIX_BLOCK_SAMPLES];
    let mut is_video_done = false;
    let mut is_audio_done = false;
    let mut appended_at = Instant::now();
    while !(is_video_done && is_audio_done) {
        if !is_video_done && video.pending.is_none() {
            video_input.mark_as_finished();
            is_video_done = true;
        }
        if !is_audio_done && mix_sample >= mix_end_sample {
            audio_input.mark_as_finished();
            is_audio_done = true;
        }
        let video_next_ns = video.pending.as_ref().map(|(_, pts_ns)| *pts_ns);
        let audio_next_ns = (!is_audio_done).then(|| mix_samples_to_ns(mix_sample));
        // The earlier track goes first; the writer holds one back while the other lags, so the
        // later one goes when the earlier is not ready.
        let is_video_first = match (video_next_ns, audio_next_ns) {
            (Some(video_ns), Some(audio_ns)) => video_ns <= audio_ns,
            (Some(_), None) => true,
            (None, _) => false,
        };
        let mut is_appended = false;
        for is_video in [is_video_first, !is_video_first] {
            if is_video && video_next_ns.is_some() && video_input.is_ready_for_more_media_data() {
                if let Some((sample, _)) = video.pending.take() {
                    append(&mut video_input, &sample, &writer).map_err(mp4_error)?;
                    video.frame_count += 1;
                }
                video.advance()?;
                is_appended = true;
                break;
            }
            if !is_video && audio_next_ns.is_some() && audio_input.is_ready_for_more_media_data() {
                let count = usize::try_from(mix_end_sample - mix_sample)
                    .unwrap_or(MIX_BLOCK_SAMPLES)
                    .min(MIX_BLOCK_SAMPLES);
                mix.read_block(mix_sample, &mut block[..count])?;
                let sample = pcm_sample(&pcm_format, &block[..count], mix_sample)
                    .map_err(|message| mp4_error(format!("PCM sample buffer: {message}")))?;
                append(&mut audio_input, &sample, &writer).map_err(mp4_error)?;
                mix_sample += count as u64;
                is_appended = true;
                break;
            }
        }
        if is_appended {
            appended_at = Instant::now();
        } else if !is_video_done || !is_audio_done {
            if writer.status() == av::AssetWriterStatus::Failed {
                return Err(mp4_error(writer_error(&writer)));
            }
            if appended_at.elapsed() >= STALL_LIMIT {
                return Err(mp4_error(format!(
                    "the writer took nothing for {} s",
                    STALL_LIMIT.as_secs()
                )));
            }
            std::thread::sleep(INPUT_WAIT);
        }
    }
    // A shifted part may end past the timeline.
    let end_ns = timeline.length_ns.max(video.end_ns);
    close_writer(&mut writer, folder, &partial, output, end_ns)?;
    Ok(WrittenTracks {
        video_frame_count: video.frame_count,
        parts_shifted: video.parts_shifted,
        parts_reencoded: video.parts_reencoded,
        length_ns: end_ns,
    })
}

/// A writer that cancels itself when dropped mid-write, so an error leaves no writer holding
/// the partial file open.
struct CancelUnfinished(arc::R<av::AssetWriter>);

impl std::ops::Deref for CancelUnfinished {
    type Target = av::AssetWriter;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for CancelUnfinished {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for CancelUnfinished {
    fn drop(&mut self) {
        // AVFoundation raises on a cancel once the writer has finished or failed.
        if self.0.status() == av::AssetWriterStatus::Writing {
            self.0.cancel_writing();
        }
    }
}

/// Ends the session at `end_ns`, finishes the file and renames it into place.
fn close_writer(
    writer: &mut av::AssetWriter,
    folder: &Path,
    partial: &Path,
    output: &Path,
    end_ns: u64,
) -> Result<(), FinalizeError> {
    let mp4_error = |message: String| FinalizeError::Mp4Write {
        path: output.to_owned(),
        message,
    };
    writer
        .end_session_at_src_time(cm::Time::new(
            i64::try_from(end_ns).unwrap_or(i64::MAX),
            1_000_000_000,
        ))
        .map_err(|exception| mp4_error(exception_reason(exception)))?;
    finish(writer).map_err(mp4_error)?;
    std::fs::rename(partial, output).map_err(|source| FinalizeError::Io {
        path: output.to_owned(),
        source,
    })?;
    File::open(folder)
        .and_then(|handle| handle.sync_all())
        .map_err(|source| FinalizeError::Io {
            path: folder.to_owned(),
            source,
        })?;
    Ok(())
}

/// An MP4 writer at `partial` with a passthrough video input and an AAC audio input, started.
fn open_writer(partial: &Path, first_format: &cm::FormatDesc) -> Result<OpenWriter, String> {
    let url = ns::Url::with_fs_path_str(&partial.to_string_lossy(), false);
    let mut writer = av::AssetWriter::with_url_and_file_type(&url, av::FileType::mp4())
        .map_err(|error| error.localized_desc().to_string())?;
    let mut video_input = av::AssetWriterInput::with_media_type_output_settings_source_format_hint(
        av::MediaType::video(),
        None,
        Some(first_format),
    )
    .map_err(exception_reason)?;
    video_input.set_expects_media_data_in_real_time(false);
    let mut audio_input = av::AssetWriterInput::with_media_type_and_output_settings(
        av::MediaType::audio(),
        Some(&aac_settings()),
    )
    .map_err(exception_reason)?;
    audio_input.set_expects_media_data_in_real_time(false);
    writer.add_input(&video_input).map_err(exception_reason)?;
    writer.add_input(&audio_input).map_err(exception_reason)?;
    if !writer.start_writing() {
        return Err(writer_error(&writer));
    }
    Ok(OpenWriter {
        writer,
        video_input,
        audio_input,
    })
}

struct OpenWriter {
    writer: arc::R<av::AssetWriter>,
    video_input: arc::R<av::AssetWriterInput>,
    audio_input: arc::R<av::AssetWriterInput>,
}

/// Every frame of every part in timeline order, each moved to its timeline time.
struct VideoFrames<'a> {
    folder: &'a Path,
    parts: &'a [PlacedPart],
    next_part: usize,
    reader: Option<PartReader>,
    /// The next frame and its timeline time in nanoseconds.
    pending: Option<(arc::R<cm::SampleBuf>, u64)>,
    last_pts_ns: Option<u64>,
    /// Where the last frame stops showing.
    end_ns: u64,
    frame_count: u64,
    parts_shifted: u64,
    parts_reencoded: u64,
    /// The first part's codec and size, which the rest must match or be encoded into.
    track_format: Option<TrackFormat>,
}

impl<'a> VideoFrames<'a> {
    fn new(folder: &'a Path, parts: &'a [PlacedPart]) -> Self {
        Self {
            folder,
            parts,
            next_part: 0,
            reader: None,
            pending: None,
            last_pts_ns: None,
            end_ns: 0,
            frame_count: 0,
            parts_shifted: 0,
            parts_reencoded: 0,
            track_format: None,
        }
    }

    /// Loads the next frame into `pending`, or leaves it empty after the last part.
    fn advance(&mut self) -> Result<(), FinalizeError> {
        self.pending = None;
        loop {
            if self.reader.is_none() {
                let Some(part) = self.parts.get(self.next_part) else {
                    return Ok(());
                };
                self.next_part += 1;
                let reader = PartReader::open(self.folder, part, self.track_format)?;
                if reader.is_reencoded() {
                    self.parts_reencoded += 1;
                }
                self.reader = Some(reader);
            }
            let Some(reader) = self.reader.as_mut() else {
                continue;
            };
            let Some(sample) = reader.next_frame()? else {
                self.reader = None;
                continue;
            };
            let is_first_frame = reader.first_file_pts.is_none();
            let (retimed, pts_ns, duration_ns) = reader.retime(&sample, self.last_pts_ns)?;
            if is_first_frame && reader.is_shifted {
                self.parts_shifted += 1;
            }
            if self.track_format.is_none() {
                self.track_format = retimed.format_desc().and_then(TrackFormat::of);
            }
            self.last_pts_ns = Some(pts_ns);
            self.end_ns = self.end_ns.max(pts_ns + duration_ns);
            self.pending = Some((retimed, pts_ns));
            return Ok(());
        }
    }
}

/// One part's joined file, open for reading. The file goes when the reader does.
struct PartReader {
    joined: PathBuf,
    part_start_ns: u64,
    /// The part's first frame time as its file states it.
    first_file_pts: Option<cm::Time>,
    /// Its clock reading put its first frame at or before the last frame before it.
    is_shifted: bool,
    output: arc::R<av::AssetReaderTrackOutput>,
    reader: arc::R<av::AssetReader>,
    /// The first frame, read to learn the part's format.
    peeked: Option<arc::R<cm::SampleBuf>>,
    /// Set when the part is encoded again into the track's format.
    transcoder: Option<Transcoder>,
    _asset: arc::R<av::UrlAsset>,
}

/// A track the asset handed over on its loading queue.
struct LoadedTrack(arc::R<av::AssetTrack>);

// SAFETY: the loading queue only hands the track over; this thread alone uses it after.
unsafe impl Send for LoadedTrack {}

impl PartReader {
    /// Opens the part to copy its frames as they are, or, when its codec or size differs from
    /// `track_format`, to re-encode them into it.
    fn open(
        folder: &Path,
        part: &PlacedPart,
        track_format: Option<TrackFormat>,
    ) -> Result<Self, FinalizeError> {
        let joined = folder.join(format!(
            "{JOINED_PART_PREFIX}{:03}-{}.mp4",
            part.segment, part.part
        ));
        join_part(&joined, part)?;
        let read_error = |message: String| FinalizeError::PartRead {
            path: joined.clone(),
            message,
        };
        let url = ns::Url::with_fs_path_str(&joined.to_string_lossy(), false);
        let asset = av::UrlAsset::with_url(&url, None)
            .ok_or_else(|| read_error("AVFoundation refused the file".to_owned()))?;
        let track = load_video_track(&asset).map_err(read_error)?;
        let (output, reader) = start_reader(&asset, &track.0, None).map_err(read_error)?;
        let mut part_reader = Self {
            joined: joined.clone(),
            part_start_ns: part.start_ns,
            first_file_pts: None,
            is_shifted: false,
            output,
            reader,
            peeked: None,
            transcoder: None,
            _asset: asset.retained(),
        };
        let first = part_reader.next_source()?;
        if let (Some(sample), Some(format)) = (&first, track_format) {
            let is_same_format = sample.format_desc().and_then(TrackFormat::of) == Some(format);
            if !is_same_format {
                part_reader.reader.cancel_reading();
                let settings = decoded_frame_settings();
                let (output, reader) =
                    start_reader(&asset, &track.0, Some(&settings)).map_err(read_error)?;
                part_reader.output = output;
                part_reader.reader = reader;
                part_reader.transcoder = Some(Transcoder::new(format).map_err(read_error)?);
                return Ok(part_reader);
            }
        }
        part_reader.peeked = first;
        Ok(part_reader)
    }

    fn is_reencoded(&self) -> bool {
        self.transcoder.is_some()
    }

    /// The part's next frame: as the file holds it, or encoded again.
    fn next_frame(&mut self) -> Result<Option<arc::R<cm::SampleBuf>>, FinalizeError> {
        if let Some(sample) = self.peeked.take() {
            return Ok(Some(sample));
        }
        let joined = self.joined.clone();
        let read_error = |message: String| FinalizeError::PartRead {
            path: joined.clone(),
            message,
        };
        loop {
            let Some(transcoder) = self.transcoder.as_mut() else {
                return self.next_source();
            };
            if let Some(sample) = transcoder.take_encoded().map_err(read_error)? {
                return Ok(Some(sample));
            }
            if transcoder.is_flushed {
                return Ok(None);
            }
            let decoded = self.next_source()?;
            let Some(transcoder) = self.transcoder.as_mut() else {
                continue;
            };
            match decoded {
                Some(decoded) => transcoder
                    .encode(&decoded, time_ns(decoded.pts()))
                    .map_err(read_error)?,
                None => transcoder.flush().map_err(read_error)?,
            }
        }
    }

    /// The next frame the reader gives, compressed or decoded as it was opened.
    fn next_source(&mut self) -> Result<Option<arc::R<cm::SampleBuf>>, FinalizeError> {
        loop {
            match self.output.next_sample_buf() {
                Ok(Some(sample)) if sample.num_samples() == 0 => {}
                Ok(Some(sample)) => return Ok(Some(sample)),
                Ok(None) if self.reader.status() == av::AssetReaderStatus::Failed => {
                    return Err(FinalizeError::PartRead {
                        path: self.joined.clone(),
                        message: reader_error(&self.reader),
                    });
                }
                Ok(None) => return Ok(None),
                Err(exception) => {
                    return Err(FinalizeError::PartRead {
                        path: self.joined.clone(),
                        message: exception_reason(exception),
                    });
                }
            }
        }
    }

    /// A copy of the frame at its timeline time, with that time and its duration. Frames keep
    /// their spacing inside the part, since the encoder stamped each with its host time. A part
    /// whose start falls at or before `last_pts_ns` moves whole to one frame after it: track
    /// times must rise, and dropping its first frames would drop the key frame the rest need.
    fn retime(
        &mut self,
        sample: &cm::SampleBuf,
        last_pts_ns: Option<u64>,
    ) -> Result<(arc::R<cm::SampleBuf>, u64, u64), FinalizeError> {
        let read_error = |message: String| FinalizeError::PartRead {
            path: self.joined.clone(),
            message,
        };
        let timing = sample
            .timing_info(0)
            .map_err(|status| read_error(format!("frame timing: {status:?}")))?;
        let duration_ns = u64::try_from(time_ns(timing.duration))
            .ok()
            .filter(|ns| *ns > 0)
            .unwrap_or(FALLBACK_FRAME_NS);
        if self.first_file_pts.is_none()
            && let Some(last) = last_pts_ns
            && self.part_start_ns <= last
        {
            self.part_start_ns = last + duration_ns;
            self.is_shifted = true;
        }
        let first = *self.first_file_pts.get_or_insert(timing.pts);
        let offset_ns = time_ns(timing.pts) - time_ns(first);
        let pts_ns = u64::try_from(i128::from(self.part_start_ns) + offset_ns).unwrap_or(0);
        let pts = scaled_time(pts_ns);
        let moved = cm::SampleTimingInfo {
            duration: timing.duration,
            pts,
            // No B-frames, so each frame decodes when it shows.
            dts: if timing.dts.is_valid() {
                pts
            } else {
                cm::Time::invalid()
            },
        };
        let mut copy = None;
        unsafe {
            CMSampleBufferCreateCopyWithNewTiming(None, sample, 1, &raw const moved, &raw mut copy)
                .result()
                .map_err(|status| read_error(format!("frame copy: {status:?}")))?;
        }
        let copy = copy.ok_or_else(|| read_error("frame copy returned nothing".to_owned()))?;
        Ok((copy, pts_ns, duration_ns))
    }
}

impl Drop for PartReader {
    fn drop(&mut self) {
        self.reader.cancel_reading();
        if let Err(error) = std::fs::remove_file(&self.joined) {
            tracing::warn!(
                file.path = %self.joined.display(),
                exception.message = %error,
                "finalize.joined_part_not_removed"
            );
        }
    }
}

/// The part's init segment followed by its fragments: one fragmented MP4.
fn join_part(joined: &Path, part: &PlacedPart) -> Result<(), FinalizeError> {
    let io_error = |path: &Path| {
        let path = path.to_owned();
        move |source| FinalizeError::Io { path, source }
    };
    let mut out = File::create(joined).map_err(io_error(joined))?;
    for source in std::iter::once(&part.init).chain(&part.fragments) {
        let mut input = File::open(source).map_err(io_error(source))?;
        std::io::copy(&mut input, &mut out).map_err(io_error(source))?;
    }
    out.flush().map_err(io_error(joined))
}

/// A reader over the track, started. `None` settings hand frames over compressed.
fn start_reader(
    asset: &av::UrlAsset,
    track: &av::AssetTrack,
    settings: Option<&ns::Dictionary<ns::String, ns::Id>>,
) -> Result<(arc::R<av::AssetReaderTrackOutput>, arc::R<av::AssetReader>), String> {
    let output =
        av::AssetReaderTrackOutput::with_track(track, settings).map_err(exception_reason)?;
    let mut reader =
        av::AssetReader::with_asset(asset).map_err(|error| error.localized_desc().to_string())?;
    reader.add_output(&output).map_err(exception_reason)?;
    match reader.start_reading() {
        Ok(true) => Ok((output, reader)),
        Ok(false) => Err(reader_error(&reader)),
        Err(exception) => Err(exception_reason(exception)),
    }
}

fn load_video_track(asset: &av::UrlAsset) -> Result<LoadedTrack, String> {
    let (done, loaded) = mpsc::sync_channel(1);
    asset.load_tracks_with_media_type_block(av::MediaType::video(), move |tracks, error| {
        let outcome = match (tracks, error) {
            (Some(tracks), _) if !tracks.is_empty() => {
                tracks.get(0).map(LoadedTrack).map_err(exception_reason)
            }
            (_, Some(error)) => Err(error.localized_desc().to_string()),
            _ => Err("the file has no video track".to_owned()),
        };
        // The receiver is gone only once its wait timed out.
        let _unheard = done.try_send(outcome);
    });
    loaded
        .recv_timeout(TRACK_LOAD_LIMIT)
        .map_err(|_| "the video track did not load".to_owned())?
}

fn append(
    input: &mut av::AssetWriterInput,
    sample: &cm::SampleBuf,
    writer: &av::AssetWriter,
) -> Result<(), String> {
    match input.append_sample_buf(sample) {
        Ok(true) => Ok(()),
        Ok(false) => Err(writer_error(writer)),
        Err(exception) => Err(exception_reason(exception)),
    }
}

fn finish(writer: &mut av::AssetWriter) -> Result<(), String> {
    let (done, finished) = mpsc::sync_channel(1);
    writer.finish_writing_with_ch(move || {
        let _unheard = done.try_send(());
    });
    finished.recv_timeout(FINISH_LIMIT).map_err(|_| {
        format!(
            "the writer did not finish within {} s",
            FINISH_LIMIT.as_secs()
        )
    })?;
    match writer.status() {
        av::AssetWriterStatus::Completed => Ok(()),
        av::AssetWriterStatus::Unknown
        | av::AssetWriterStatus::Writing
        | av::AssetWriterStatus::Failed
        | av::AssetWriterStatus::Cancelled => Err(writer_error(writer)),
    }
}

/// The mix as the writer receives it: 16 kHz mono signed 16-bit, packed.
fn pcm_format() -> os::Result<arc::R<cm::AudioFormatDesc>> {
    let flags = cat::audio::FormatFlags(
        cat::audio::FormatFlags::IS_SIGNED_INTEGER.0 | cat::audio::FormatFlags::IS_PACKED.0,
    );
    cm::AudioFormatDesc::with_asbd(&cat::audio::StreamBasicDesc {
        sample_rate: f64::from(MIX_RATE_HZ),
        format: cat::audio::Format::LINEAR_PCM,
        format_flags: flags,
        bytes_per_packet: 2,
        frames_per_packet: 1,
        bytes_per_frame: 2,
        channels_per_frame: 1,
        bits_per_channel: 16,
        reserved: 0,
    })
}

fn pcm_sample(
    format: &cm::AudioFormatDesc,
    samples: &[i16],
    start_sample: u64,
) -> Result<arc::R<cm::SampleBuf>, String> {
    let status_error = |status: os::Error| format!("{status:?}");
    let mut block = cm::BlockBuf::with_mem_block(samples.len() * 2).map_err(status_error)?;
    let bytes_out = block.as_mut_slice().map_err(status_error)?;
    for (bytes, sample) in bytes_out.as_chunks_mut::<2>().0.iter_mut().zip(samples) {
        *bytes = sample.to_le_bytes();
    }
    let timing = cm::SampleTimingInfo {
        duration: cm::Time::new(1, i32::try_from(MIX_RATE_HZ).unwrap_or(i32::MAX)),
        pts: cm::Time::new(
            i64::try_from(start_sample).unwrap_or(i64::MAX),
            i32::try_from(MIX_RATE_HZ).unwrap_or(i32::MAX),
        ),
        dts: cm::Time::invalid(),
    };
    let sample_size_bytes = 2_usize;
    let mut sample = None;
    unsafe {
        cm::SampleBuf::create_in(
            None,
            Some(&block),
            true,
            None,
            std::ptr::null(),
            Some(format),
            isize::try_from(samples.len()).unwrap_or(isize::MAX),
            1,
            &raw const timing,
            1,
            &raw const sample_size_bytes,
            &raw mut sample,
        )
        .map_err(status_error)?;
    }
    sample.ok_or_else(|| "CoreMedia returned no sample buffer".to_owned())
}

fn aac_settings() -> arc::R<ns::Dictionary<ns::String, ns::Id>> {
    let format = ns::Number::with_u32(cat::audio::Format::MPEG4_AAC.0);
    let rate = ns::Number::with_f64(f64::from(MIX_RATE_HZ));
    let channels = ns::Number::with_u32(1);
    let bit_rate = ns::Number::with_u32(MIX_BIT_RATE_BPS);
    ns::Dictionary::with_keys_values(
        &[
            av::audio::all_formats_keys::id(),
            av::audio::all_formats_keys::sample_rate(),
            av::audio::all_formats_keys::number_of_channels(),
            av::audio::settings::encoder_propery_keys::bit_rate(),
        ],
        &[
            format.as_id_ref(),
            rate.as_id_ref(),
            channels.as_id_ref(),
            bit_rate.as_id_ref(),
        ],
    )
}

fn scaled_time(ns: u64) -> cm::Time {
    let value = i128::from(ns) * i128::from(VIDEO_TICKS_PER_SECOND) / 1_000_000_000;
    cm::Time::new(
        i64::try_from(value).unwrap_or(i64::MAX),
        VIDEO_TICKS_PER_SECOND,
    )
}

fn time_ns(time: cm::Time) -> i128 {
    if time.scale <= 0 {
        return 0;
    }
    i128::from(time.value) * 1_000_000_000 / i128::from(time.scale)
}

fn writer_error(writer: &av::AssetWriter) -> String {
    writer.error().map_or_else(
        || format!("status {:?}", writer.status()),
        |error| error.localized_desc().to_string(),
    )
}

fn reader_error(reader: &av::AssetReader) -> String {
    reader.error().map_or_else(
        || format!("status {:?}", reader.status()),
        |error| error.localized_desc().to_string(),
    )
}

fn exception_reason(exception: &ns::Exception) -> String {
    exception
        .reason()
        .map_or_else(|| exception.name().to_string(), |reason| reason.to_string())
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferCreateCopyWithNewTiming(
        allocator: Option<&cf::Allocator>,
        original: &cm::SampleBuf,
        timing_entry_count: cm::ItemCount,
        timing_array: *const cm::SampleTimingInfo,
        copy_out: *mut Option<arc::R<cm::SampleBuf>>,
    ) -> os::Status;
}
