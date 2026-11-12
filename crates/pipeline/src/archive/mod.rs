//! The Drive archive of a call folder: its media goes to the Drive account's folder, and the
//! local copy goes only once every upload is verified. `restore_folder` brings it back.
//!
//! A folder with `call.mp4` archives that file and, as lossless FLAC, each stem its mix left
//! out; then it deletes every WAV stem and `call.mp4`. A folder with no video, or whose video the
//! 90-day purge took, archives each stem as FLAC and deletes the WAVs. Manifests, `stt/`,
//! `finalize.json`, locks, purge files and fragment folders stay.
//!
//! `drive.json` in the call folder is `uploading` while each verified upload is recorded as it
//! lands, with the resumable session of an upload under way. Once every planned file is
//! recorded, and the plan holds again under the folder's locks, it is `uploaded`, before
//! anything is deleted. Once the local copies are gone it is `media_deleted`. A run in
//! `uploaded` only finishes the deletion of what is still there. A restore leaves the record
//! `uploaded` with its time, and the next run after `RESTORED_MEDIA_KEEP` deletes the restored
//! files again without uploading them.

pub(crate) mod flac;
mod open_calls;

pub use open_calls::OpenCallsFile;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use call_finalize::{ArchiveKind, FinalizeError};
use call_folder::drive_record::{
    ArchiveState, ArchivedFile, ArchivedFileKind, DriveRecordError, RecordVersion,
};
use call_folder::media::{CallMedia, MediaLink, MediaTarget, VIDEO_FILE_NAME};
use jiff::{SignedDuration, Timestamp};
use jobs::{JobState, Meeting, MeetingId, StoreError};

use crate::drive::{DriveClient, DriveError, FileId, FolderId, UploadSession, VerifiedUpload};
use crate::transcribe::{
    SavedTranscriptStanding, saved_replies_match_stems, saved_transcript_standing,
};
use crate::{Pipeline, call_folder_name};
use flac::{FlacError, RestoredStem};

/// `drive.json` with this crate's upload sessions.
pub type DriveRecord = call_folder::drive_record::DriveRecord<UploadSession>;

/// FLAC files wait here between encode and upload, and downloads between Drive and their
/// check. A dot folder, so nothing that lists the call folder's media sees it.
const STAGING_DIR_NAME: &str = ".drive-archive";

/// Held by an archive or restore run for its whole length, so two processes never work on
/// one folder at once.
const ARCHIVE_LOCK_FILE_NAME: &str = "archive.lock";

/// A restore is for listening back or transcribing again. A week covers that; after it the
/// Drive copy is enough again, and the restored files go without a second upload.
pub const RESTORED_MEDIA_KEEP: SignedDuration = SignedDuration::from_hours(7 * 24);

/// Why a call folder keeps its media for now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveHold {
    /// Finalize, the purge clock or a lock says not yet.
    Folder(call_finalize::ArchiveHold),
    /// No meeting in the job store names this call folder.
    NoMeeting,
    /// A meeting of the call has not reached `Written` with a settled transcript: it runs, waits
    /// for the network or a retry, failed, or owes speech-to-text.
    MeetingNotWritten {
        meeting_id: MeetingId,
        state: &'static str,
    },
    /// `stt/choice.json` says the note holds the captions while speech-to-text is owed.
    TranscriptPending,
    TranscriptChoiceUnreadable,
    /// A folder to archive as stems has none.
    NoStems,
    /// A stem's header is not the stem writer's, so FLAC could not rebuild it byte for byte.
    StemNotAsWritten {
        file_name: String,
        reason: &'static str,
    },
    /// `mc restore` brought the media back less than `RESTORED_MEDIA_KEEP` ago.
    RestoredRecently {
        deletes_after: Timestamp,
    },
    /// Under the locks, the folder no longer plans to the files this run uploaded, or a planned
    /// file changed since its upload.
    PlanChanged,
    /// A recorded Drive copy is gone, in the trash or not as uploaded, while its local file is
    /// still here. The record is back to `uploading`, so the next run uploads it again.
    DriveCopyLost {
        file_name: String,
    },
    /// Another archive or restore run holds the folder's `archive.lock`.
    ArchiveRunning,
    /// A saved speech-to-text reply no longer matches the stems it was read from, so it could
    /// not stand in for them once they are gone.
    RepliesStale,
    /// The archive was turned off, or its account changed, during the run.
    ArchiveTurnedOff,
    /// Every upload is on the Drive of another account than the one signed in now; only that
    /// account may finish the deletion.
    OtherAccount {
        account: String,
    },
}

impl ArchiveHold {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Folder(hold) => hold.as_str(),
            Self::NoMeeting => "no_meeting",
            Self::MeetingNotWritten { .. } => "meeting_not_written",
            Self::TranscriptPending => "transcript_pending",
            Self::TranscriptChoiceUnreadable => "transcript_choice_unreadable",
            Self::NoStems => "no_stems",
            Self::StemNotAsWritten { .. } => "stem_not_as_written",
            Self::RestoredRecently { .. } => "restored_recently",
            Self::PlanChanged => "plan_changed",
            Self::DriveCopyLost { .. } => "drive_copy_lost",
            Self::ArchiveRunning => "archive_running",
            Self::RepliesStale => "replies_stale",
            Self::ArchiveTurnedOff => "archive_turned_off",
            Self::OtherAccount { .. } => "other_account",
        }
    }

    /// One line for a person.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Folder(hold) => match hold {
                call_finalize::ArchiveHold::AlreadyArchived => {
                    "already archived, or drive.json cannot be read".to_owned()
                }
                call_finalize::ArchiveHold::OutsideCalls => {
                    "not a folder inside the calls folder".to_owned()
                }
                call_finalize::ArchiveHold::CallOpen => "the app holds the call open".to_owned(),
                call_finalize::ArchiveHold::NoPurgeClock => {
                    "no purge has run yet, so the call's age is unknown".to_owned()
                }
                call_finalize::ArchiveHold::NoCaptureFile => {
                    "no stem, manifest or fragment to date the call by".to_owned()
                }
                call_finalize::ArchiveHold::EndedRecently { call_ended_at } => {
                    format!("the call ended under an hour ago ({call_ended_at})")
                }
                call_finalize::ArchiveHold::Busy => {
                    "a capture, finalize or purge holds the folder".to_owned()
                }
                call_finalize::ArchiveHold::FinalizePending => {
                    "call.mp4 is not written from the current manifests yet".to_owned()
                }
                call_finalize::ArchiveHold::FinalizeFailed => "finalize failed".to_owned(),
                call_finalize::ArchiveHold::CallFileMismatch => {
                    "call.mp4 has another length than finalize recorded".to_owned()
                }
                call_finalize::ArchiveHold::CallFileIncomplete => {
                    "call.mp4 is not a complete MP4".to_owned()
                }
                call_finalize::ArchiveHold::MixUnknown { message } => {
                    format!("which stems call.mp4 mixed is unknown: {message}")
                }
                call_finalize::ArchiveHold::FragmentsLeft => {
                    "the fragment cleanup has not run on it yet".to_owned()
                }
                call_finalize::ArchiveHold::PurgeUnfinished => {
                    "the 90-day purge left call.mp4; its next run finishes".to_owned()
                }
                call_finalize::ArchiveHold::StaleCallFile => {
                    "finalize recorded no video, yet call.mp4 is there".to_owned()
                }
            },
            Self::NoMeeting => "no meeting in the job store names this call folder".to_owned(),
            Self::MeetingNotWritten { meeting_id, state } => {
                format!("meeting {meeting_id} is {state}, not written")
            }
            Self::TranscriptPending => "speech-to-text is still owed".to_owned(),
            Self::TranscriptChoiceUnreadable => "stt/choice.json cannot be read".to_owned(),
            Self::NoStems => "no WAV stems to archive".to_owned(),
            Self::StemNotAsWritten { file_name, reason } => {
                format!("{file_name} is not as the stem writer writes it: {reason}")
            }
            Self::RestoredRecently { deletes_after } => {
                format!("restored recently; its files stay until {deletes_after}")
            }
            Self::PlanChanged => {
                "the folder changed during the uploads; the next run plans it again".to_owned()
            }
            Self::DriveCopyLost { file_name } => format!(
                "the Drive copy of {file_name} is gone, in the trash or changed; the next run uploads it again"
            ),
            Self::ArchiveRunning => "another archive or restore run holds the folder".to_owned(),
            Self::RepliesStale => {
                "a saved speech-to-text reply no longer matches the stems; transcribe it again"
                    .to_owned()
            }
            Self::ArchiveTurnedOff => {
                "the archive was turned off or its account changed during the run".to_owned()
            }
            Self::OtherAccount { account } => {
                format!("its media is on the Drive of {account}; sign in as that account to finish")
            }
        }
    }
}

