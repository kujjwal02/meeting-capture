//! Walks a Slack window's accessibility tree and reads captions out of its elements.

use std::collections::VecDeque;
use std::rc::Rc;

use super::ax::Element;
use crate::profile::LayoutProfile;
use crate::tracker::{Caption, ListMatch, RowSight};

const MAX_WALK_DEPTH: usize = 60;
const MAX_WALK_NODES: usize = 4000;
// The huddle window's toolbar sits about 10 levels down; the main window has thousands of nodes.
const CHILD_TOOLBAR_DEPTH: usize = 12;
const CHILD_TOOLBAR_NODES: usize = 150;

/// One node of a walked window, with the attributes every check needs read once.
pub(super) struct Node {
    pub(super) element: Element,
    parent: Option<usize>,
    pub(super) role: Option<String>,
    pub(super) description: Option<String>,
    pub(super) dom_id: Option<String>,
    classes: Vec<String>,
}

impl Node {
    pub(super) fn has_class(&self, class: &str) -> bool {
        self.classes.iter().any(|known| known == class)
    }
}

/// Every node of `root`, breadth first. `None` when Slack is too busy to answer, or the tree is
/// not built yet. The caption list's rows are not walked: they grow without limit, and the list
/// is read on its own.
pub(super) fn walk(root: &Element, profiles: &[Rc<LayoutProfile>]) -> Option<Vec<Node>> {
    let is_caption_list = |node: &Node| {
        profiles.iter().any(|profile| {
            node.role.as_deref() == Some(profile.transcript_list.role.as_str())
                && node.description.as_deref() == Some(profile.transcript_list.description.as_str())
        })
    };
    let mut nodes = Vec::new();
    let mut queue = VecDeque::from([(root.clone(), None, 0_usize)]);
    while let Some((element, parent, depth)) = queue.pop_front() {
        if nodes.len() >= MAX_WALK_NODES {
            break;
        }
        let node = Node {
            role: element.role(),
            description: element.description(),
            dom_id: element.dom_id(),
            classes: element.classes(),
            element,
            parent,
        };
        if depth < MAX_WALK_DEPTH && !is_caption_list(&node) {
            match node.element.children() {
                Ok(children) => {
                    let index = nodes.len();
                    queue.extend(
                        children
                            .into_iter()
                            .map(|child| (child, Some(index), depth + 1)),
                    );
                }
                Err(failure) if failure.is_busy() => return None,
                Err(_) => {}
            }
        }
        nodes.push(node);
    }
    (nodes.len() > 1).then_some(nodes)
}

pub(super) fn is_child_window(window: &Element, profiles: &[Rc<LayoutProfile>]) -> bool {
    has_node(
        window,
        CHILD_TOOLBAR_DEPTH,
        CHILD_TOOLBAR_NODES,
        |element| {
            let description = element.description();
            profiles.iter().any(|profile| {
                description.as_deref() == Some(profile.huddle_window.child_window_toolbar.as_str())
            })
        },
    )
}

pub(super) fn is_in_huddle(
    windows: &[Element],
    except: Option<&Element>,
    profiles: &[Rc<LayoutProfile>],
) -> bool {
    windows
        .iter()
        .filter(|window| Some(*window) != except)
        .any(|window| {
            has_node(window, MAX_WALK_DEPTH, MAX_WALK_NODES, |element| {
                let description = element.description();
                profiles.iter().any(|profile| {
                    description.as_deref() == Some(profile.huddle_window.in_huddle_button.as_str())
                })
            })
        })
}

/// Whether any node under `root` matches, reading one attribute per node and stopping early.
fn has_node(
    root: &Element,
    max_depth: usize,
    max_nodes: usize,
    is_match: impl Fn(&Element) -> bool,
) -> bool {
    let mut queue = VecDeque::from([(root.clone(), 0_usize)]);
    let mut visited = 0;
    while let Some((element, depth)) = queue.pop_front() {
        if is_match(&element) {
            return true;
        }
        visited += 1;
        if visited >= max_nodes {
            return false;
        }
        if depth < max_depth {
            for child in element.children().unwrap_or_default() {
                queue.push_back((child, depth + 1));
            }
        }
    }
    false
}

