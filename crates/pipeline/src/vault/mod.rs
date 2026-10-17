//! The meeting notes folder in the vault. A write keeps every edit the user made outside the
//! app's sections and refuses one inside them, so a rerun never discards the user's edits.
//!
//! A note whose contents match a write the pipeline made, or started, is still the pipeline's
//! own. Hashes, unlike file times, survive Syncthing and git rewriting the time.

mod durable_write;
mod note_path;
mod splice;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jiff::Zoned;
use jobs::{ExtractionAtWrite, LeasedRun, NoteOwnership};

use crate::PipelineError;

pub(crate) struct Vault {
    meetings_dir: PathBuf,
    /// Outside the vault, so Syncthing never sees a half-written note.
    staging_dir: PathBuf,
}

impl Vault {
    pub(crate) fn new(meetings_dir: PathBuf, staging_dir: PathBuf) -> Self {
        Self {
            meetings_dir,
            staging_dir,
        }
    }

    /// Writes `rendered` as the run's meeting note, spliced into the note on disk when the user
    /// edited it, and records the write with `extraction`. A new note gets a path named from
    /// `started_at`, in the user's time zone, and `title`. Returns the note's path.
    ///
    /// # Errors
    ///
    /// Returns `PipelineError::NoteEdited` when the user's edits cannot be kept,
    /// `PipelineError::NoteNameTaken` when no free path is left for a new note,
    /// `PipelineError::ReadFile` when the note on disk cannot be read, `PipelineError::WriteNote`
    /// when the write fails, and a store error when the run lost its lease or a new capture
    /// superseded it.
    pub(crate) fn write(
        &self,
        run: &LeasedRun<'_>,
        started_at: &Zoned,
        title: &str,
        rendered: &str,
        extraction: ExtractionAtWrite<'_>,
    ) -> Result<PathBuf, PipelineError> {
        let ownership = &run.meeting().note;
        let note_path = match ownership.path() {
            Some(note_path) => note_path.to_path_buf(),
            None => note_path::claim(run, &self.meetings_dir, started_at, title)?,
        };
        let FittedNote {
            contents,
            edited_sha256,
            section_sha256s,
        } = fit_to_note(&note_path, ownership, rendered)?;
        let note_sha256 = note_sha256(contents.as_bytes());
        // Recorded first, so after a crash or a re-post mid-write the next run still knows the
        // note on disk is its own.
        run.record_pending_note(&note_sha256)?;
        durable_write::write(&self.staging_dir, &note_path, &contents, || {
            run.confirm_current_input()?;
            check_note_unchanged(&note_path, ownership, edited_sha256.as_deref())
        })?;
        run.mark_written(&note_sha256, &section_sha256s, extraction)?;
        Ok(note_path)
    }
}

/// Whether the meeting's note differs from every write the pipeline made, so a rerun has to
/// splice into it or overwrite it.
#[must_use]
pub fn has_user_edits(ownership: &NoteOwnership) -> bool {
    let known = ownership.known_sha256s();
    ownership.path().is_some_and(|note_path| {
        !known.is_empty()
            && std::fs::read(note_path)
                .is_ok_and(|on_disk| !known.contains(&note_sha256(&on_disk).as_str()))
    })
}

struct FittedNote {
    contents: String,
    /// The hash of the edited note the splice started from, which the write checks again just
    /// before the rename.
    edited_sha256: Option<String>,
    /// Stored after the write, so the next rerun can tell the app's sections from the user's.
    section_sha256s: BTreeMap<String, String>,
}

// The contents to write: the render spliced into the note on disk, so text the user added outside
// the sections survives every rerun, not only the first.
fn fit_to_note(
    note_path: &Path,
    ownership: &NoteOwnership,
    rendered: &str,
) -> Result<FittedNote, PipelineError> {
    let note_edited = |reason: String| PipelineError::NoteEdited {
        path: note_path.to_path_buf(),
        reason,
    };
    let rendered_section_sha256s = splice::section_sha256s(rendered)
        .map_err(|reason| note_edited(format!("in the template, {reason}")))?;
    let as_rendered = |section_sha256s: BTreeMap<String, String>| FittedNote {
        contents: rendered.to_owned(),
        edited_sha256: None,
        section_sha256s,
    };
    let known = ownership.known_sha256s();
    // No hash: a new note, or edits the user chose to discard.
    if known.is_empty() {
        return Ok(as_rendered(rendered_section_sha256s));
    }
    let on_disk = match std::fs::read_to_string(note_path) {
        Ok(on_disk) => on_disk,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(as_rendered(rendered_section_sha256s));
        }
        Err(source) => {
            return Err(PipelineError::ReadFile {
                path: note_path.to_path_buf(),
                source,
            });
        }
    };
    let on_disk_sha256 = note_sha256(on_disk.as_bytes());
    let is_as_written = known.contains(&on_disk_sha256.as_str());
    // A custom template without markers leaves nothing to splice into.
    if rendered_section_sha256s.is_empty() {
        return if is_as_written {
            Ok(as_rendered(rendered_section_sha256s))
        } else {
            Err(note_edited(
                "the note template has no mc:begin/mc:end markers, so the app cannot keep your edits"
                    .to_owned(),
            ))
        };
    }
    let mut written = ownership.section_sha256s().clone();
    if is_as_written {
        let on_disk_section_sha256s = splice::section_sha256s(&on_disk)
            .map_err(|reason| note_edited(format!("in the note, {reason}")))?;
        // Written from a template without markers: every section would land after the old text.
        if on_disk_section_sha256s.is_empty() {
            return Ok(as_rendered(rendered_section_sha256s));
        }
        // A note the pipeline wrote holds its sections as written, which covers a write that
        // crashed before the store recorded its section hashes.
        written.extend(on_disk_section_sha256s);
    }
    let spliced = splice::splice(&on_disk, rendered, &written).map_err(note_edited)?;
    let mut section_sha256s = splice::section_sha256s(&spliced)
        .map_err(|reason| note_edited(format!("in the spliced note, {reason}")))?;
    // A section the user deleted keeps its hash, so later reruns leave it out too.
    for (name, sha256) in written {
        if rendered_section_sha256s.contains_key(&name) {
            section_sha256s.entry(name).or_insert(sha256);
        }
    }
    Ok(FittedNote {
        contents: spliced,
        edited_sha256: Some(on_disk_sha256),
        section_sha256s,
    })
}

// Run just before the rename: the note must still be one the pipeline wrote, or the edited note
// the splice started from.
fn check_note_unchanged(
    note_path: &Path,
    ownership: &NoteOwnership,
    edited_sha256: Option<&str>,
) -> Result<(), PipelineError> {
    let mut expected = ownership.known_sha256s();
    expected.extend(edited_sha256);
    if expected.is_empty() {
        return Ok(());
    }
    match std::fs::read(note_path) {
        Ok(on_disk) if expected.contains(&note_sha256(&on_disk).as_str()) => Ok(()),
        Ok(_) => Err(PipelineError::NoteEdited {
            path: note_path.to_path_buf(),
            reason: "it changed while the run was writing; the next run will try again".to_owned(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(PipelineError::ReadFile {
            path: note_path.to_path_buf(),
            source,
        }),
    }
}

// SHA-256 of a note or a section, in lowercase hex.
fn note_sha256(contents: &[u8]) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    sha2::Sha256::digest(contents)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}