/// What an archive run would upload and delete.
#[derive(Debug, Clone)]
pub struct ArchivePlan {
    pub kind: ArchiveKind,
    pub call_ended_at: Timestamp,
    /// `call.mp4` and each stem its mix left out, or every stem, such as `001-mic.wav`.
    pub upload_names: Vec<String>,
    /// The files deleted once every upload is verified, `call.mp4` last.
    pub deleted_names: Vec<String>,
    pub freed_bytes: u64,
    /// Below the Drive root folder: year, month, and `<YYYY-MM-DD HHMM> <title>`.
    pub drive_folder_names: Vec<String>,
}

/// The deletion a folder in `uploaded` still owes: its recorded files that are still there.
#[derive(Debug, Clone)]
pub struct OwedDeletion {
    pub deleted_names: Vec<String>,
    pub freed_bytes: u64,
}

/// Whether a call folder may be archived now.
#[derive(Debug, Clone)]
pub enum ArchiveReadiness {
    Ready(ArchivePlan),
    /// Every upload is recorded; the run only deletes.
    DeletionOwed(OwedDeletion),
    Held(ArchiveHold),
}

#[derive(Debug, Clone)]
pub enum ArchiveOutcome {
    Archived {
        uploaded_count: usize,
        deleted_count: usize,
        freed_bytes: u64,
    },
    Held(ArchiveHold),
}

#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    pub restored_names: Vec<String>,
    /// Already there with the recorded MD5.
    pub kept_names: Vec<String>,
    /// Stems `call.mp4` holds in its mix, not here and not on Drive on their own.
    pub mixed_only_names: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error(transparent)]
    Drive(#[from] DriveError),
    #[error(transparent)]
    Finalize(#[from] FinalizeError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Record(#[from] DriveRecordError),
    #[error(transparent)]
    Flac(#[from] FlacError),
    #[error("could not read or write {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{} does not decode to the stem it was encoded from: MD5 {actual}, expected {expected}; nothing was uploaded or deleted",
        path.display()
    )]
    RoundTripMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error(
        "the stem rebuilt from Drive for {} differs from the original: MD5 {actual}, expected {expected}; it was not written",
        path.display()
    )]
    RestoreMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error(
        "{} changed after its upload: MD5 {actual}, expected {expected}; nothing was deleted",
        path.display()
    )]
    LocalChanged {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("call folder {} has no drive.json, so nothing of it is on Drive", folder.display())]
    NotArchived { folder: PathBuf },
    #[error("drive.json of {} lists mixed stems but no call.mp4 upload", folder.display())]
    NoRecordedVideo { folder: PathBuf },
    #[error("drive.json names {name:?}, which is not a call file")]
    UnknownRecordedFile { name: String },
    #[error("the call was archived to the Drive of {recorded}, but {signed_in} is signed in")]
    AccountMismatch { recorded: String, signed_in: String },
    #[error(
        "no call folder {name:?} in {}; give a call folder name or a meeting ID",
        calls_dir.display()
    )]
    UnknownCallFolder { name: String, calls_dir: PathBuf },
    #[error("meeting {0} has no call folder on this machine")]
    NoCallFolder(MeetingId),
    #[error(
        "a capture, finalize, purge or another archive run holds {}; try again later",
        folder.display()
    )]
    Busy { folder: PathBuf },
    #[error(
        "{} is still uploading, so nothing of it was deleted; there is nothing to restore",
        folder.display()
    )]
    StillUploading { folder: PathBuf },
    #[error(
        "{} differs from its Drive copy: MD5 {actual}, recorded {expected}; move it away to restore it",
        path.display()
    )]
    LocalDiffers {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("{} is not an open-call list: {message}", path.display())]
    OpenCallsUnreadable { path: PathBuf, message: String },
}

impl ArchiveError {
    /// Whether the same run may work later with nothing changed: a network fault or a busy
    /// folder. Local media is untouched either way.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Drive(error) => error.is_retryable(),
            Self::Busy { .. } => true,
            Self::Finalize(_)
            | Self::Store(_)
            | Self::Record(_)
            | Self::Flac(_)
            | Self::Io { .. }
            | Self::RoundTripMismatch { .. }
            | Self::RestoreMismatch { .. }
            | Self::LocalChanged { .. }
            | Self::NotArchived { .. }
            | Self::NoRecordedVideo { .. }
            | Self::UnknownRecordedFile { .. }
            | Self::AccountMismatch { .. }
            | Self::UnknownCallFolder { .. }
            | Self::NoCallFolder(_)
            | Self::StillUploading { .. }
            | Self::LocalDiffers { .. }
            | Self::OpenCallsUnreadable { .. } => false,
        }
    }

    /// Whether archiving stops for every folder until the user acts: the Drive is full or the
    /// account must sign in again.
    #[must_use]
    pub fn stops_archiving(&self) -> bool {
        matches!(
            self,
            Self::Drive(DriveError::QuotaExceeded { .. } | DriveError::SignInAgain { .. })
        )
    }
}

/// One Drive copy confirmed before the locks, and the local file it stands for: `call.mp4`, or
/// the WAV a FLAC was encoded from.
struct ArchivedCopy {
    local_name: String,
    upload: VerifiedUpload,
}

/// What the confirmations of a folder's recorded copies found.
enum CopyCheck {
    Confirmed(Vec<ArchivedCopy>),
    /// A copy is gone, in the trash or not as uploaded, and its local file is still here, so
    /// that file can go up again.
    Lost {
        local_name: String,
        error: DriveError,
    },
}

