//! Puts a fresh render into a note the user edited. Each section between `%% mc:begin NAME %%` and
//! `%% mc:end NAME %%` is replaced, and so are the app's frontmatter keys and tags. Everything
//! else stays as the user left it. The splice refuses rather than guess when the markers do not
//! line up, and rather than overwrite when the user edited inside a section.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::note::{self, APP_KEYS, APP_TAGS};

const BEGIN: &str = "%% mc:begin ";
const END: &str = "%% mc:end ";
const MARKER_CLOSE: &str = " %%";

/// SHA-256 of each marked section's contents in a note the app renders, by section name.
///
/// # Errors
///
/// Returns why the template's markers do not line up.
pub(crate) fn section_sha256s(rendered: &str) -> Result<BTreeMap<String, String>, String> {
    let (_, body) = split_frontmatter(rendered);
    let pieces = pieces(body).map_err(|reason| format!("in the template, {reason}"))?;
    Ok(pieces
        .iter()
        .filter_map(|piece| match piece {
            Piece::Section { name, content } => {
                Some(((*name).to_owned(), note::note_sha256(content.as_bytes())))
            }
            Piece::Text(_) => None,
        })
        .collect())
}

/// The note with `rendered`'s sections and frontmatter keys put into `on_disk`. `written` holds
/// each section's hash as the app last wrote it.
///
/// # Errors
///
/// Returns why the two cannot be lined up, for the `NoteEdited` message.
pub(crate) fn splice(
    on_disk: &str,
    rendered: &str,
    written: &BTreeMap<String, String>,
) -> Result<String, String> {
    // Editors on Windows and git's autocrlf write CRLF, some with a BOM. The app writes LF.
    let on_disk = on_disk
        .strip_prefix('\u{feff}')
        .unwrap_or(on_disk)
        .replace("\r\n", "\n");
    let (disk_frontmatter, disk_body) = split_frontmatter(&on_disk);
    if disk_frontmatter.is_none() && on_disk.starts_with("---") {
        return Err("the note's frontmatter has no closing --- line".to_owned());
    }
    let (rendered_frontmatter, rendered_body) = split_frontmatter(rendered);
    let disk_pieces = pieces(disk_body).map_err(|reason| format!("in the note, {reason}"))?;
    let rendered_pieces =
        pieces(rendered_body).map_err(|reason| format!("in the template, {reason}"))?;

    let section_names = |pieces: &[Piece<'_>]| -> Vec<String> {
        let mut names: Vec<String> = pieces
            .iter()
            .filter_map(|piece| match piece {
                Piece::Section { name, .. } => Some((*name).to_owned()),
                Piece::Text(_) => None,
            })
            .collect();
        names.sort();
        names
    };
    let (disk_names, rendered_names) =
        (section_names(&disk_pieces), section_names(&rendered_pieces));
    if disk_names != rendered_names {
        let missing: Vec<&str> = rendered_names
            .iter()
            .filter(|name| !disk_names.contains(name))
            .map(String::as_str)
            .collect();
        return Err(format!(
            "the mc:begin/mc:end markers for {} are missing or extra",
            if missing.is_empty() {
                "some sections".to_owned()
            } else {
                missing.join(", ")
            }
        ));
    }

    let edited: Vec<&str> = disk_pieces
        .iter()
        .filter_map(|piece| match piece {
            Piece::Section { name, content } => {
                let is_as_written = written
                    .get(*name)
                    .is_some_and(|sha256| *sha256 == note::note_sha256(content.as_bytes()));
                (!is_as_written).then_some(*name)
            }
            Piece::Text(_) => None,
        })
        .collect();
    if !edited.is_empty() {
        return Err(format!(
            "the {} {} edited, and the app does not overwrite your edits there",
            edited.join(", "),
            if edited.len() == 1 {
                "section was"
            } else {
                "sections were"
            }
        ));
    }

    let mut body = String::with_capacity(rendered_body.len() + disk_body.len() / 4);
    for piece in &disk_pieces {
        match piece {
            Piece::Text(text) => body.push_str(text),
            Piece::Section { name, .. } => {
                let content = rendered_pieces
                    .iter()
                    .find_map(|rendered| match rendered {
                        Piece::Section {
                            name: rendered_name,
                            content,
                        } if rendered_name == name => Some(*content),
                        Piece::Section { .. } | Piece::Text(_) => None,
                    })
                    .unwrap_or_default();
                let _ = writeln!(body, "{BEGIN}{name}{MARKER_CLOSE}");
                body.push_str(content);
                let _ = writeln!(body, "{END}{name}{MARKER_CLOSE}");
            }
        }
    }

    let frontmatter = match (disk_frontmatter, rendered_frontmatter) {
        (Some(disk), Some(rendered)) => merge_frontmatter(disk, rendered)?,
        (None, Some(rendered)) => rendered.to_owned(),
        (_, None) => return Err("the template writes no frontmatter".to_owned()),
    };
    Ok(format!("---\n{frontmatter}---\n{body}"))
}