pub(super) fn find_list<'a>(
    profile: &LayoutProfile,
    nodes: &[Node],
    tile_names: impl Iterator<Item = &'a str>,
) -> Option<(Element, ListMatch)> {
    let markers = &profile.transcript_list;
    let is_list = |node: &Node| node.role.as_deref() == Some(markers.role.as_str());
    if let Some(node) = nodes.iter().find(|node| {
        is_list(node) && node.description.as_deref() == Some(markers.description.as_str())
    }) {
        return Some((node.element.clone(), ListMatch::Semantic));
    }
    let row_list = nodes
        .iter()
        .filter(|node| node.has_class(&markers.speaker_class))
        .find_map(|node| ancestor(nodes, node, is_list));
    if let Some(list) = row_list {
        return Some((list.element.clone(), ListMatch::Class));
    }
    // A list whose rows each start with a participant's name.
    let names: Vec<&str> = tile_names.collect();
    if names.is_empty() {
        return None;
    }
    nodes
        .iter()
        .filter(|node| is_list(node))
        .find(|list| {
            list.element
                .children()
                .unwrap_or_default()
                .iter()
                .any(|row| {
                    let texts = static_texts(row, 3);
                    texts.len() >= 2 && names.contains(&texts[0].as_str())
                })
        })
        .map(|list| (list.element.clone(), ListMatch::Structure))
}

fn ancestor<'a>(
    nodes: &'a [Node],
    node: &Node,
    is_match: impl Fn(&Node) -> bool,
) -> Option<&'a Node> {
    let mut parent = node.parent;
    while let Some(index) = parent {
        let candidate = &nodes[index];
        if is_match(candidate) {
            return Some(candidate);
        }
        parent = candidate.parent;
    }
    None
}

pub(super) fn row_sight(profile: &LayoutProfile, row: &Element, level: ListMatch) -> RowSight {
    let markers = &profile.transcript_list;
    let mut speaker = None;
    let mut text = None;
    let mut language = None;
    match level {
        ListMatch::Semantic | ListMatch::Class => {
            for part in row.children().unwrap_or_default() {
                let classes = part.classes();
                let has = |class: &str| classes.iter().any(|known| known == class);
                if has(&markers.speaker_class) {
                    speaker = Some(one_line(&static_texts(&part, 3).join("")));
                } else if has(&markers.text_class) {
                    text = Some(one_line(&static_texts(&part, 3).join("")));
                } else if has(&markers.header_class) {
                    language = static_texts(&part, 3).iter().find_map(|line| {
                        let language = line.strip_prefix(&markers.language_prefix)?;
                        Some(language.trim().trim_end_matches('.').trim().to_owned())
                    });
                }
            }
        }
        ListMatch::Structure => {
            let parts = static_texts(row, 4);
            if let Some((first, rest)) = parts.split_first() {
                speaker = Some(one_line(first));
                text = Some(one_line(&rest.join(" ")));
            }
        }
    }
    if let Some(language) = language {
        return RowSight::Language(language);
    }
    match (
        speaker.filter(|speaker| !speaker.is_empty()),
        text.filter(|text| !text.is_empty()),
    ) {
        (Some(speaker), Some(text)) => RowSight::Caption(Caption { speaker, text }),
        (Some(_) | None, _) => RowSight::Unreadable,
    }
}

/// An overlay line reads "<speaker>", ":", "<text>".
pub(super) fn overlay_line(line: &Element) -> Option<Caption> {
    let texts = static_texts(line, 4);
    let (speaker, rest) = texts.split_first()?;
    let text = one_line(
        &rest
            .iter()
            .skip_while(|part| part.trim() == ":")
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
    );
    let speaker = one_line(speaker);
    (!text.is_empty() && !speaker.is_empty()).then_some(Caption { speaker, text })
}

/// The values of the static text nodes under `element`, in tree order.
fn static_texts(element: &Element, max_depth: usize) -> Vec<String> {
    let mut texts = Vec::new();
    collect_static_texts(element, max_depth, &mut texts);
    texts
}

fn collect_static_texts(element: &Element, depth_left: usize, texts: &mut Vec<String>) {
    if element.role().as_deref() == Some("AXStaticText") {
        if let Some(value) = element.value_text() {
            texts.push(value);
        }
        return;
    }
    if depth_left == 0 {
        return;
    }
    for child in element.children().unwrap_or_default() {
        collect_static_texts(&child, depth_left - 1, texts);
    }
}

// Slack splits a caption over several text nodes, with spaces at the joins.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
