//! The files in a call folder that a person opens: the video, the audio stems, and the purge
//! record that says when the video went. The note writer links them, and the app opens a link.
//!
//! A link is `meetingcapture://call/<folder>/<target>`, where the target is `video`, `folder`,
//! `mic/<segment>` or `remote/<segment>`. The link names the folder, not a path, so it works on
//! any machine and after the app's data folder moves. The app resolves it when it is clicked.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// The call's playable file, in its call folder.
pub const VIDEO_FILE_NAME: &str = "call.mp4";

/// In each purged call folder.
pub const PURGE_RECORD_FILE_NAME: &str = "media-purge.json";

/// Raised when the purge record changes in a way an older reader would misread.
pub const PURGE_RECORD_SCHEMA_VERSION: u32 = 1;

/// The URL scheme the app registers for media links.
pub const LINK_SCHEME: &str = "meetingcapture";

const LINK_PREFIX: &str = "meetingcapture://call/";

/// Written by the purge before it deletes anything.
#[derive(Debug, Serialize, Deserialize)]
pub struct PurgeRecord {
    pub schema_version: u32,
    pub purged_at: Timestamp,
    pub call_ended_at: Timestamp,
}

impl PurgeRecord {
    /// The folder's purge record. A missing, unreadable, or newer record counts as none.
    #[must_use]
    pub fn read(folder: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(folder.join(PURGE_RECORD_FILE_NAME)).ok()?;
        serde_json::from_str::<Self>(&text)
            .ok()
            .filter(|record| record.schema_version <= PURGE_RECORD_SCHEMA_VERSION)
    }
}

/// The video of a call folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallVideo {
    Kept {
        bytes: u64,
    },
    Purged {
        purged_at: Timestamp,
    },
    /// No video was recorded, or finalize has not written it yet.
    Missing,
}

/// One audio side of a call: the local microphone or everyone else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StemSide {
    Mic,
    Remote,
}

impl StemSide {
    const ALL: [StemSide; 2] = [StemSide::Mic, StemSide::Remote];

    fn as_str(self) -> &'static str {
        match self {
            StemSide::Mic => "mic",
            StemSide::Remote => "remote",
        }
    }

    /// The stem's file name in segment `number`, such as `001-mic.wav`.
    #[must_use]
    pub fn file_name(self, number: u32) -> String {
        format!("{number:03}-{}.wav", self.as_str())
    }
}

/// The stems one recording segment left. A call restarted mid-way has several segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSegment {
    pub number: u32,
    pub has_mic: bool,
    pub has_remote: bool,
}

/// What a call folder holds for a person to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallMedia {
    pub video: CallVideo,
    /// In segment order.
    pub audio_segments: Vec<AudioSegment>,
}

impl CallMedia {
    /// Lists the folder's video and stems.
    ///
    /// # Errors
    ///
    /// Fails when the folder cannot be listed, `NotFound` included.
    pub fn scan(folder: &Path) -> std::io::Result<Self> {
        let mut video_bytes = None;
        let mut audio_segments: BTreeMap<u32, AudioSegment> = BTreeMap::new();
        for entry in std::fs::read_dir(folder)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            if file_name == VIDEO_FILE_NAME {
                // A purge or a finalize may remove the file between the listing and this read.
                match entry.metadata() {
                    Ok(metadata) => video_bytes = Some(metadata.len()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                continue;
            }
            let Some((number, side)) = stem_of(file_name) else {
                continue;
            };
            let segment = audio_segments.entry(number).or_insert(AudioSegment {
                number,
                has_mic: false,
                has_remote: false,
            });
            match side {
                StemSide::Mic => segment.has_mic = true,
                StemSide::Remote => segment.has_remote = true,
            }
        }
        let video = match video_bytes {
            Some(bytes) => CallVideo::Kept { bytes },
            None => match PurgeRecord::read(folder) {
                Some(record) => CallVideo::Purged {
                    purged_at: record.purged_at,
                },
                None => CallVideo::Missing,
            },
        };
        Ok(Self {
            video,
            audio_segments: audio_segments.into_values().collect(),
        })
    }
}

// `001-mic.wav` is segment 1's mic stem. Only a name the stem writer would give counts, so each
// link the note gets opens the file it was built from.
fn stem_of(file_name: &str) -> Option<(u32, StemSide)> {
    let (number, rest) = file_name.split_once('-')?;
    let number: u32 = number.parse().ok().filter(|number| *number > 0)?;
    let side = StemSide::ALL
        .into_iter()
        .find(|side| rest.strip_suffix(".wav") == Some(side.as_str()))?;
    (side.file_name(number) == file_name).then_some((number, side))
}

/// What a media link opens in its call folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaTarget {
    Video,
    Stem { number: u32, side: StemSide },
    Folder,
}

