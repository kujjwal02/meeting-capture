//! Repair of segments whose writer died before closing them. After a `kill -9` or a crash the
//! page cache still holds every sample the writer handed the OS, but the header counts only the
//! samples at the last checkpoint and `audio.json` still says `open`.
//!
//! Repair rebuilds each stem's header from the file length, cutting a partial trailing sample,
//! and marks the segment `recovered`. It never appends a sample and never hands a file back to a
//! writer: the next recording in the folder takes the next segment number. It runs under the
//! folder's lock, so it skips a folder whose writer is alive. A second run finds nothing left to
//! do. A stem file it cannot repair is left as it was and listed as skipped, with the reason;
//! the folder's other stems are still repaired.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;

use jiff::Timestamp;
use serde::Serialize;
use sources::Platform;
use stem_timing::{Stem, StemLosses};

use call_folder::FolderLock;

use crate::manifest::{
    AudioManifest, SegmentRecord, SegmentRepair, SegmentState, StemFix, StemRecord, StemRepair,
};
use crate::wav::{BYTES_PER_SAMPLE, HEADER_BYTES, data_bytes, wav_header};
use crate::{LOCK_FILE_NAME, OUTPUT_RATE_HZ, StemWriterError, segment_number_of, stem_file_name};

/// What repair found in one call folder.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum FolderRepair {
    /// Another process holds the folder's lock: its helper is recording there.
    Busy,
    /// Every segment was already closed or recovered.
    Clean,
    /// The segments repair recovered, as `audio.json` now lists them.
    Repaired { segments: Vec<SegmentRecord> },
}

/// Repairs one call folder, unless its writer is alive.
///
/// # Errors
///
/// Fails when the folder or its lock cannot be read, or `audio.json` is unreadable, from a newer
/// schema or cannot be stored. Then `audio.json` stays as it was, so the next run tries again. A
/// stem file that cannot be repaired does not fail the folder: it is listed as skipped.
pub fn repair_folder(folder: &Path) -> Result<FolderRepair, StemWriterError> {
    let lock =
        FolderLock::try_take(folder, LOCK_FILE_NAME).map_err(|source| StemWriterError::Io {
            path: folder.join(LOCK_FILE_NAME),
            source,
        })?;
    let Some(_lock) = lock else {
        tracing::info!(file.directory = %folder.display(), "repair.folder_busy");
        return Ok(FolderRepair::Busy);
    };
    repair_locked(folder)
}