/// What a run does next with a folder, by its record.
enum ArchiveStep {
    Upload {
        plan: ArchivePlan,
        record: Option<DriveRecord>,
    },
    FinishDeletion {
        record: DriveRecord,
        owed: OwedDeletion,
    },
    Held(ArchiveHold),
}

/// Every meeting of the job store, read once for a run over many call folders.
pub struct CallMeetings {
    meetings: Vec<Meeting>,
}

impl CallMeetings {
    /// The meetings whose call folder is `folder_name`.
    fn of_folder(&self, folder_name: &str) -> Vec<&Meeting> {
        self.meetings
            .iter()
            .filter(|meeting| call_folder_name(meeting).as_deref() == Some(folder_name))
            .collect()
    }
}

/// What an archive run shares across its call folders.
pub struct ArchiveRun<'a> {
    pub drive: &'a DriveClient,
    /// The Drive folder every archived call goes below.
    pub root_folder: &'a str,
    /// The folders of calls the app holds open.
    pub open_call_folders: &'a [PathBuf],
    /// Read once for the run; each deletion reads the job store again under the folder's locks.
    pub call_meetings: &'a CallMeetings,
    /// Asked right before each deletion: `false` once the archive is off or its account changed.
    pub is_archive_on: &'a dyn Fn() -> bool,
}

impl Pipeline {
    /// The app's calls folder.
    #[must_use]
    pub fn calls_dir(&self) -> PathBuf {
        self.app_dirs.calls_dir()
    }

    /// Every meeting of the job store, for `archive_readiness` and `ArchiveRun`.
    ///
    /// # Errors
    ///
    /// Returns `ArchiveError::Store` when the job store cannot be read.
    pub fn call_meetings(&self) -> Result<CallMeetings, ArchiveError> {
        Ok(CallMeetings {
            meetings: self.store.recent(u32::MAX)?,
        })
    }

    /// The call folders the app holds open, from the list it keeps in the data folder. A
    /// missing list means none.
    ///
    /// # Errors
    ///
    /// Returns `ArchiveError` when the list cannot be read or is not one.
    pub fn open_call_folders(&self) -> Result<Vec<PathBuf>, ArchiveError> {
        open_calls::OpenCallsFile::new(&self.app_dirs).read()
    }

    /// The call folder a meeting ID or a call folder name names.
    ///
    /// # Errors
    ///
    /// Returns `ArchiveError::UnknownCallFolder` or `ArchiveError::NoCallFolder` when there is
    /// none, or `ArchiveError::Store` when the meeting cannot be read.
    pub fn resolve_call_folder(&self, folder_or_meeting: &str) -> Result<PathBuf, ArchiveError> {
        let calls_dir = self.calls_dir();
        if let Ok(id) = folder_or_meeting.parse::<i64>() {
            let id = MeetingId::new(id);
            let meeting = self.store.get(id)?;
            return call_folder_name(&meeting)
                .map(|name| calls_dir.join(name))
                .filter(|folder| folder.is_dir())
                .ok_or(ArchiveError::NoCallFolder(id));
        }
        let unknown = || ArchiveError::UnknownCallFolder {
            name: folder_or_meeting.to_owned(),
            calls_dir: calls_dir.clone(),
        };
        // The same check a media link gets, so a name never reaches outside the calls folder.
        let link = MediaLink::new(folder_or_meeting, MediaTarget::Folder).map_err(|_| unknown())?;
        let folder = link.folder_in(&calls_dir);
        if folder.is_dir() {
            Ok(folder)
        } else {
            Err(unknown())
        }
    }

    /// Whether `folder` may be archived now, and what the run would upload and delete.
    /// `open_call_folders` are the folders of calls the app holds open.
    ///
    /// # Errors
    ///
    /// Returns `ArchiveError` when the folder, its record or its stems cannot be read.
    pub fn archive_readiness(
        &self,
        folder: &Path,
        open_call_folders: &[PathBuf],
        call_meetings: &CallMeetings,
    ) -> Result<ArchiveReadiness, ArchiveError> {
        Ok(
            match self.next_step(folder, open_call_folders, call_meetings)? {
                ArchiveStep::Upload { plan, .. } => ArchiveReadiness::Ready(plan),
                ArchiveStep::FinishDeletion { owed, .. } => ArchiveReadiness::DeletionOwed(owed),
                ArchiveStep::Held(hold) => ArchiveReadiness::Held(hold),
            },
        )
    }

    fn next_step(
        &self,
        folder: &Path,
        open_call_folders: &[PathBuf],
        call_meetings: &CallMeetings,
    ) -> Result<ArchiveStep, ArchiveError> {
        let record = match DriveRecord::read(folder)? {
            Some(record) => {
                if let Some(hold) = restored_hold(&record, Timestamp::now()) {
                    return Ok(ArchiveStep::Held(hold));
                }
                match record.state {
                    ArchiveState::Uploading => Some(record),
                    ArchiveState::Uploaded => {
                        let owed = owed_deletion(folder, &record)?;
                        return Ok(ArchiveStep::FinishDeletion { record, owed });
                    }
                    ArchiveState::MediaDeleted { .. } => {
                        return Ok(ArchiveStep::Held(ArchiveHold::Folder(
                            call_finalize::ArchiveHold::AlreadyArchived,
                        )));
                    }
                }
            }
            None => None,
        };
        Ok(
            match self.plan_archive(folder, open_call_folders, call_meetings)? {
                Ok(plan) => ArchiveStep::Upload { plan, record },
                Err(hold) => ArchiveStep::Held(hold),
            },
        )
    }

    fn plan_archive(
        &self,
        folder: &Path,
        open_call_folders: &[PathBuf],
        call_meetings: &CallMeetings,
    ) -> Result<Result<ArchivePlan, ArchiveHold>, ArchiveError> {
        let calls_dir = self.calls_dir();
        let archivable = match call_finalize::archive_readiness(
            &calls_dir,
            folder,
            open_call_folders,
            SystemTime::now(),
        )? {
            Ok(archivable) => archivable,
            Err(hold) => return Ok(Err(ArchiveHold::Folder(hold))),
        };
        let folder_name = folder_name_of(folder);
        let meetings = call_meetings.of_folder(&folder_name);
        let Some(first_meeting) = meetings.iter().min_by_key(|meeting| meeting.started_at) else {
            return Ok(Err(ArchiveHold::NoMeeting));
        };
        if let Some(hold) = meetings_hold(folder, &meetings) {
            return Ok(Err(hold));
        }
        let stem_names = stem_names(folder)?;
        let (upload_names, deleted_names) = match &archivable.kind {
            ArchiveKind::Video { mixed_stem_names } => {
                let mut upload_names = vec![VIDEO_FILE_NAME.to_owned()];
                upload_names.extend(
                    stem_names
                        .iter()
                        .filter(|name| !mixed_stem_names.contains(*name))
                        .cloned(),
                );
                let mut deleted_names = stem_names;
                deleted_names.push(VIDEO_FILE_NAME.to_owned());
                (upload_names, deleted_names)
            }
            ArchiveKind::Stems => {
                if stem_names.is_empty() {
                    return Ok(Err(ArchiveHold::NoStems));
                }
                (stem_names.clone(), stem_names)
            }
        };
        for stem_name in upload_names
            .iter()
            .filter(|name| file_kind_of(name) == ArchivedFileKind::Stem)
        {
            match flac::stem_data_bytes(&folder.join(stem_name)) {
                Ok(_) => {}
                Err(FlacError::NotAStem { reason, .. }) => {
                    return Ok(Err(ArchiveHold::StemNotAsWritten {
                        file_name: stem_name.clone(),
                        reason,
                    }));
                }
                Err(error) => return Err(error.into()),
            }
        }
        let mut freed_bytes = 0;
        for name in &deleted_names {
            freed_bytes += regular_file_bytes(&folder.join(name))?.unwrap_or_default();
        }
        Ok(Ok(ArchivePlan {
            kind: archivable.kind,
            call_ended_at: archivable.call_ended_at,
            upload_names,
            deleted_names,
            freed_bytes,
            drive_folder_names: self.drive_folder_names(first_meeting, &folder_name),
        }))
    }