/// A clickable link to a call's media. Built by the note writer, parsed by the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaLink {
    folder_name: String,
    pub target: MediaTarget,
}

impl MediaLink {
    /// A link into the call folder `folder_name`.
    ///
    /// # Errors
    ///
    /// Fails when `folder_name` is not a plain call folder name.
    pub fn new(folder_name: &str, target: MediaTarget) -> Result<Self, MediaLinkError> {
        check_folder_name(folder_name)?;
        Ok(Self {
            folder_name: folder_name.to_owned(),
            target,
        })
    }

    /// Reads a `meetingcapture://call/<folder>/<target>` URL.
    ///
    /// # Errors
    ///
    /// Fails with what is wrong in the URL and the forms it may take.
    pub fn parse(url: &str) -> Result<Self, MediaLinkError> {
        let rest = url
            .strip_prefix(LINK_PREFIX)
            .ok_or(MediaLinkError::NotAMediaLink)?;
        let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
        let target = match parts.as_slice() {
            [_, "video"] => MediaTarget::Video,
            [_, "folder"] => MediaTarget::Folder,
            [_, side, number] => {
                let side = StemSide::ALL
                    .into_iter()
                    .find(|known| known.as_str() == *side)
                    .ok_or(MediaLinkError::UnknownTarget)?;
                let number = number
                    .parse::<u32>()
                    .ok()
                    .filter(|number| *number > 0)
                    .ok_or(MediaLinkError::UnknownTarget)?;
                MediaTarget::Stem { number, side }
            }
            _ => return Err(MediaLinkError::UnknownTarget),
        };
        Self::new(parts[0], target)
    }

    /// A link to another target in the same call folder.
    #[must_use]
    pub fn with_target(&self, target: MediaTarget) -> Self {
        Self {
            folder_name: self.folder_name.clone(),
            target,
        }
    }

    #[must_use]
    pub fn folder_name(&self) -> &str {
        &self.folder_name
    }

    #[must_use]
    pub fn url(&self) -> String {
        let target = match self.target {
            MediaTarget::Video => "video".to_owned(),
            MediaTarget::Folder => "folder".to_owned(),
            MediaTarget::Stem { number, side } => format!("{}/{number}", side.as_str()),
        };
        format!("{LINK_PREFIX}{}/{target}", self.folder_name)
    }

    /// The call folder under `calls_dir`.
    #[must_use]
    pub fn folder_in(&self, calls_dir: &Path) -> PathBuf {
        calls_dir.join(&self.folder_name)
    }

    /// The file or folder the link opens, under `calls_dir`.
    #[must_use]
    pub fn path_in(&self, calls_dir: &Path) -> PathBuf {
        let folder = self.folder_in(calls_dir);
        match self.target {
            MediaTarget::Video => folder.join(VIDEO_FILE_NAME),
            MediaTarget::Stem { number, side } => folder.join(side.file_name(number)),
            MediaTarget::Folder => folder,
        }
    }
}

// A call folder name is one path component the app made: a UTC time and an id. Anything else in
// a link could reach outside the calls folder.
fn check_folder_name(folder_name: &str) -> Result<(), MediaLinkError> {
    let is_plain = !folder_name.is_empty()
        && !folder_name.starts_with('.')
        && folder_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
    if is_plain {
        Ok(())
    } else {
        Err(MediaLinkError::BadFolderName)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaLinkError {
    NotAMediaLink,
    BadFolderName,
    UnknownTarget,
}

impl fmt::Display for MediaLinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            MediaLinkError::NotAMediaLink => "the link does not start with meetingcapture://call/",
            MediaLinkError::BadFolderName => {
                "the call folder name may hold only letters, digits, '.', '_' and '-'"
            }
            MediaLinkError::UnknownTarget => {
                "the link must end in /video, /folder, /mic/<segment> or /remote/<segment>"
            }
        })
    }
}

impl std::error::Error for MediaLinkError {}