/// Repairs the folder. The caller holds its lock.
pub(crate) fn repair_locked(folder: &Path) -> Result<FolderRepair, StemWriterError> {
    let mut manifest = AudioManifest::load_or_new(folder)?;
    let files = stem_files(folder)?;
    let repaired_at = Timestamp::now();
    let mut repaired_numbers = Vec::new();

    for segment in manifest
        .segments
        .iter_mut()
        .filter(|segment| segment.state == SegmentState::Open)
    {
        let mut skipped_count = 0;
        for record in &mut segment.stems {
            skipped_count += usize::from(!repair_or_skip(folder, segment.number, record, true));
        }
        // The writer creates a file, then lists it; a death between the two leaves it unlisted.
        for &(stem, ref file) in files.get(&segment.number).into_iter().flatten() {
            if !segment.stems.iter().any(|record| record.stem == stem) {
                let mut record = unlisted_stem(stem, file.clone());
                skipped_count +=
                    usize::from(!repair_or_skip(folder, segment.number, &mut record, false));
                segment.stems.push(record);
            }
        }
        // A segment whose every stem failed stays open, so the next run retries it: the cause
        // may pass, as a file another process held open on Windows.
        if skipped_count > 0 && skipped_count == segment.stems.len() {
            continue;
        }
        segment.state = SegmentState::Recovered;
        segment.repair = Some(SegmentRepair {
            repaired_at,
            is_rebuilt_from_files: false,
        });
        repaired_numbers.push(segment.number);
    }

    // Files of a segment `audio.json` never listed, or of a folder that lost its `audio.json`.
    // The other segments, when any, say whose call it is.
    let call_id = manifest
        .segments
        .iter()
        .rev()
        .find_map(|segment| segment.call_id.clone());
    let platform = manifest
        .segments
        .last()
        .map_or(Platform::Other, |segment| segment.platform);
    for (&number, stem_files) in &files {
        if manifest
            .segments
            .iter()
            .any(|segment| segment.number == number)
        {
            continue;
        }
        let mut stems = Vec::new();
        let mut is_any_repaired = false;
        for (stem, file) in stem_files {
            let mut record = unlisted_stem(*stem, file.clone());
            is_any_repaired |= repair_or_skip(folder, number, &mut record, false);
            stems.push(record);
        }
        // With every file skipped the segment stays unlisted, so the next run retries it.
        if !is_any_repaired {
            continue;
        }
        manifest.segments.push(SegmentRecord {
            number,
            call_id: call_id.clone(),
            platform,
            state: SegmentState::Recovered,
            clock: None,
            closing_clock: None,
            closed_at: None,
            stems,
            repair: Some(SegmentRepair {
                repaired_at,
                is_rebuilt_from_files: true,
            }),
        });
        repaired_numbers.push(number);
    }

    if repaired_numbers.is_empty() {
        return Ok(FolderRepair::Clean);
    }
    manifest.segments.sort_by_key(|segment| segment.number);
    manifest.store(folder)?;
    let segments: Vec<SegmentRecord> = manifest
        .segments
        .into_iter()
        .filter(|segment| repaired_numbers.contains(&segment.number))
        .collect();
    for segment in &segments {
        tracing::info!(
            file.directory = %folder.display(),
            segment.number = segment.number,
            call.id = segment.call_id.as_deref(),
            repair.is_rebuilt_from_files =
                segment.repair.is_some_and(|repair| repair.is_rebuilt_from_files),
            "segment.recovered"
        );
    }
    Ok(FolderRepair::Repaired { segments })
}

/// A stem whose file is on disk but not in the manifest: nothing is known but its name.
fn unlisted_stem(stem: Stem, file: String) -> StemRecord {
    StemRecord {
        stem,
        file,
        first_host_time_ns: None,
        started_at: None,
        last_host_time_ns: None,
        sample_rate_hz: OUTPUT_RATE_HZ,
        samples_written: 0,
        devices: Vec::new(),
        rates_in: Vec::new(),
        gaps: Vec::new(),
        runs_ahead: Vec::new(),
        ring_losses: StemLosses::default(),
        stopped_by: None,
        repair: None,
    }
}

/// Repairs one stem file and says whether it did. A failure leaves the record's counts as they
/// were and lists the stem as skipped, with the reason and any fix already made to the file.
fn repair_or_skip(folder: &Path, number: u32, record: &mut StemRecord, is_listed: bool) -> bool {
    let mut fixes = Vec::new();
    if !is_listed {
        fixes.push(StemFix::AddedToManifest);
    }
    let Err(error) = repair_stem(folder, number, record, is_listed, &mut fixes) else {
        return true;
    };
    fixes.push(StemFix::Skipped);
    record.repair = Some(StemRepair {
        header_samples_before: None,
        listed_samples_before: is_listed.then_some(record.samples_written),
        trailing_bytes_cut: 0,
        positions_dropped: 0,
        fixes,
        skipped_reason: Some(error.to_string()),
    });
    tracing::warn!(
        file.path = %folder.join(&record.file).display(),
        segment.number = number,
        capture.stem = %record.stem,
        exception.message = %error,
        "stem.repair_skipped"
    );
    false
}