    /// Year, month, and `<YYYY-MM-DD HHMM> <title>`, by the call start in the notes' time zone,
    /// so two calls of one title on one day get folders of their own. A call with no title takes
    /// its folder name.
    fn drive_folder_names(&self, meeting: &Meeting, folder_name: &str) -> Vec<String> {
        let started_at = meeting.started_at.to_zoned(self.settings.time_zone.clone());
        let title = meeting
            .title
            .as_deref()
            .map(drive_name_of)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| folder_name.to_owned());
        vec![
            started_at.strftime("%Y").to_string(),
            started_at.strftime("%m").to_string(),
            format!("{} {title}", started_at.strftime("%Y-%m-%d %H%M")),
        ]
    }

    /// Uploads the folder's media to `<root_folder>/<YYYY>/<MM>/<YYYY-MM-DD HHMM> <title>/` on
    /// the run's Drive, then deletes the local copies once every Drive copy is confirmed. A
    /// folder whose uploads are all recorded only has its deletion finished. Holds
    /// `archive.lock` throughout, so one archive or restore runs on a folder at a time.
    ///
    /// # Errors
    ///
    /// Returns `ArchiveError` when an upload, the record or a local file fails. `drive.json`
    /// keeps what was uploaded, and only files it records as on Drive were deleted.
    pub fn archive_folder(
        &self,
        run: &ArchiveRun<'_>,
        folder: &Path,
    ) -> Result<ArchiveOutcome, ArchiveError> {
        let _span = tracing::info_span!(
            "archive.folder",
            file.directory = %folder.display(),
            mc.archive.kind = tracing::field::Empty,
        )
        .entered();
        let Some(_archive_lock) = take_archive_lock(folder)? else {
            return Ok(held(ArchiveHold::ArchiveRunning));
        };
        let (plan, record) =
            match self.next_step(folder, run.open_call_folders, run.call_meetings)? {
                ArchiveStep::Upload { plan, record } => (plan, record),
                ArchiveStep::FinishDeletion { record, .. } => {
                    return self.finish_deletion(run, folder, record);
                }
                ArchiveStep::Held(hold) => return Ok(held(hold)),
            };
        tracing::Span::current().record("mc.archive.kind", plan.kind.as_str());
        let mut record = match start_record(run.drive, run.root_folder, folder, &plan, record)? {
            Ok(record) => record,
            Err(hold) => return Ok(held(hold)),
        };
        let folder_id = FolderId::new(record.folder_id.clone());
        std::fs::create_dir_all(folder.join(STAGING_DIR_NAME))
            .map_err(io_error(&folder.join(STAGING_DIR_NAME)))?;
        for local_name in &plan.upload_names {
            upload_one(run.drive, &folder_id, folder, local_name, &mut record)?;
        }
        record.mixed_stem_names = match &plan.kind {
            ArchiveKind::Video { mixed_stem_names } => mixed_stem_names.iter().cloned().collect(),
            ArchiveKind::Stems => Vec::new(),
        };
        // Each copy again, this run's too: one may have gone to the trash since its upload.
        let copies = match confirm_copies(run.drive, folder, &record)? {
            CopyCheck::Confirmed(copies) => copies,
            CopyCheck::Lost { error, .. } => return Err(error.into()),
        };
        if !(run.is_archive_on)() {
            return Ok(held(ArchiveHold::ArchiveTurnedOff));
        }

        let Some(_locks) = call_finalize::take_media_locks(folder)? else {
            return Ok(held(ArchiveHold::Folder(call_finalize::ArchiveHold::Busy)));
        };
        // The whole plan again under the locks: during the uploads a call may have resumed, a
        // meeting gone back to work, or finalize rewritten call.mp4.
        let call_meetings = self.call_meetings()?;
        match self.plan_archive(folder, run.open_call_folders, &call_meetings)? {
            Ok(again)
                if again.upload_names == plan.upload_names
                    && again.deleted_names == plan.deleted_names => {}
            Ok(_) => return Ok(held(ArchiveHold::PlanChanged)),
            Err(hold) => return Ok(held(hold)),
        }
        if !saved_replies_match_stems(folder) {
            return Ok(held(ArchiveHold::RepliesStale));
        }
        let deletions = match local_deletions(folder, &record, &copies) {
            Ok(Ok(deletions)) => deletions,
            Ok(Err(hold)) => return Ok(held(hold)),
            // The plan held under the locks, so the file changed after its upload. The next run
            // finds its entry superseded and uploads it again.
            Err(error @ ArchiveError::LocalChanged { .. }) => {
                tracing::warn!(exception.message = %error, "archive.planned_file_changed");
                return Ok(held(ArchiveHold::PlanChanged));
            }
            Err(error) => return Err(error),
        };
        record.state = ArchiveState::Uploaded;
        record.write(folder)?;
        let deleted = delete_covered_files(folder, &deletions)?;
        record.state = ArchiveState::MediaDeleted {
            deleted_at: Timestamp::now(),
        };
        record.write(folder)?;
        remove_staging_dir(folder);
        let uploaded_count = plan.upload_names.len();
        tracing::info!(
            mc.archive.uploaded_count = uploaded_count,
            mc.archive.deleted_count = deleted.file_count,
            mc.archive.freed_bytes = deleted.bytes,
            "archive.folder_archived"
        );
        Ok(ArchiveOutcome::Archived {
            uploaded_count,
            deleted_count: deleted.file_count,
            freed_bytes: deleted.bytes,
        })
    }

    /// Downloads the folder's archived media back into it. A stem is rebuilt from its FLAC and
    /// written only when its MD5 matches the original WAV's. A file already here is kept when
    /// its MD5 is the recorded one, and refused otherwise. The record keeps the Drive copies and
    /// the restore time, so the run after `RESTORED_MEDIA_KEEP` deletes the files again without
    /// an upload. Stems `call.mp4` mixed were never uploaded, so they stay gone.
    ///
    /// # Errors
    ///
    /// Returns `ArchiveError` when the folder has no record or is still uploading, another
    /// account is signed in, a local file differs from its Drive copy, a download or a check
    /// fails, a Drive copy is in the trash, or the folder is busy.
    pub fn restore_folder(
        &self,
        drive: &DriveClient,
        folder: &Path,
    ) -> Result<RestoreOutcome, ArchiveError> {
        let _span =
            tracing::info_span!("archive.restore", file.directory = %folder.display()).entered();
        let busy = || ArchiveError::Busy {
            folder: folder.to_owned(),
        };
        let Some(_archive_lock) = take_archive_lock(folder)? else {
            return Err(busy());
        };
        let Some(mut record) = DriveRecord::read(folder)? else {
            return Err(ArchiveError::NotArchived {
                folder: folder.to_owned(),
            });
        };
        match record.state {
            ArchiveState::Uploaded | ArchiveState::MediaDeleted { .. } => {}
            ArchiveState::Uploading => {
                return Err(ArchiveError::StillUploading {
                    folder: folder.to_owned(),
                });
            }
        }
        if record.account != drive.account() {
            return Err(ArchiveError::AccountMismatch {
                recorded: record.account,
                signed_in: drive.account().to_owned(),
            });
        }
        let Some(_locks) = call_finalize::take_media_locks(folder)? else {
            return Err(busy());
        };
        // Every local file is checked before the first download, so a refusal writes nothing.
        let mut kept_names = Vec::new();
        let mut missing_files = Vec::new();
        for file in &record.files {
            check_recorded_name(file)?;
            let local_path = folder.join(&file.local_name);
            if regular_file_bytes(&local_path)?.is_none() {
                missing_files.push(file.clone());
                continue;
            }
            let actual = crate::drive::local_md5(&local_path)?;
            if actual != file.local_md5 {
                return Err(ArchiveError::LocalDiffers {
                    path: local_path,
                    expected: file.local_md5.clone(),
                    actual,
                });
            }
            kept_names.push(file.local_name.clone());
        }
        // Recorded before the first file lands, so a restore cut short leaves files the archive
        // owns and deletes again after `RESTORED_MEDIA_KEEP`.
        record.restored_at = Some(Timestamp::now());
        record.state = ArchiveState::Uploaded;
        record.write(folder)?;
        let staging_dir = folder.join(STAGING_DIR_NAME);
        std::fs::create_dir_all(&staging_dir).map_err(io_error(&staging_dir))?;
        let mut restored_names = Vec::new();
        for file in &missing_files {
            restore_file(drive, folder, &staging_dir, file)?;
            restored_names.push(file.local_name.clone());
        }
        remove_staging_dir(folder);
        let mixed_only_names = record
            .mixed_stem_names
            .iter()
            .filter(|name| !folder.join(name).exists())
            .cloned()
            .collect();
        Ok(RestoreOutcome {
            restored_names,
            kept_names,
            mixed_only_names,
        })
    }

    /// The deletion a folder in `uploaded` still owes. Each Drive copy is confirmed before the
    /// locks; under them only local state is checked.
    fn finish_deletion(
        &self,
        run: &ArchiveRun<'_>,
        folder: &Path,
        mut record: DriveRecord,
    ) -> Result<ArchiveOutcome, ArchiveError> {
        if record.account != run.drive.account() {
            return Ok(held(ArchiveHold::OtherAccount {
                account: record.account,
            }));
        }
        let copies = match confirm_copies(run.drive, folder, &record)? {
            CopyCheck::Confirmed(copies) => copies,
            CopyCheck::Lost { local_name, error } => {
                return reopen_upload(folder, record, local_name, &error);
            }
        };
        if !(run.is_archive_on)() {
            return Ok(held(ArchiveHold::ArchiveTurnedOff));
        }
        let Some(_locks) = call_finalize::take_media_locks(folder)? else {
            return Ok(held(ArchiveHold::Folder(call_finalize::ArchiveHold::Busy)));
        };
        // A rerun, or `mc transcribe` after a restore, reads the stems with no lock once its
        // meeting has left `Written`. Checked under the locks, so that meeting holds the deletion.
        let call_meetings = self.call_meetings()?;
        if let Some(hold) = meetings_hold(folder, &call_meetings.of_folder(&folder_name_of(folder)))
        {
            return Ok(held(hold));
        }
        if !saved_replies_match_stems(folder) {
            return Ok(held(ArchiveHold::RepliesStale));
        }
        let deletions = match local_deletions(folder, &record, &copies)? {
            Ok(deletions) => deletions,
            Err(hold) => return Ok(held(hold)),
        };
        let deleted = delete_covered_files(folder, &deletions)?;
        record.state = ArchiveState::MediaDeleted {
            deleted_at: Timestamp::now(),
        };
        record.write(folder)?;
        remove_staging_dir(folder);
        tracing::info!(
            mc.archive.deleted_count = deleted.file_count,
            mc.archive.freed_bytes = deleted.bytes,
            "archive.deletion_finished"
        );
        Ok(ArchiveOutcome::Archived {
            uploaded_count: 0,
            deleted_count: deleted.file_count,
            freed_bytes: deleted.bytes,
        })
    }
}

