//! Repair of video segments whose writer died before closing them. AVAssetWriter holds the
//! fragment it is building in memory, so a `kill -9` loses at most that one, about 2 s. Every
//! fragment before it is whole on disk and listed in the playlist.
//!
//! Repair finishes or deletes each `.partial` file by the writer's order rule, drops playlist
//! lines whose files are gone, adds `#EXT-X-ENDLIST`, brings the segment's fragment list in
//! `video.json` up to the playlist, and marks it `recovered`. It runs under `video.lock`, so a
//! live writer's folder is skipped. A second run finds nothing left to do.

use std::collections::BTreeMap;
use std::path::Path;

use jiff::Timestamp;
use serde::Serialize;
use sources::Platform;

use call_folder::FolderLock;

use crate::manifest::{
    FragmentRecord, FrameCounts, VideoCodec, VideoManifest, VideoRepair, VideoSegmentRecord,
    VideoSegmentState,
};
use crate::playlist::{PLAYLIST_FILE_NAME, Playlist, PlaylistEntry};
use crate::writer::final_name_of_partial;
use crate::{
    LOCK_FILE_NAME, MANIFEST_FILE_NAME, VideoWriterError, ms_to_ns, segment_folder_name,
    segment_number_of_folder,
};

/// What repair found in one call folder.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum VideoFolderRepair {
    /// Another process holds `video.lock`: its video process is recording there.
    Busy,
    /// Every segment was already closed or recovered, or the folder has no video.
    Clean,
    /// The segments repair recovered, as `video.json` now lists them.
    Repaired { segments: Vec<VideoSegmentRecord> },
}

/// Repairs the video segments of one call folder, unless its writer is alive.
///
/// # Errors
///
/// Fails when the folder or its lock cannot be read, or `video.json` is unreadable, from a newer
/// schema or cannot be stored. `video.json` then stays as it was, so the next run tries again.
pub fn repair_folder(folder: &Path) -> Result<VideoFolderRepair, VideoWriterError> {
    if !has_video(folder)? {
        return Ok(VideoFolderRepair::Clean);
    }
    let lock =
        FolderLock::try_take(folder, LOCK_FILE_NAME).map_err(|source| VideoWriterError::Io {
            path: folder.join(LOCK_FILE_NAME),
            source,
        })?;
    let Some(_lock) = lock else {
        tracing::info!(file.directory = %folder.display(), "video.repair_folder_busy");
        return Ok(VideoFolderRepair::Busy);
    };
    repair_locked(folder)
}

/// Whether the folder holds anything of the video process, so a folder with audio alone gets no
/// `video.lock`.
fn has_video(folder: &Path) -> Result<bool, VideoWriterError> {
    if folder.join(MANIFEST_FILE_NAME).exists() {
        return Ok(true);
    }
    Ok(!segment_folders(folder)?.is_empty())
}