/// Rebuilds one stem file's header from its length and updates its record to match. Writes only
/// the header and the file length; the samples stay as they are.
fn repair_stem(
    folder: &Path,
    number: u32,
    record: &mut StemRecord,
    is_listed: bool,
    fixes: &mut Vec<StemFix>,
) -> Result<(), StemWriterError> {
    let path = folder.join(&record.file);
    let io_error = |source| StemWriterError::Io {
        path: path.clone(),
        source,
    };
    let listed_samples_before = is_listed.then_some(record.samples_written);
    let mut file = match File::options().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fixes.push(StemFix::FileMissing);
            record.samples_written = 0;
            record.last_host_time_ns = record.first_host_time_ns;
            let positions_dropped = cap_positions(record, 0, fixes);
            record.repair = Some(StemRepair {
                header_samples_before: None,
                listed_samples_before,
                trailing_bytes_cut: 0,
                positions_dropped,
                fixes: std::mem::take(fixes),
                skipped_reason: None,
            });
            log_stem_repair(folder, number, record);
            return Ok(());
        }
        Err(source) => return Err(io_error(source)),
    };
    let file_bytes = file.metadata().map_err(io_error)?.len();
    let header_bytes = u64::from(HEADER_BYTES);

    let (header_samples_before, data_bytes_after, trailing_bytes_cut) = if file_bytes < header_bytes
    {
        // The writer's buffer never reached the file, so not even the header is whole. One write
        // of the whole header sizes the file.
        write_header(&mut file, 0).map_err(io_error)?;
        fixes.push(StemFix::HeaderWritten);
        (None, 0, 0)
    } else {
        let mut header = [0_u8; HEADER_BYTES as usize];
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.read_exact(&mut header))
            .map_err(io_error)?;
        let template = wav_header(0);
        // Every field but the two sizes is fixed for a file this writer made.
        let is_own_layout = header[..4] == template[..4] && header[8..40] == template[8..40];
        if !is_own_layout {
            return Err(StemWriterError::NotOwnWav { path });
        }
        let counted_bytes = u32::from_le_bytes([header[40], header[41], header[42], header[43]]);
        let file_data_bytes = file_bytes - header_bytes;
        let trailing = file_data_bytes % u64::from(BYTES_PER_SAMPLE);
        let whole_data_bytes = file_data_bytes - trailing;
        let samples = whole_data_bytes / u64::from(BYTES_PER_SAMPLE);
        // Every check comes before the first write, so a skipped file is untouched.
        let data_after =
            data_bytes(samples).ok_or_else(|| StemWriterError::WavFull { path: path.clone() })?;
        // The header first: it counts whole samples only, so a file whose cut then fails still
        // plays to the same end.
        if header != wav_header(data_after) {
            write_header(&mut file, data_after).map_err(io_error)?;
            fixes.push(StemFix::HeaderSizesRewritten);
        }
        if trailing > 0 {
            file.set_len(header_bytes + whole_data_bytes)
                .map_err(io_error)?;
            fixes.push(StemFix::PartialSampleCut);
        }
        (
            Some(u64::from(counted_bytes) / u64::from(BYTES_PER_SAMPLE)),
            data_after,
            trailing,
        )
    };
    file.sync_all().map_err(io_error)?;

    let samples_after = u64::from(data_bytes_after) / u64::from(BYTES_PER_SAMPLE);
    record.last_host_time_ns = last_host_time(record, samples_after);
    record.samples_written = samples_after;
    let positions_dropped = cap_positions(record, samples_after, fixes);
    record.repair = Some(StemRepair {
        header_samples_before,
        listed_samples_before,
        trailing_bytes_cut,
        positions_dropped,
        fixes: std::mem::take(fixes),
        skipped_reason: None,
    });
    log_stem_repair(folder, number, record);
    Ok(())
}

