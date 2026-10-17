//! Puts a fresh render into a note the user edited. Each section between `%% mc:begin NAME %%` and
//! `%% mc:end NAME %%` is replaced, and so are the app's frontmatter keys and tags. Everything
//! else stays as the user left it. The splice refuses rather than guess when the markers do not
//! line up, and rather than overwrite when the user edited inside a section.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::note_sha256;
use crate::render::{APP_KEYS, APP_TAGS};

const BEGIN: &str = "%% mc:begin ";
const END: &str = "%% mc:end ";
const MARKER_CLOSE: &str = " %%";

/// SHA-256 of each marked section's contents in a note, by section name.
///
/// # Errors
///
/// Returns why the markers do not line up.
pub(super) fn section_sha256s(note: &str) -> Result<BTreeMap<String, String>, String> {
    let (_, body) = split_frontmatter(note);
    let pieces = pieces(body)?;
    Ok(pieces
        .iter()
        .filter_map(|piece| match piece {
            Piece::Section { name, content } => {
                Some(((*name).to_owned(), note_sha256(content.as_bytes())))
            }
            Piece::Text(_) => None,
        })
        .collect())
}

/// The note with `rendered`'s sections and frontmatter keys put into `on_disk`. `written` holds
/// the hash of each section the app last wrote, by name.
///
/// # Errors
///
/// Returns why the two cannot be lined up, for the `NoteEdited` message.
pub(super) fn splice(
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

    let rendered_sections = sections(&rendered_pieces);
    let rendered_content = |name: &str| {
        rendered_sections
            .iter()
            .find(|(rendered_name, _)| *rendered_name == name)
            .map(|(_, content)| *content)
    };
    let disk_sections = sections(&disk_pieces);

    let is_as_written = |name: &str, content: &str| {
        written
            .get(name)
            .is_some_and(|sha256| *sha256 == note_sha256(content.as_bytes()))
    };

    // Only sections the render replaces need to be as the app wrote them.
    let edited: Vec<&str> = disk_sections
        .iter()
        .filter(|(name, content)| rendered_content(name).is_some() && !is_as_written(name, content))
        .map(|(name, _)| *name)
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

    // A section missing from the note with a stored hash was deleted by the user, markers and
    // all, and stays deleted. One without a hash is new to the template: it goes before the next
    // section the note has, in template order, or at the end.
    let mut new_sections = rendered_sections
        .iter()
        .filter(|(name, _)| {
            !written.contains_key(*name)
                && !disk_sections.iter().any(|(disk_name, _)| disk_name == name)
        })
        .peekable();
    let mut body = String::with_capacity(rendered_body.len() + disk_body.len() / 4);
    let write_section = |body: &mut String, name: &str, content: &str| {
        let _ = writeln!(body, "{BEGIN}{name}{MARKER_CLOSE}");
        body.push_str(content);
        let _ = writeln!(body, "{END}{name}{MARKER_CLOSE}");
    };
    let template_position = |name: &str| {
        rendered_sections
            .iter()
            .position(|(rendered_name, _)| *rendered_name == name)
    };
    let mut is_after_dropped_section = false;
    for piece in &disk_pieces {
        match piece {
            // Keeps one blank line where a dropped section sat between two blank lines.
            Piece::Text(text) if is_after_dropped_section && body.ends_with("\n\n") => {
                body.push_str(text.strip_prefix('\n').unwrap_or(text));
            }
            Piece::Text(text) => body.push_str(text),
            Piece::Section { name, content } => match rendered_content(name) {
                Some(rendered) => {
                    let position = template_position(name);
                    while let Some((new_name, new_content)) =
                        new_sections.next_if(|(new_name, _)| template_position(new_name) < position)
                    {
                        write_section(&mut body, new_name, new_content);
                        body.push('\n');
                    }
                    write_section(&mut body, name, rendered);
                }
                // A section the template no longer renders is dropped when it holds the app's own
                // text, and kept when the user edited it.
                None if is_as_written(name, content) => {
                    is_after_dropped_section = true;
                    continue;
                }
                None => write_section(&mut body, name, content),
            },
        }
        is_after_dropped_section = false;
    }
    for (new_name, new_content) in new_sections {
        if !body.ends_with("\n\n") {
            body.push('\n');
        }
        write_section(&mut body, new_name, new_content);
    }

    let frontmatter = match (disk_frontmatter, rendered_frontmatter) {
        (Some(disk), Some(rendered)) => merge_frontmatter(disk, rendered)?,
        (None, Some(rendered)) => rendered.to_owned(),
        (_, None) => return Err("the template writes no frontmatter".to_owned()),
    };
    Ok(format!("---\n{frontmatter}---\n{body}"))
}

/// Each marked section's name and contents, in order.
fn sections<'a>(pieces: &[Piece<'a>]) -> Vec<(&'a str, &'a str)> {
    pieces
        .iter()
        .filter_map(|piece| match piece {
            Piece::Section { name, content } => Some((*name, *content)),
            Piece::Text(_) => None,
        })
        .collect()
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