enum Piece<'a> {
    Text(&'a str),
    Section { name: &'a str, content: &'a str },
}

// Splits a body into text and marked sections. Markers sit alone on their line; sections do not nest
// and each name appears once.
fn pieces(body: &str) -> Result<Vec<Piece<'_>>, String> {
    let mut pieces = Vec::new();
    let mut open: Option<(&str, usize)> = None;
    let mut text_start = 0;
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let marker = line.trim();
        if let Some(name) = marker
            .strip_prefix(BEGIN)
            .and_then(|rest| rest.strip_suffix(MARKER_CLOSE))
        {
            if let Some((open_name, _)) = open {
                return Err(format!("section {name} starts inside section {open_name}"));
            }
            let is_repeat = pieces
                .iter()
                .any(|piece| matches!(piece, Piece::Section { name: seen, .. } if *seen == name));
            if is_repeat {
                return Err(format!("section {name} appears twice"));
            }
            pieces.push(Piece::Text(&body[text_start..line_start]));
            open = Some((name, offset));
        } else if let Some(name) = marker
            .strip_prefix(END)
            .and_then(|rest| rest.strip_suffix(MARKER_CLOSE))
        {
            match open {
                Some((open_name, content_start)) if open_name == name => {
                    pieces.push(Piece::Section {
                        name,
                        content: &body[content_start..line_start],
                    });
                    open = None;
                    text_start = offset;
                }
                Some((open_name, _)) => {
                    return Err(format!(
                        "section {open_name} ends with the marker for {name}"
                    ));
                }
                None => return Err(format!("section {name} ends without starting")),
            }
        }
    }
    if let Some((name, _)) = open {
        return Err(format!("section {name} never ends"));
    }
    pieces.push(Piece::Text(&body[text_start..]));
    Ok(pieces)
}

/// The YAML between the opening and closing `---` lines, and the body after them.
fn split_frontmatter(note: &str) -> (Option<&str>, &str) {
    let Some(rest) = note.strip_prefix("---\n") else {
        return (None, note);
    };
    if let Some(body) = rest.strip_prefix("---\n") {
        return (Some(""), body);
    }
    match rest.find("\n---\n") {
        Some(end) => (Some(&rest[..=end]), &rest[end + 5..]),
        None => (None, note),
    }
}

#[derive(Serialize, Deserialize)]
struct Tags {
    #[serde(default)]
    tags: Vec<String>,
}

// The rendered frontmatter, with the user's own tags added to its tags and the user's own keys
// after it, as the user wrote them.
fn merge_frontmatter(disk: &str, rendered: &str) -> Result<String, String> {
    let disk_entries = entries(disk);
    let rendered_entries = entries(rendered);
    let tags_of =
        |entries: &[(Option<&str>, String)], whose: &str| -> Result<Vec<String>, String> {
            match entries.iter().find(|(key, _)| *key == Some("tags")) {
                Some((_, text)) => serde_saphyr::from_str::<Tags>(text)
                    .map(|parsed| parsed.tags)
                    .map_err(|error| format!("{whose} tags are not a list of strings: {error}")),
                None => Ok(Vec::new()),
            }
        };
    let mut tags = tags_of(&rendered_entries, "the template's")?;
    for tag in tags_of(&disk_entries, "the note's")? {
        if !APP_TAGS.contains(&tag.as_str()) && !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    let tags_entry = serde_saphyr::to_string(&Tags { tags })
        .map_err(|error| format!("could not write the tags: {error}"))?;

    let mut merged = String::new();
    for (key, text) in &rendered_entries {
        if *key == Some("tags") {
            merged.push_str(&tags_entry);
        } else {
            merged.push_str(text);
        }
    }
    for (key, text) in &disk_entries {
        if !key.is_some_and(|key| APP_KEYS.contains(&key)) {
            merged.push_str(text);
        }
    }
    Ok(merged)
}

// Top-level YAML entries with their raw text, continuation lines included. A key of `None` holds
// comments or blank lines before the first key.
fn entries(yaml: &str) -> Vec<(Option<&str>, String)> {
    let mut entries: Vec<(Option<&str>, String)> = Vec::new();
    for line in yaml.split_inclusive('\n') {
        let starts_entry = line
            .chars()
            .next()
            .is_some_and(|first| !first.is_whitespace() && first != '-' && first != '#');
        let key = starts_entry
            .then(|| {
                line.split_once(':')
                    .map(|(key, _)| key.trim().trim_matches(['"', '\'']))
            })
            .flatten();
        match (key, entries.last_mut()) {
            (None, Some(last)) => last.1.push_str(line),
            (key, _) => entries.push((key, line.to_owned())),
        }
    }
    entries
}