/// Caps the record's file positions at the repaired end, for a file shorter than what the
/// writer recorded, as after a power cut. Entries that start past the end are dropped and
/// counted; ones that run past it end there. The first device and rate start the file, so they
/// stay.
fn cap_positions(record: &mut StemRecord, samples: u64, fixes: &mut Vec<StemFix>) -> u64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "sample counts stay far below 2^52"
    )]
    let end_ms = samples as f64 * 1e3 / f64::from(OUTPUT_RATE_HZ);
    let before =
        record.gaps.len() + record.runs_ahead.len() + record.devices.len() + record.rates_in.len();
    let mut is_trimmed = false;
    record.gaps.retain(|gap| gap.start_ms < end_ms);
    for gap in &mut record.gaps {
        if gap.start_ms + gap.length_ms > end_ms {
            gap.length_ms = end_ms - gap.start_ms;
            is_trimmed = true;
        }
    }
    record.runs_ahead.retain(|span| span.start_ms < end_ms);
    for span in &mut record.runs_ahead {
        if let Some(span_end_ms) = span.end_ms
            && span_end_ms > end_ms
        {
            // The lead shrinks linearly over the span, as `host_time_at` reads it.
            let progress = (end_ms - span.start_ms) / (span_end_ms - span.start_ms).max(1e-9);
            span.end_ahead_ms =
                span.start_ahead_ms + (span.end_ahead_ms - span.start_ahead_ms) * progress;
            span.end_ms = Some(end_ms);
            is_trimmed = true;
        }
    }
    let mut index = 0;
    record.devices.retain(|device| {
        index += 1;
        index == 1 || device.start_ms < end_ms
    });
    let mut index = 0;
    record.rates_in.retain(|rate| {
        index += 1;
        index == 1 || rate.start_ms < end_ms
    });
    let after =
        record.gaps.len() + record.runs_ahead.len() + record.devices.len() + record.rates_in.len();
    let dropped = (before - after) as u64;
    if dropped > 0 || is_trimmed {
        fixes.push(StemFix::PositionsCapped);
    }
    dropped
}

/// Host time at the end of the repaired file. The writer's last value carries its drift
/// estimate, so the samples past it extend it; with no last value, the file position maps
/// through the runs ahead.
fn last_host_time(record: &StemRecord, samples_after: u64) -> Option<u64> {
    let listed_end_ns = record.last_host_time_ns.map(|last_ns| {
        let extra_samples = i128::from(samples_after) - i128::from(record.samples_written);
        let extra_ns = extra_samples * 1_000_000_000 / i128::from(OUTPUT_RATE_HZ);
        u64::try_from(i128::from(last_ns) + extra_ns).unwrap_or(last_ns)
    });
    listed_end_ns.or_else(|| {
        #[expect(
            clippy::cast_precision_loss,
            reason = "sample counts stay far below 2^52"
        )]
        let end_ms = samples_after as f64 * 1e3 / f64::from(OUTPUT_RATE_HZ);
        record.host_time_at(end_ms)
    })
}

fn write_header(file: &mut File, data_bytes: u32) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&wav_header(data_bytes))
}

fn log_stem_repair(folder: &Path, number: u32, record: &StemRecord) {
    let Some(repair) = &record.repair else {
        return;
    };
    tracing::info!(
        file.path = %folder.join(&record.file).display(),
        segment.number = number,
        capture.stem = %record.stem,
        repair.header_samples_before = repair.header_samples_before,
        repair.listed_samples_before = repair.listed_samples_before,
        repair.samples_after = record.samples_written,
        repair.trailing_bytes_cut = repair.trailing_bytes_cut,
        repair.positions_dropped = repair.positions_dropped,
        repair.fixes = %FixList(&repair.fixes),
        "stem.repaired"
    );
}

/// The folder's stem files by segment number: every `NNN-mic.wav` and `NNN-remote.wav`.
fn stem_files(folder: &Path) -> Result<BTreeMap<u32, Vec<(Stem, String)>>, StemWriterError> {
    let io_error = |source| StemWriterError::Io {
        path: folder.to_owned(),
        source,
    };
    let mut files: BTreeMap<u32, Vec<(Stem, String)>> = BTreeMap::new();
    for entry in std::fs::read_dir(folder).map_err(io_error)? {
        let name = entry.map_err(io_error)?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(number) = segment_number_of(name) else {
            continue;
        };
        for stem in Stem::ALL {
            if name == stem_file_name(number, stem) {
                files
                    .entry(number)
                    .or_default()
                    .push((stem, name.to_owned()));
            }
        }
    }
    for stems in files.values_mut() {
        stems.sort_by_key(|(stem, _)| Stem::ALL.iter().position(|known| known == stem));
    }
    Ok(files)
}

/// Fixes as one comma-separated log field.
struct FixList<'a>(&'a [StemFix]);

impl std::fmt::Display for FixList<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, fix) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str(",")?;
            }
            write!(formatter, "{fix}")?;
        }
        Ok(())
    }
}