/// Repairs the folder. The caller holds its lock.
pub(crate) fn repair_locked(folder: &Path) -> Result<VideoFolderRepair, VideoWriterError> {
    let mut manifest = VideoManifest::load_or_new(folder)?;
    let folders = segment_folders(folder)?;
    let repaired_at = Timestamp::now();
    let mut repaired_numbers = Vec::new();

    for segment in manifest
        .segments
        .iter_mut()
        .filter(|segment| segment.state == VideoSegmentState::Open)
    {
        let segment_dir = folder.join(&segment.folder);
        let repair = repair_segment(&segment_dir, segment, repaired_at, false)?;
        segment.state = VideoSegmentState::Recovered;
        segment.repair = Some(repair);
        repaired_numbers.push(segment.number);
    }

    // A folder `video.json` never listed: the writer died between creating it and storing the
    // entry, or `video.json` was lost. The other segments say whose call it is.
    let call_id = manifest
        .segments
        .iter()
        .rev()
        .find_map(|segment| segment.call_id.clone());
    let platform = manifest
        .segments
        .last()
        .map_or(Platform::Other, |segment| segment.platform);
    for (&number, name) in &folders {
        if manifest
            .segments
            .iter()
            .any(|segment| segment.number == number)
        {
            continue;
        }
        let mut segment = unlisted_segment(number, name.clone(), call_id.clone(), platform);
        let repair = repair_segment(&folder.join(name), &mut segment, repaired_at, true)?;
        segment.repair = Some(repair);
        manifest.segments.push(segment);
        repaired_numbers.push(number);
    }

    if repaired_numbers.is_empty() {
        return Ok(VideoFolderRepair::Clean);
    }
    manifest.segments.sort_by_key(|segment| segment.number);
    manifest.store(folder)?;
    let segments: Vec<VideoSegmentRecord> = manifest
        .segments
        .into_iter()
        .filter(|segment| repaired_numbers.contains(&segment.number))
        .collect();
    for segment in &segments {
        let repair = segment.repair;
        tracing::info!(
            file.directory = %folder.display(),
            segment.number = segment.number,
            call.id = segment.call_id.as_deref(),
            video.fragment_count = segment.fragments.len(),
            repair.partial_fragments_dropped = repair.map(|r| r.partial_fragments_dropped),
            repair.fragments_added = repair.map(|r| r.fragments_added),
            repair.is_rebuilt_from_files = repair.map(|r| r.is_rebuilt_from_files),
            "video.segment_recovered"
        );
    }
    Ok(VideoFolderRepair::Repaired { segments })
}

/// The `NNN-video` folders in the call folder, by number.
pub(crate) fn segment_folders(folder: &Path) -> Result<BTreeMap<u32, String>, VideoWriterError> {
    let io_error = |source| VideoWriterError::Io {
        path: folder.to_owned(),
        source,
    };
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(io_error(error)),
    };
    let mut folders = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(io_error)?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if let Some(number) = segment_number_of_folder(&name)
            && entry.file_type().map_err(io_error)?.is_dir()
        {
            folders.insert(number, name);
        }
    }
    Ok(folders)
}

fn unlisted_segment(
    number: u32,
    folder: String,
    call_id: Option<String>,
    platform: Platform,
) -> VideoSegmentRecord {
    debug_assert_eq!(folder, segment_folder_name(number));
    VideoSegmentRecord {
        number,
        audio_segment: None,
        call_id,
        platform,
        state: VideoSegmentState::Recovered,
        folder,
        playlist: PLAYLIST_FILE_NAME.to_owned(),
        codec: VideoCodec::Hevc,
        clock: None,
        closing_clock: None,
        closed_at: None,
        window: None,
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
        repair: None,
    }
}