/// Downloads one recorded file and writes it into the call folder once its MD5 is the one
/// recorded: `call.mp4` as downloaded, a stem rebuilt from its FLAC.
fn restore_file(
    drive: &DriveClient,
    folder: &Path,
    staging_dir: &Path,
    file: &ArchivedFile,
) -> Result<(), ArchiveError> {
    let local_path = folder.join(&file.local_name);
    let staged_path = staging_dir.join(&file.drive_name);
    let downloaded = drive.download(&FileId::new(file.file_id.clone()), &staged_path)?;
    // `download` checks the bytes against Drive's MD5; this ties them to the upload.
    let downloaded_md5 = downloaded.md5_checksum.unwrap_or_default();
    if downloaded_md5 != file.md5 {
        return Err(ArchiveError::RestoreMismatch {
            path: local_path,
            expected: file.md5.clone(),
            actual: downloaded_md5,
        });
    }
    match file.kind {
        ArchivedFileKind::Video => {
            std::fs::rename(&staged_path, &local_path).map_err(io_error(&local_path))?;
            call_folder::sync_folder(folder).map_err(io_error(folder))?;
        }
        ArchivedFileKind::Stem => {
            match flac::restore_stem(&staged_path, &local_path, &file.local_md5)? {
                RestoredStem::Written => {}
                RestoredStem::Mismatch { rebuilt_md5 } => {
                    return Err(ArchiveError::RestoreMismatch {
                        path: local_path,
                        expected: file.local_md5.clone(),
                        actual: rebuilt_md5,
                    });
                }
            }
            std::fs::remove_file(&staged_path).map_err(io_error(&staged_path))?;
        }
    }
    tracing::info!(
        file.path = %local_path.display(),
        mc.drive.file_id = %file.file_id,
        "archive.file_restored"
    );
    Ok(())
}

/// Drops the entry of a Drive copy that was lost while its local file is still here, and puts
/// the record back to `uploading`, so the next run's upload path sends the file again. With
/// `call.mp4`'s entry go the mixed stems it covered: the next plan names them again.
fn reopen_upload(
    folder: &Path,
    mut record: DriveRecord,
    local_name: String,
    error: &DriveError,
) -> Result<ArchiveOutcome, ArchiveError> {
    let Some(lost) = record
        .files
        .iter()
        .find(|file| file.local_name == local_name)
        .cloned()
    else {
        return Err(ArchiveError::UnknownRecordedFile { name: local_name });
    };
    tracing::warn!(
        file.name = %local_name,
        mc.drive.file_id = %lost.file_id,
        exception.message = %error,
        "archive.drive_copy_lost"
    );
    record.files.retain(|file| file.local_name != local_name);
    match lost.kind {
        ArchivedFileKind::Video => record.mixed_stem_names.clear(),
        ArchivedFileKind::Stem => {}
    }
    record.state = ArchiveState::Uploading;
    record.write(folder)?;
    Ok(held(ArchiveHold::DriveCopyLost {
        file_name: local_name,
    }))
}

