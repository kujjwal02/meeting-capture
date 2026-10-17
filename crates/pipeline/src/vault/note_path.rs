//! Chooses a new meeting's note path in the vault and claims it in the job store.

use std::path::{Path, PathBuf};

use jiff::Zoned;
use jobs::{LeasedRun, MeetingId, NotePathClaim};

use crate::PipelineError;
use crate::render::UNTITLED_CALL;

// Claimed before the first write, so a crash between the write and mark_written rewrites this note
// instead of adding a second one. A path is skipped when a file is there, which covers notes that
// no job owns, such as the Attendee bot's.
pub(super) fn claim(
    run: &LeasedRun<'_>,
    meetings_dir: &Path,
    started_at: &Zoned,
    title: &str,
) -> Result<PathBuf, PipelineError> {
    for note_path in candidates(meetings_dir, run.meeting().id, started_at, title) {
        if note_path.exists() {
            continue;
        }
        match run.claim_note_path(&note_path)? {
            NotePathClaim::Claimed => return Ok(note_path),
            NotePathClaim::TakenByOtherMeeting => {}
        }
    }
    Err(PipelineError::NoteNameTaken)
}

// Note paths to try for a new meeting, in order: `YYYY/MM/yyyy-mm-dd-hhmm-<title-slug>.md` under
// `meetings_dir`, then the same with the meeting id, which is unique to the meeting.
fn candidates(meetings_dir: &Path, id: MeetingId, started_at: &Zoned, title: &str) -> [PathBuf; 2] {
    let month_dir = meetings_dir
        .join(started_at.strftime("%Y").to_string())
        .join(started_at.strftime("%m").to_string());
    let date_time = started_at.strftime("%Y-%m-%d-%H%M");
    let title = title_slug(title);
    [
        month_dir.join(format!("{date_time}-{title}.md")),
        month_dir.join(format!("{date_time}-{title}-{id}.md")),
    ]
}

// Long event titles would push paths past Windows' 260-character limit.
const MAX_SLUG_CHARS: usize = 60;

fn title_slug(title: &str) -> String {
    let slug = slug::slugify(title);
    let mut slug = if slug.len() <= MAX_SLUG_CHARS {
        slug
    } else {
        let cut = slug[..MAX_SLUG_CHARS].rfind('-').unwrap_or(MAX_SLUG_CHARS);
        slug[..cut].to_owned()
    };
    if slug.is_empty() {
        slug = slug::slugify(UNTITLED_CALL);
    }
    slug
}