/// Settles the segment's partial files, rewrites its playlist with only whole fragments and an
/// end, and makes its fragment list match.
fn repair_segment(
    segment_dir: &Path,
    segment: &mut VideoSegmentRecord,
    repaired_at: Timestamp,
    is_rebuilt_from_files: bool,
) -> Result<VideoRepair, VideoWriterError> {
    let io_error = |path: &Path| {
        let path = path.to_owned();
        move |source| VideoWriterError::Io { path, source }
    };
    let playlist_path = segment_dir.join(&segment.playlist);
    let text = match std::fs::read_to_string(&playlist_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(io_error(&playlist_path)(error)),
    };
    let listed = Playlist::parse(&text);
    let partial_fragments_dropped = settle_partials(segment_dir, &listed)?;
    let mut kept = Playlist {
        entries: Vec::new(),
        has_endlist: true,
    };
    let mut fragments = Vec::new();
    let mut part = 0_u32;
    let mut fragments_added = 0_u32;
    let mut previous_end_ns: Option<u64> = None;
    for entry in listed.entries {
        match &entry {
            PlaylistEntry::Init { file } => {
                if !segment_dir.join(file).exists() {
                    continue;
                }
                part += 1;
                // A part starts where its first frame is, not where the last part ended: a
                // window change or a stall lies between them.
                previous_end_ns = segment
                    .parts
                    .iter()
                    .find(|record| record.init == *file)
                    .map(|record| record.first_host_time_ns);
            }
            PlaylistEntry::Fragment { file, duration_ms } => {
                let Ok(metadata) = std::fs::metadata(segment_dir.join(file)) else {
                    continue;
                };
                let known = segment
                    .fragments
                    .iter()
                    .find(|fragment| fragment.file == *file)
                    .cloned();
                let record = known.unwrap_or_else(|| {
                    fragments_added += 1;
                    FragmentRecord {
                        file: file.clone(),
                        part,
                        // Fragments follow each other with no hole, so one starts where the last
                        // ended.
                        first_host_time_ns: previous_end_ns,
                        duration_ms: *duration_ms,
                        bytes: metadata.len(),
                    }
                });
                previous_end_ns = record
                    .first_host_time_ns
                    .map(|first| first.saturating_add(ms_to_ns(record.duration_ms)));
                fragments.push(record);
            }
        }
        kept.entries.push(entry);
    }
    let fragments_missing = u32::try_from(
        segment
            .fragments
            .iter()
            .filter(|listed| !fragments.iter().any(|kept| kept.file == listed.file))
            .count(),
    )
    .unwrap_or(u32::MAX);
    let is_endlist_added = !listed.has_endlist;
    if std::fs::metadata(segment_dir).is_ok() {
        let staged_path = segment_dir.join(format!(".{PLAYLIST_FILE_NAME}.repair"));
        std::fs::write(&staged_path, kept.render())
            .and_then(|()| std::fs::File::open(&staged_path)?.sync_all())
            .map_err(io_error(&staged_path))?;
        std::fs::rename(&staged_path, &playlist_path).map_err(io_error(&playlist_path))?;
        crate::sync_folder(segment_dir)?;
    }
    segment.last_host_time_ns = previous_end_ns.or(segment.last_host_time_ns);
    segment.fragments = fragments;
    Ok(VideoRepair {
        repaired_at,
        is_rebuilt_from_files,
        partial_fragments_dropped,
        fragments_added,
        fragments_missing,
        is_endlist_added,
    })
}

/// Finishes each listed `.partial` fragment and deletes the rest. The writer fsyncs a fragment
/// before its playlist line, so a listed partial is whole. Returns how many it deleted.
fn settle_partials(segment_dir: &Path, listed: &Playlist) -> Result<u32, VideoWriterError> {
    let io_error = |path: &Path| {
        let path = path.to_owned();
        move |source| VideoWriterError::Io { path, source }
    };
    let listed_fragments: Vec<&str> = listed
        .entries
        .iter()
        .filter_map(|entry| match entry {
            PlaylistEntry::Fragment { file, .. } => Some(file.as_str()),
            PlaylistEntry::Init { .. } => None,
        })
        .collect();
    let names: Vec<String> = match std::fs::read_dir(segment_dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(io_error(segment_dir)(error)),
    };
    let mut partial_fragments_dropped = 0_u32;
    for name in &names {
        let Some(final_name) = final_name_of_partial(name) else {
            continue;
        };
        let partial_path = segment_dir.join(name);
        let final_path = segment_dir.join(final_name);
        if listed_fragments.contains(&final_name) && !final_path.exists() {
            std::fs::rename(&partial_path, &final_path).map_err(io_error(&final_path))?;
            tracing::info!(file.path = %final_path.display(), "video.partial_fragment_finished");
        } else {
            std::fs::remove_file(&partial_path).map_err(io_error(&partial_path))?;
            partial_fragments_dropped += 1;
            tracing::info!(file.path = %partial_path.display(), "video.partial_fragment_dropped");
        }
    }
    Ok(partial_fragments_dropped)
}