/// The outcome of a folder that keeps its media for now, logged.
fn held(hold: ArchiveHold) -> ArchiveOutcome {
    tracing::info!(mc.archive.hold = hold.as_str(), "archive.folder_held");
    ArchiveOutcome::Held(hold)
}

/// The folder's `archive.lock`, taken with no wait, or `None` when another run holds it.
fn take_archive_lock(folder: &Path) -> Result<Option<call_folder::FolderLock>, ArchiveError> {
    call_folder::FolderLock::try_take(folder, ARCHIVE_LOCK_FILE_NAME)
        .map_err(io_error(&folder.join(ARCHIVE_LOCK_FILE_NAME)))
}

/// The hold of a folder `mc restore` brought back less than `RESTORED_MEDIA_KEEP` ago.
fn restored_hold(record: &DriveRecord, now: Timestamp) -> Option<ArchiveHold> {
    let deletes_after = record
        .restored_at?
        .checked_add(RESTORED_MEDIA_KEEP)
        .unwrap_or(Timestamp::MAX);
    (now < deletes_after).then_some(ArchiveHold::RestoredRecently { deletes_after })
}

/// Why the meetings of a call keep its stems: one has a run ahead that may read them, or the
/// note holds the captions while speech-to-text is owed.
fn meetings_hold(folder: &Path, meetings: &[&Meeting]) -> Option<ArchiveHold> {
    for meeting in meetings {
        // A pending transcript, an extraction retry, a network wait and a resume all keep a run
        // ahead that may read the stems.
        let state = meeting.state();
        match state {
            JobState::Written { .. } => {}
            JobState::Captured
            | JobState::Transcribed
            | JobState::Matched
            | JobState::Extracted
            | JobState::ExtractionPending { .. }
            | JobState::NetworkWait { .. }
            | JobState::Resuming { .. }
            | JobState::Failed { .. } => {
                return Some(ArchiveHold::MeetingNotWritten {
                    meeting_id: meeting.id,
                    state: state.label(),
                });
            }
        }
    }
    match saved_transcript_standing(folder) {
        SavedTranscriptStanding::Settled => None,
        SavedTranscriptStanding::Pending => Some(ArchiveHold::TranscriptPending),
        SavedTranscriptStanding::Unreadable => Some(ArchiveHold::TranscriptChoiceUnreadable),
    }
}

