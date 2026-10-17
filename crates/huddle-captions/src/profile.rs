//! Layout profiles: the markers the watcher looks for in Slack's accessibility tree, as data.
//! Slack moves its huddle window around between releases; a new profile file fixes the watcher
//! without a new build.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Built in, newest first.
const BUILT_IN: [&str; 1] = [include_str!("../layouts/2026-10-slack-4.52.toml")];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutProfile {
    pub id: String,
    /// Slack versions this profile was checked against, such as "4.52.171".
    pub tested_on: Vec<String>,
    pub huddle_window: HuddleWindowMarkers,
    pub transcript_list: TranscriptListMarkers,
    pub overlay: OverlayMarkers,
    pub tiles: TileMarkers,
    pub captions_tab: CaptionsTabMarkers,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HuddleWindowMarkers {
    /// The description of the toolbar every Slack child window has, the huddle window included.
    pub child_window_toolbar: String,
    pub content_class: String,
    /// The description of a button Slack shows in the main window during a huddle.
    pub in_huddle_button: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptListMarkers {
    pub role: String,
    pub description: String,
    pub speaker_class: String,
    pub text_class: String,
    pub header_class: String,
    pub language_prefix: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayMarkers {
    pub class: String,
    pub line_class: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TileMarkers {
    pub role: String,
    pub id_prefix: String,
    pub self_marker: String,
    pub name_prefix: String,
    pub name_suffix: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptionsTabMarkers {
    pub id: String,
    pub title: String,
    pub active_class: String,
}

impl AsRef<LayoutProfile> for LayoutProfile {
    fn as_ref(&self) -> &LayoutProfile {
        self
    }
}

impl LayoutProfile {
    #[must_use]
    pub fn is_tested_on(&self, slack_version: &str) -> bool {
        self.tested_on
            .iter()
            .any(|version| version == slack_version)
    }

    /// The participant's name from a tile description such as "View Ujjwal Kumar's profile".
    #[must_use]
    pub fn tile_name<'a>(&self, description: &'a str) -> Option<&'a str> {
        description
            .strip_prefix(&self.tiles.name_prefix)?
            .strip_suffix(&self.tiles.name_suffix)
            .filter(|name| !name.trim().is_empty())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("layout profile {path} is not valid: {source}")]
    Invalid {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("could not read layout profile {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The built-in profiles, with any file in `override_dir` replacing the built-in one of the same
/// id or adding a new one. Each bad file is reported and skipped, so one typo does not stop the
/// watcher.
///
/// # Panics
///
/// Panics when a built-in profile is not valid, which a build with it never ships.
#[must_use]
pub fn load(override_dir: &Path) -> (Vec<LayoutProfile>, Vec<ProfileError>) {
    let mut errors = Vec::new();
    let mut profiles: Vec<LayoutProfile> = BUILT_IN
        .iter()
        .map(|text| toml::from_str(text).expect("built-in layout profiles are valid TOML"))
        .collect();
    let paths = match crate::files_with_extension(override_dir, "toml") {
        Ok(paths) => paths,
        Err(source) => {
            errors.push(ProfileError::Read {
                path: override_dir.to_path_buf(),
                source,
            });
            return (profiles, errors);
        }
    };
    for path in paths {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) => {
                errors.push(ProfileError::Read { path, source });
                continue;
            }
        };
        match toml::from_str::<LayoutProfile>(&text) {
            Ok(profile) => {
                profiles.retain(|known| known.id != profile.id);
                profiles.push(profile);
            }
            Err(source) => errors.push(ProfileError::Invalid { path, source }),
        }
    }
    (profiles, errors)
}

/// The order to try profiles in for `slack_version`: those tested on it, then the rest, newest
/// first. Profile ids start with their date, so newest is the largest id.
#[must_use]
pub fn probe_order<'a, P: AsRef<LayoutProfile>>(
    profiles: &'a [P],
    slack_version: &str,
) -> Vec<&'a P> {
    let mut ordered: Vec<&P> = profiles.iter().collect();
    ordered.sort_by(|left, right| {
        let (left, right) = (left.as_ref(), right.as_ref());
        right
            .is_tested_on(slack_version)
            .cmp(&left.is_tested_on(slack_version))
            .then_with(|| right.id.cmp(&left.id))
    });
    ordered
}
