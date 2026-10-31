//! The HLS playlist of one video segment. The writer appends to it as each init segment and
//! fragment lands, so a player, and repair, can read every finished fragment in order. A part
//! after the first starts with a discontinuity and its own init segment.

use std::fmt::Write as _;

pub const PLAYLIST_FILE_NAME: &str = "index.m3u8";

/// Fragments end on the first keyframe after each 2 s, so one runs a little over 2 s. A rebuilt
/// stream can stretch one further; players accept a fragment up to this rounded length.
const TARGET_DURATION_S: u32 = 4;

pub(crate) const ENDLIST_LINE: &str = "#EXT-X-ENDLIST";

/// One line group of the playlist, in order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlaylistEntry {
    /// A part's init segment. Every fragment after it until the next one decodes with it.
    Init {
        file: String,
    },
    Fragment {
        file: String,
        duration_ms: f64,
    },
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Playlist {
    pub entries: Vec<PlaylistEntry>,
    pub has_endlist: bool,
}

pub(crate) fn header() -> String {
    format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{TARGET_DURATION_S}\n\
         #EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-INDEPENDENT-SEGMENTS\n"
    )
}

pub(crate) fn entry_lines(entry: &PlaylistEntry, is_first_init: bool) -> String {
    match entry {
        PlaylistEntry::Init { file } if is_first_init => {
            format!("#EXT-X-MAP:URI=\"{file}\"\n")
        }
        PlaylistEntry::Init { file } => {
            format!("#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"{file}\"\n")
        }
        PlaylistEntry::Fragment { file, duration_ms } => {
            format!("#EXTINF:{:.3},\n{file}\n", duration_ms / 1_000.0)
        }
    }
}

impl Playlist {
    /// Reads the entries the writer appended. A line cut short by a crash, as an `#EXTINF` with
    /// no file after it, is dropped.
    pub(crate) fn parse(text: &str) -> Self {
        let mut playlist = Self::default();
        let mut pending_duration_ms = None;
        for line in text.lines().map(str::trim) {
            if let Some(uri) = line
                .strip_prefix("#EXT-X-MAP:URI=\"")
                .and_then(|rest| rest.strip_suffix('"'))
            {
                playlist.entries.push(PlaylistEntry::Init {
                    file: uri.to_owned(),
                });
            } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
                let seconds = rest.trim_end_matches(',').split(',').next().unwrap_or("");
                pending_duration_ms = seconds.parse::<f64>().ok().map(|s| s * 1_000.0);
            } else if line == ENDLIST_LINE {
                playlist.has_endlist = true;
            } else if !line.is_empty()
                && !line.starts_with('#')
                && let Some(duration_ms) = pending_duration_ms.take()
            {
                playlist.entries.push(PlaylistEntry::Fragment {
                    file: line.to_owned(),
                    duration_ms,
                });
            }
        }
        playlist
    }

    pub(crate) fn render(&self) -> String {
        let mut text = header();
        let mut is_first_init = true;
        for entry in &self.entries {
            text.push_str(&entry_lines(entry, is_first_init));
            if matches!(entry, PlaylistEntry::Init { .. }) {
                is_first_init = false;
            }
        }
        if self.has_endlist {
            let _infallible = writeln!(text, "{ENDLIST_LINE}");
        }
        text
    }
}