fn folder_name_of(folder: &Path) -> String {
    folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The recorded files of a folder in `uploaded` that are still there.
fn owed_deletion(folder: &Path, record: &DriveRecord) -> Result<OwedDeletion, ArchiveError> {
    let mut deleted_names = Vec::new();
    let mut freed_bytes = 0;
    for name in recorded_local_names(folder, record)? {
        if let Some(bytes) = regular_file_bytes(&folder.join(&name))? {
            deleted_names.push(name);
            freed_bytes += bytes;
        }
    }
    Ok(OwedDeletion {
        deleted_names,
        freed_bytes,
    })
}

/// Every local file the record covers: its uploads and the stems `call.mp4` mixed, checked as
/// call files, with `call.mp4` last.
fn recorded_local_names(folder: &Path, record: &DriveRecord) -> Result<Vec<String>, ArchiveError> {
    let mut names = Vec::new();
    for file in &record.files {
        check_recorded_name(file)?;
        names.push(file.local_name.clone());
    }
    for name in &record.mixed_stem_names {
        if !is_stem_file_name(name) {
            return Err(ArchiveError::UnknownRecordedFile { name: name.clone() });
        }
        if !record
            .files
            .iter()
            .any(|file| file.kind == ArchivedFileKind::Video)
        {
            return Err(ArchiveError::NoRecordedVideo {
                folder: folder.to_owned(),
            });
        }
        names.push(name.clone());
    }
    names.sort();
    names.dedup();
    names.sort_by_key(|name| name == VIDEO_FILE_NAME);
    Ok(names)
}

/// The folder's record to go on with, or a new one with its Drive folder made. A record of
/// another account starts over, unless one of its files is no longer here: that Drive copy is
/// then the only one, and its entry must stay, so the folder waits for that account.
fn start_record(
    drive: &DriveClient,
    root_folder: &str,
    folder: &Path,
    plan: &ArchivePlan,
    record: Option<DriveRecord>,
) -> Result<Result<DriveRecord, ArchiveHold>, ArchiveError> {
    if let Some(record) = record {
        if record.account == drive.account() {
            return resume_record(drive, folder, record).map(Ok);
        }
        for file in &record.files {
            if regular_file_bytes(&folder.join(&file.local_name))?.is_none() {
                return Ok(Err(ArchiveHold::OtherAccount {
                    account: record.account,
                }));
            }
        }
        let orphaned_file_ids: Vec<&str> = record
            .files
            .iter()
            .map(|file| file.file_id.as_str())
            .collect();
        tracing::warn!(
            mc.drive.file_ids = %orphaned_file_ids.join(","),
            "archive.record_entries_orphaned"
        );
    }
    let folder_path: Vec<String> = std::iter::once(root_folder.to_owned())
        .chain(plan.drive_folder_names.iter().cloned())
        .collect();
    let names: Vec<&str> = folder_path.iter().map(String::as_str).collect();
    let folder_id = drive.ensure_folder_path(&names)?;
    let record = DriveRecord {
        version: RecordVersion,
        account: drive.account().to_owned(),
        folder_id: folder_id.as_str().to_owned(),
        folder_path,
        state: ArchiveState::Uploading,
        files: Vec::new(),
        mixed_stem_names: Vec::new(),
        pending_sessions: std::collections::BTreeMap::new(),
        restored_at: None,
    };
    record.write(folder)?;
    Ok(Ok(record))
}

/// The record of a cut-short run, with its Drive folder made again when it is gone or in the
/// trash. Its uploads went with that folder, so their entries and sessions go too; an entry
/// whose local file is no longer here stays, as the only trace of that copy.
fn resume_record(
    drive: &DriveClient,
    folder: &Path,
    mut record: DriveRecord,
) -> Result<DriveRecord, ArchiveError> {
    match drive.file(&FileId::new(record.folder_id.clone())) {
        Ok(remote) if !remote.is_trashed => return Ok(record),
        Ok(_) | Err(DriveError::NotFound { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    let names: Vec<&str> = record.folder_path.iter().map(String::as_str).collect();
    let folder_id = drive.ensure_folder_path(&names)?;
    let mut dropped_file_ids = Vec::new();
    let mut kept_files = Vec::new();
    for file in std::mem::take(&mut record.files) {
        if regular_file_bytes(&folder.join(&file.local_name))?.is_some() {
            dropped_file_ids.push(file.file_id);
        } else {
            kept_files.push(file);
        }
    }
    tracing::warn!(
        mc.drive.file_ids = %dropped_file_ids.join(","),
        "archive.drive_folder_made_again"
    );
    record.files = kept_files;
    record.pending_sessions.clear();
    folder_id.as_str().clone_into(&mut record.folder_id);
    record.write(folder)?;
    Ok(record)
}

/// Uploads one file of the plan, or confirms the upload a cut-short run recorded. A stem is
/// encoded to FLAC first and decoded back to prove it rebuilds the WAV byte for byte.
fn upload_one(
    drive: &DriveClient,
    folder_id: &FolderId,
    folder: &Path,
    local_name: &str,
    record: &mut DriveRecord,
) -> Result<(), ArchiveError> {
    let local_path = folder.join(local_name);
    let kind = file_kind_of(local_name);
    let drive_name = drive_name_for(kind, local_name);
    let local_md5 = crate::drive::local_md5(&local_path)?;
    if confirm_recorded(drive, local_name, kind, &local_md5, record)? {
        return Ok(());
    }

    let upload_path = match kind {
        ArchivedFileKind::Video => local_path.clone(),
        ArchivedFileKind::Stem => {
            let flac_path = folder.join(STAGING_DIR_NAME).join(&drive_name);
            // A FLAC left by a cut-short run keeps its upload session, whose bytes are bound to
            // the file as it is.
            if !flac_path.is_file() || !decodes_to(&flac_path, &local_md5)? {
                record.pending_sessions.remove(&drive_name);
                flac::encode_stem(&local_path, &flac_path)?;
                let mut sink = Md5Writer::default();
                let rebuilt_md5 = flac::decode_stem(&flac_path, &mut sink, &local_path)?;
                if rebuilt_md5 != local_md5 {
                    return Err(ArchiveError::RoundTripMismatch {
                        path: local_path,
                        expected: local_md5,
                        actual: rebuilt_md5,
                    });
                }
            }
            flac_path
        }
    };
    let upload_md5 = crate::drive::local_md5(&upload_path)?;
    let saved_session = record.pending_sessions.get(&drive_name).cloned();
    let upload = drive.upload(&upload_path, folder_id, saved_session, |session| {
        record
            .pending_sessions
            .insert(drive_name.clone(), session.clone());
        if let Err(error) = record.write(folder) {
            // The upload goes on; only a resume after a quit is lost.
            tracing::warn!(exception.message = %error, "archive.session_not_saved");
        }
    })?;
    // The upload's MD5 is read after it finished; the FLAC checked above must be that file.
    if upload.md5() != upload_md5 {
        return Err(ArchiveError::LocalChanged {
            path: upload_path,
            expected: upload_md5,
            actual: upload.md5().to_owned(),
        });
    }
    record.pending_sessions.remove(&drive_name);
    let uploaded = upload.file();
    record.files.push(ArchivedFile {
        local_name: local_name.to_owned(),
        drive_name: drive_name.clone(),
        kind,
        file_id: uploaded.id.as_str().to_owned(),
        size_bytes: uploaded.size_bytes.unwrap_or_default(),
        md5: upload.md5().to_owned(),
        local_md5,
        web_view_link: uploaded.web_view_link.clone(),
        uploaded_at: Timestamp::now(),
    });
    record.write(folder)?;
    match kind {
        ArchivedFileKind::Video => {}
        // Recorded and verified, so the FLAC is no longer needed to resume anything.
        ArchivedFileKind::Stem => {
            std::fs::remove_file(&upload_path).map_err(io_error(&upload_path))?;
        }
    }
    tracing::info!(
        file.name = local_name,
        file.size = uploaded.size_bytes.unwrap_or_default(),
        mc.drive.file_id = %uploaded.id,
        "archive.file_uploaded"
    );
    Ok(())
}

/// Whether a cut-short run recorded this file's upload and Drive still has it. An entry whose
/// local file changed, or whose Drive copy is gone or in the trash, is dropped: the local file
/// is here, so the entry is not its only copy.
fn confirm_recorded(
    drive: &DriveClient,
    local_name: &str,
    kind: ArchivedFileKind,
    local_md5: &str,
    record: &mut DriveRecord,
) -> Result<bool, ArchiveError> {
    if let Some(recorded) = record
        .files
        .iter()
        .find(|file| file.local_name == local_name)
        .cloned()
    {
        if recorded.local_md5 == local_md5 && recorded.kind == kind {
            match drive.confirm_upload(
                &FileId::new(recorded.file_id.clone()),
                recorded.size_bytes,
                &recorded.md5,
            ) {
                Ok(_) => return Ok(true),
                Err(
                    error @ (DriveError::NotFound { .. }
                    | DriveError::Trashed { .. }
                    | DriveError::VerificationFailed { .. }),
                ) => {
                    tracing::warn!(
                        mc.drive.file_id = %recorded.file_id,
                        exception.message = %error,
                        "archive.recorded_upload_gone"
                    );
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            tracing::warn!(
                file.name = local_name,
                mc.drive.file_id = %recorded.file_id,
                "archive.record_entry_superseded"
            );
        }
        record.files.retain(|file| file.local_name != local_name);
    }
    Ok(false)
}

/// Whether the FLAC decodes to a stem of this MD5. A FLAC that does not decode counts as not.
fn decodes_to(flac_path: &Path, wav_md5: &str) -> Result<bool, ArchiveError> {
    let mut sink = Md5Writer::default();
    match flac::decode_stem(flac_path, &mut sink, flac_path) {
        Ok(rebuilt_md5) => Ok(rebuilt_md5 == wav_md5),
        Err(FlacError::Decode { message, .. }) => {
            tracing::warn!(file.path = %flac_path.display(), exception.message = %message, "archive.staged_flac_unreadable");
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

/// Hashes what is written to it and keeps nothing.
#[derive(Default)]
struct Md5Writer {
    hasher: md5::Md5,
}

impl std::io::Write for Md5Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        md5::Digest::update(&mut self.hasher, bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Confirms against Drive, one request each, every recorded copy that a local file still here
/// needs: its own, or for a mixed stem that of `call.mp4`. Run before the folder's locks, so no
/// request waits under them. A lost copy whose own local file is gone fails: that copy was the
/// only one.
fn confirm_copies(
    drive: &DriveClient,
    folder: &Path,
    record: &DriveRecord,
) -> Result<CopyCheck, ArchiveError> {
    let mut needed_files: Vec<&ArchivedFile> = Vec::new();
    for name in recorded_local_names(folder, record)? {
        if regular_file_bytes(&folder.join(&name))?.is_none() {
            continue;
        }
        let file = covering_file(folder, record, &name)?;
        if !needed_files
            .iter()
            .any(|needed| needed.local_name == file.local_name)
        {
            needed_files.push(file);
        }
    }
    let mut copies = Vec::with_capacity(needed_files.len());
    for file in needed_files {
        let upload = match drive.confirm_upload(
            &FileId::new(file.file_id.clone()),
            file.size_bytes,
            &file.md5,
        ) {
            Ok(upload) => upload,
            Err(
                error @ (DriveError::NotFound { .. }
                | DriveError::Trashed { .. }
                | DriveError::VerificationFailed { .. }),
            ) => {
                if regular_file_bytes(&folder.join(&file.local_name))?.is_some() {
                    return Ok(CopyCheck::Lost {
                        local_name: file.local_name.clone(),
                        error,
                    });
                }
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        };
        copies.push(ArchivedCopy {
            local_name: file.local_name.clone(),
            upload,
        });
    }
    Ok(CopyCheck::Confirmed(copies))
}

/// The recorded upload that holds a local file: its own, or for a mixed stem `call.mp4`'s.
fn covering_file<'a>(
    folder: &Path,
    record: &'a DriveRecord,
    local_name: &str,
) -> Result<&'a ArchivedFile, ArchiveError> {
    if let Some(file) = record
        .files
        .iter()
        .find(|file| file.local_name == local_name)
    {
        return Ok(file);
    }
    record
        .files
        .iter()
        .find(|file| file.kind == ArchivedFileKind::Video)
        .ok_or_else(|| ArchiveError::NoRecordedVideo {
            folder: folder.to_owned(),
        })
}

/// What a deletion removed.
#[derive(Default)]
struct DeletionCount {
    file_count: usize,
    bytes: u64,
}

/// Each local file the record covers that is still here, stems before `call.mp4`, with the
/// copy confirmed before the locks that holds it. Reads only local files: the caller holds the
/// folder's media locks. Each uploaded file's MD5 is checked again. A file with no confirmed
/// copy appeared after the confirmations, so the folder waits for the next run.
fn local_deletions(
    folder: &Path,
    record: &DriveRecord,
    copies: &[ArchivedCopy],
) -> Result<Result<Vec<(PathBuf, VerifiedUpload)>, ArchiveHold>, ArchiveError> {
    let mut deletions = Vec::new();
    for name in recorded_local_names(folder, record)? {
        let path = folder.join(&name);
        if regular_file_bytes(&path)?.is_none() {
            continue;
        }
        let file = covering_file(folder, record, &name)?;
        let Some(copy) = copies
            .iter()
            .find(|copy| copy.local_name == file.local_name)
        else {
            return Ok(Err(ArchiveHold::PlanChanged));
        };
        if file.local_name == name {
            let actual = crate::drive::local_md5(&path)?;
            if actual != file.local_md5 {
                return Err(ArchiveError::LocalChanged {
                    path,
                    expected: file.local_md5.clone(),
                    actual,
                });
            }
            match file.kind {
                ArchivedFileKind::Video if copy.upload.md5() != actual => {
                    return Err(ArchiveError::LocalChanged {
                        path,
                        expected: copy.upload.md5().to_owned(),
                        actual,
                    });
                }
                ArchivedFileKind::Video | ArchivedFileKind::Stem => {}
            }
        }
        deletions.push((path, copy.upload.clone()));
    }
    Ok(Ok(deletions))
}

/// Deletes the files `local_deletions` found, in its order.
fn delete_covered_files(
    folder: &Path,
    deletions: &[(PathBuf, VerifiedUpload)],
) -> Result<DeletionCount, ArchiveError> {
    let mut deleted = DeletionCount::default();
    for (path, covered_by) in deletions {
        deleted.bytes += delete_covered(path, covered_by)?;
        deleted.file_count += 1;
    }
    call_folder::sync_folder(folder).map_err(io_error(folder))?;
    Ok(deleted)
}

/// Deletes one local file whose content `covered_by` holds on Drive. Returns its length.
fn delete_covered(path: &Path, covered_by: &VerifiedUpload) -> Result<u64, ArchiveError> {
    let file_bytes = regular_file_bytes(path)?.unwrap_or_default();
    std::fs::remove_file(path).map_err(io_error(path))?;
    tracing::info!(
        file.path = %path.display(),
        file.size = file_bytes,
        mc.drive.file_id = %covered_by.file().id,
        "archive.file_deleted"
    );
    Ok(file_bytes)
}

/// The length of a regular file, or `None` when nothing is there. Anything else, such as a
/// symlink, is logged and counts as not there, so the archive never follows it.
fn regular_file_bytes(path: &Path) -> Result<Option<u64>, ArchiveError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(Some(metadata.len())),
        Ok(_) => {
            tracing::warn!(file.path = %path.display(), "archive.entry_refused");
            Ok(None)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ArchiveError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// The folder's WAV stems by the names the stem writer gives, in segment order.
fn stem_names(folder: &Path) -> Result<Vec<String>, ArchiveError> {
    let media = CallMedia::scan(folder).map_err(io_error(folder))?;
    let mut names = Vec::new();
    for segment in media.audio_segments {
        for (is_present, side) in [
            (segment.has_mic, call_folder::media::StemSide::Mic),
            (segment.has_remote, call_folder::media::StemSide::Remote),
        ] {
            if is_present {
                names.push(side.file_name(segment.number));
            }
        }
    }
    Ok(names)
}

/// What a planned file goes up as. A plan names only `call.mp4` and stems.
fn file_kind_of(local_name: &str) -> ArchivedFileKind {
    if local_name == VIDEO_FILE_NAME {
        ArchivedFileKind::Video
    } else {
        ArchivedFileKind::Stem
    }
}

/// `call.mp4` keeps its name; `001-mic.wav` goes up as `001-mic.flac`.
fn drive_name_for(kind: ArchivedFileKind, local_name: &str) -> String {
    match kind {
        ArchivedFileKind::Video => local_name.to_owned(),
        ArchivedFileKind::Stem => Path::new(local_name)
            .with_extension("flac")
            .to_string_lossy()
            .into_owned(),
    }
}

/// A recorded name must be one this archive writes, so a damaged record never writes outside
/// the folder or over a manifest.
fn check_recorded_name(file: &ArchivedFile) -> Result<(), ArchiveError> {
    let is_call_file = match file.kind {
        ArchivedFileKind::Video => file.local_name == VIDEO_FILE_NAME,
        ArchivedFileKind::Stem => is_stem_file_name(&file.local_name),
    };
    if is_call_file && file.drive_name == drive_name_for(file.kind, &file.local_name) {
        Ok(())
    } else {
        Err(ArchiveError::UnknownRecordedFile {
            name: file.local_name.clone(),
        })
    }
}

fn is_stem_file_name(name: &str) -> bool {
    let Some((number, _)) = name.split_once('-') else {
        return false;
    };
    let Ok(number) = number.parse::<u32>() else {
        return false;
    };
    [
        call_folder::media::StemSide::Mic,
        call_folder::media::StemSide::Remote,
    ]
    .into_iter()
    .any(|side| side.file_name(number) == name)
}

/// Drive allows any name, but `/` reads as a path in every tool that shows one, and control
/// characters do not show.
fn drive_name_of(title: &str) -> String {
    title
        .chars()
        .filter(|character| *character != '/' && !character.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

fn remove_staging_dir(folder: &Path) {
    let staging_dir = folder.join(STAGING_DIR_NAME);
    match std::fs::remove_dir_all(&staging_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(file.directory = %staging_dir.display(), exception.message = %error, "archive.staging_not_removed");
        }
    }
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> ArchiveError + '_ {
    move |source| ArchiveError::Io {
        path: path.to_owned(),
        source,
    }
}
