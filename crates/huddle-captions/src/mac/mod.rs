//! The macOS watcher: finds Slack huddle windows, reads their captions and participants, and
//! reports how well that worked.

mod ax;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use sources::slack_huddle::{self, HuddleHealth, LogLine};

use self::ax::{Element, SlackProcess};
use crate::log::HuddleLog;
use crate::profile::{self, LayoutProfile};
use crate::{Notice, WatchPaths, WatchSettings};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
// Walking a window costs one IPC call per attribute per node, so windows are walked less often
// than the cached caption list is read.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);
const PERMISSION_CHECK_INTERVAL: Duration = Duration::from_secs(5);
// Slack takes a few seconds to show captions in a new huddle.
const CAPTIONS_GRACE: Duration = Duration::from_secs(30);
const FIRST_RETRY_AFTER: Duration = Duration::from_secs(10);
// Once the panel was shown, a hidden panel means the user picked another tab, such as Thread.
// Slack keeps filling the hidden panel, so nothing is lost meanwhile.
const HIDDEN_PANEL_PATIENCE: Duration = Duration::from_mins(2);
// A warning must hold this long before it is reported. At huddle start the overlay shows a few
// seconds before the panel.
const WARNING_SETTLE: Duration = Duration::from_secs(10);
// One failed read of Slack's window list or a busy Slack must not end a huddle.
const MISSED_DISCOVERIES_TO_END: u32 = 3;
const MAX_WALK_DEPTH: usize = 60;
const MAX_WALK_NODES: usize = 4000;
// The huddle window's toolbar sits about 10 levels down; the main window has thousands of nodes.
const CHILD_TOOLBAR_DEPTH: usize = 12;
const CHILD_TOOLBAR_NODES: usize = 150;
// A row that grows is one of the last few; earlier rows are final.
const LIVE_ROWS: usize = 3;
const OVERLAY_MEMORY: usize = 40;

pub(crate) fn run(
    paths: &WatchPaths,
    settings: &impl Fn() -> WatchSettings,
    notify: impl FnMut(Notice),
) {
    let (profiles, errors) = profile::load(&paths.layout_override_dir);
    let mut watcher = Watcher {
        log_dir: paths.log_dir.clone(),
        profiles: profiles.into_iter().map(Rc::new).collect(),
        notify,
        is_trusted: None,
        permission_checked_at: None,
        discovered_at: None,
        huddles: Vec::new(),
        has_reported_start_error: false,
    };
    for error in errors {
        tracing::warn!(exception.message = %error, "layout profile skipped");
        (watcher.notify)(Notice::Problem {
            message: error.to_string(),
        });
    }
    loop {
        // AppKit hands back autoreleased objects, and this thread has no run loop to drain them.
        objc2::rc::autoreleasepool(|_| watcher.tick(settings()));
        std::thread::sleep(POLL_INTERVAL);
    }
}

struct Watcher<N> {
    log_dir: PathBuf,
    profiles: Vec<Rc<LayoutProfile>>,
    notify: N,
    is_trusted: Option<bool>,
    permission_checked_at: Option<Instant>,
    discovered_at: Option<Instant>,
    huddles: Vec<Huddle>,
    /// A log that cannot be created fails again every discovery; one notification is enough.
    has_reported_start_error: bool,
}

impl<N: FnMut(Notice)> Watcher<N> {
    fn tick(&mut self, settings: WatchSettings) {
        if !settings.read_captions {
            self.end_huddles(|_| true);
            return;
        }
        if !self.check_permission() {
            return;
        }
        if self
            .discovered_at
            .is_none_or(|at| at.elapsed() >= DISCOVERY_INTERVAL)
        {
            self.discovered_at = Some(Instant::now());
            self.discover(settings);
        }
        for huddle in &mut self.huddles {
            if let Err(error) = huddle.read_captions() {
                huddle.report_log_error(&mut self.notify, &error);
            }
        }
    }

    // The user can grant or revoke the permission at any time, without a relaunch.
    fn check_permission(&mut self) -> bool {
        if self
            .permission_checked_at
            .is_some_and(|at| at.elapsed() < PERMISSION_CHECK_INTERVAL)
        {
            return self.is_trusted == Some(true);
        }
        self.permission_checked_at = Some(Instant::now());
        let is_trusted = ax::is_trusted();
        match (self.is_trusted, is_trusted) {
            (None | Some(true), false) => {
                tracing::warn!("the Accessibility permission is missing");
                ax::ask_for_trust();
                (self.notify)(Notice::PermissionMissing);
                // Nothing can be read until the grant, so the logs so far become notes.
                self.end_huddles(|_| true);
            }
            (Some(false), true) => {
                tracing::info!("the Accessibility permission was granted");
                (self.notify)(Notice::PermissionGranted);
            }
            (None | Some(true), true) | (Some(false), false) => {}
        }
        self.is_trusted = Some(is_trusted);
        is_trusted
    }

    fn discover(&mut self, settings: WatchSettings) {
        let Some(slack) = ax::find_slack() else {
            self.end_huddles(|_| true);
            return;
        };
        let app = Element::application(slack.pid);
        app.enable_manual_accessibility();
        let windows = match app.windows() {
            Ok(windows) => windows,
            Err(failure) => {
                tracing::debug!(ax.error = failure.0, "Slack window list read failed");
                return;
            }
        };
        let seen: Vec<usize> = windows
            .iter()
            .filter_map(|window| self.discover_window(&slack, &windows, window, settings))
            .collect();
        self.handle_missing_windows(&windows, &seen);
    }

    /// Returns the index of the huddle this window belongs to.
    fn discover_window(
        &mut self,
        slack: &SlackProcess,
        windows: &[Element],
        window: &Element,
        settings: WatchSettings,
    ) -> Option<usize> {
        let known = self
            .huddles
            .iter()
            .position(|huddle| huddle.window.as_ref() == Some(window));
        let mut place = None;
        if known.is_none() {
            // The sharing toolbar ("Slack") and an untitled window open with a screen share; a
            // huddle window is titled "<channel> - <workspace> - Slack".
            let title = window.title()?;
            let (channel, workspace) = slack_huddle::split_window_title(&title)?;
            // The main window can carry the same title shape; a huddle window is a child window.
            if !self.is_child_window(window) {
                return None;
            }
            place = Some((title, channel, workspace));
        }
        let Some(nodes) = walk(window, &self.profiles) else {
            // Slack is busy, or the tree is still being built. Try again next time.
            return known;
        };
        let profile = profile::probe_order(&self.profiles, &slack.version)
            .into_iter()
            .find(|profile| {
                nodes
                    .iter()
                    .any(|node| node.has_class(&profile.huddle_window.content_class))
            })
            .cloned();
        let index = match (known, place) {
            (Some(index), _) => index,
            (None, Some((title, channel, workspace))) => {
                // A conversation opened in its own window is a child window too. An unreadable
                // child window counts as a huddle only while Slack offers "Leave huddle".
                if profile.is_none() && !self.is_in_huddle(windows, Some(window)) {
                    return None;
                }
                self.attach_window(slack, window, title, channel, workspace, profile.clone())?
            }
            (None, None) => return None,
        };
        let huddle = &mut self.huddles[index];
        huddle.missed_discoveries = 0;
        huddle.last_seen_at = Timestamp::now();
        let checked = match profile {
            Some(profile) if huddle.profile.is_none() => huddle.set_profile(profile),
            Some(_) | None => Ok(()),
        }
        .and_then(|()| huddle.check_window(&nodes, settings));
        self.report_check(index, checked);
        Some(index)
    }

    /// Gives a window to the huddle whose window was closed while it went on, else starts a new
    /// huddle. Returns the huddle's index.
    fn attach_window(
        &mut self,
        slack: &SlackProcess,
        window: &Element,
        title: String,
        channel: String,
        workspace: String,
        profile: Option<Rc<LayoutProfile>>,
    ) -> Option<usize> {
        if let Some(index) = self
            .huddles
            .iter()
            .position(|huddle| huddle.window.is_none() && huddle.channel == channel)
        {
            tracing::info!("Slack huddle window reopened");
            self.huddles[index].window = Some(window.clone());
            return Some(index);
        }
        match Huddle::start(
            &self.log_dir,
            slack,
            window,
            title,
            channel,
            workspace,
            profile,
        ) {
            Ok(huddle) => {
                self.has_reported_start_error = false;
                tracing::info!(
                    slack.version = slack.version,
                    slack.layout_profile = profile_id(huddle.profile.as_deref()),
                    "Slack huddle started"
                );
                (self.notify)(Notice::HuddleStarted {
                    log_path: huddle.log.path().to_path_buf(),
                    channel: huddle.channel.clone(),
                });
                self.huddles.push(huddle);
                Some(self.huddles.len() - 1)
            }
            Err(error) => {
                tracing::error!(exception.message = %error, "could not start a huddle log");
                if !std::mem::replace(&mut self.has_reported_start_error, true) {
                    (self.notify)(Notice::Problem {
                        message: format!("could not start a huddle log: {error}"),
                    });
                }
                None
            }
        }
    }

    // A huddle whose window is gone ends, unless Slack still offers "Leave huddle": the user
    // closed the window and the huddle goes on in the sidebar.
    fn handle_missing_windows(&mut self, windows: &[Element], seen: &[usize]) {
        let mut is_in_huddle = None;
        let mut over = Vec::new();
        for index in 0..self.huddles.len() {
            if seen.contains(&index) {
                continue;
            }
            let huddle = &mut self.huddles[index];
            huddle.missed_discoveries += 1;
            if huddle.missed_discoveries < MISSED_DISCOVERIES_TO_END {
                continue;
            }
            let is_in_huddle =
                *is_in_huddle.get_or_insert_with(|| self.is_in_huddle(windows, None));
            let huddle = &mut self.huddles[index];
            if !is_in_huddle {
                over.push(huddle.log.path().to_path_buf());
                continue;
            }
            huddle.last_seen_at = Timestamp::now();
            if huddle.window.take().is_some() {
                tracing::info!("Slack huddle window closed; the huddle goes on");
                huddle.detach();
            }
            // Slack keeps filling the closed window's panel, so a short absence loses nothing.
            let is_closed_briefly = huddle
                .panel_hidden_since
                .is_some_and(|since| since.elapsed() < HIDDEN_PANEL_PATIENCE);
            let checked = huddle.set_health((!is_closed_briefly).then(|| {
                (
                    HuddleHealth::Degraded,
                    "The huddle window is closed, so captions are not read. Open it from Slack's sidebar"
                        .to_owned(),
                )
            }));
            self.report_check(index, checked);
        }
        self.end_huddles(|huddle| over.iter().any(|path| path == huddle.log.path()));
    }

    fn report_check(&mut self, index: usize, checked: std::io::Result<()>) {
        let huddle = &mut self.huddles[index];
        if let Err(error) = checked {
            huddle.report_log_error(&mut self.notify, &error);
        }
        if let Some((state, reason)) = huddle.health_to_report.take() {
            (self.notify)(Notice::Health {
                log_path: huddle.log.path().to_path_buf(),
                channel: huddle.channel.clone(),
                state,
                reason,
            });
        }
    }

    fn is_child_window(&self, window: &Element) -> bool {
        has_node(
            window,
            CHILD_TOOLBAR_DEPTH,
            CHILD_TOOLBAR_NODES,
            |element| {
                let description = element.description();
                self.profiles.iter().any(|profile| {
                    description.as_deref()
                        == Some(profile.huddle_window.child_window_toolbar.as_str())
                })
            },
        )
    }

    fn is_in_huddle(&self, windows: &[Element], except: Option<&Element>) -> bool {
        windows
            .iter()
            .filter(|window| Some(*window) != except)
            .any(|window| {
                has_node(window, MAX_WALK_DEPTH, MAX_WALK_NODES, |element| {
                    let description = element.description();
                    self.profiles.iter().any(|profile| {
                        description.as_deref()
                            == Some(profile.huddle_window.in_huddle_button.as_str())
                    })
                })
            })
    }

    fn end_huddles(&mut self, is_over: impl Fn(&Huddle) -> bool) {
        let (ended, open): (Vec<Huddle>, Vec<Huddle>) = std::mem::take(&mut self.huddles)
            .into_iter()
            .partition(is_over);
        self.huddles = open;
        for mut huddle in ended {
            let ended_at = if huddle.missed_discoveries > 0 {
                huddle.last_seen_at
            } else {
                Timestamp::now()
            };
            let written = huddle
                .flush_overlay()
                .and_then(|()| huddle.log.append(&LogLine::Ended { seen_at: ended_at }));
            if let Err(error) = written {
                huddle.report_log_error(&mut self.notify, &error);
            }
            tracing::info!(
                caption.rows = huddle.row_captions.len(),
                caption.row_edits = huddle.row_edits,
                caption.overlay_lines = huddle.overlay_lines,
                "Slack huddle ended"
            );
            (self.notify)(Notice::HuddleEnded {
                log_path: huddle.log.path().to_path_buf(),
                channel: huddle.channel.clone(),
            });
        }
    }
}

fn profile_id(profile: Option<&LayoutProfile>) -> &str {
    profile.map_or("none", |profile| &profile.id)
}

/// How the caption list was found. Each level down survives more Slack changes and trusts the
/// result less.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListMatch {
    /// Role and accessibility description, which Slack keeps for screen readers.
    Semantic,
    /// The row classes.
    Class,
    /// A list whose rows start with a participant's name.
    Structure,
}

#[derive(Debug, Clone, Copy)]
enum ListFind {
    /// The first time in this huddle: the rows predate the watch.
    First,
    /// After the panel was hidden.
    Again,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Caption {
    speaker: String,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tile {
    name: String,
    is_self: bool,
}

struct Huddle {
    channel: String,
    /// `None` while the user has closed the huddle window and the huddle goes on.
    window: Option<Element>,
    profile: Option<Rc<LayoutProfile>>,
    slack_version: String,
    log: HuddleLog,
    started: Instant,
    missed_discoveries: u32,
    last_seen_at: Timestamp,
    list: Option<(Element, ListMatch)>,
    /// The list was found since the last read, so every row is read, not just the last few.
    unread_list: Option<ListFind>,
    has_seen_list: bool,
    panel_hidden_since: Option<Instant>,
    overlay: Option<Element>,
    row_captions: HashMap<String, Caption>,
    row_edits: u64,
    overlay_logged: VecDeque<Caption>,
    overlay_live: Option<LiveOverlay>,
    overlay_lines: u64,
    tiles: BTreeMap<String, Tile>,
    captions_seen: Option<Instant>,
    health: Option<HuddleHealth>,
    pending_warning: Option<(HuddleHealth, Instant)>,
    /// A health change for the watcher to report.
    health_to_report: Option<(HuddleHealth, String)>,
    presses: u32,
    pressed_at: Option<Instant>,
    language: Option<String>,
    /// A log that cannot be written fails again every poll; one notification is enough.
    has_reported_log_error: bool,
}

impl Huddle {
    fn start(
        log_dir: &std::path::Path,
        slack: &SlackProcess,
        window: &Element,
        title: String,
        channel: String,
        workspace: String,
        profile: Option<Rc<LayoutProfile>>,
    ) -> std::io::Result<Self> {
        let started_at = Timestamp::now();
        let mut log = HuddleLog::create(log_dir, started_at)?;
        log.append(&LogLine::Started {
            schema: slack_huddle::SCHEMA,
            seen_at: started_at,
            slack_version: slack.version.clone(),
            layout_profile: profile_id(profile.as_deref()).to_owned(),
            window_title: title,
            channel: channel.clone(),
            workspace,
        })?;
        Ok(Self {
            channel,
            window: Some(window.clone()),
            profile,
            slack_version: slack.version.clone(),
            log,
            started: Instant::now(),
            missed_discoveries: 0,
            last_seen_at: started_at,
            list: None,
            unread_list: None,
            has_seen_list: false,
            panel_hidden_since: None,
            overlay: None,
            row_captions: HashMap::new(),
            row_edits: 0,
            overlay_logged: VecDeque::new(),
            overlay_live: None,
            overlay_lines: 0,
            tiles: BTreeMap::new(),
            captions_seen: None,
            health: None,
            pending_warning: None,
            health_to_report: None,
            presses: 0,
            pressed_at: None,
            language: None,
            has_reported_log_error: false,
        })
    }

    fn report_log_error(&mut self, notify: &mut impl FnMut(Notice), error: &std::io::Error) {
        tracing::error!(
            file.path = %self.log.path().display(),
            exception.message = %error,
            "could not write the huddle log"
        );
        if !std::mem::replace(&mut self.has_reported_log_error, true) {
            notify(Notice::Problem {
                message: format!("could not write the huddle log: {error}"),
            });
        }
    }

    /// A layout profile matched after the start: the window's tree was still being built.
    fn set_profile(&mut self, profile: Rc<LayoutProfile>) -> std::io::Result<()> {
        tracing::info!(
            slack.layout_profile = profile.id,
            "Slack huddle layout matched"
        );
        self.log.append(&LogLine::LayoutProfile {
            seen_at: Timestamp::now(),
            layout_profile: profile.id.clone(),
        })?;
        self.profile = Some(profile);
        Ok(())
    }

    /// Forgets the closed window's elements. The rows read so far stay, so the reopened window's
    /// panel adds only new rows.
    fn detach(&mut self) {
        self.list = None;
        self.overlay = None;
        self.panel_hidden_since = Some(Instant::now());
    }

    /// Reads tiles, finds the caption list and overlay, presses the Captions tab when allowed,
    /// and works out the health.
    fn check_window(&mut self, nodes: &[Node], settings: WatchSettings) -> std::io::Result<()> {
        let Some(profile) = self.profile.clone() else {
            let health = (self.started.elapsed() >= CAPTIONS_GRACE).then(|| {
                (
                    HuddleHealth::Broken,
                    format!(
                        "Can't read Slack {} huddle captions: no layout profile matches the huddle window",
                        self.slack_version
                    ),
                )
            });
            return self.set_health(health);
        };

        self.read_tiles(&profile, nodes)?;
        if self.list.is_none() {
            self.list = find_list(&profile, nodes, &self.tiles);
            if self.list.is_some() {
                self.unread_list = Some(if self.has_seen_list {
                    ListFind::Again
                } else {
                    ListFind::First
                });
                self.has_seen_list = true;
                self.panel_hidden_since = None;
            }
        }
        self.overlay = nodes
            .iter()
            .find(|node| node.has_class(&profile.overlay.class))
            .map(|node| node.element.clone());
        let has_overlay = self.overlay.is_some();
        if self.list.is_none() && settings.auto_start_captions {
            self.press_captions_tab(&profile, nodes)?;
        }

        if self.list.is_some() || has_overlay {
            self.captions_seen = Some(Instant::now());
        }
        let quiet_for = self.captions_seen.unwrap_or(self.started).elapsed();
        let is_hidden_briefly = self
            .panel_hidden_since
            .is_some_and(|since| since.elapsed() < HIDDEN_PANEL_PATIENCE);
        let health = match (&self.list, has_overlay) {
            (None, _) if is_hidden_briefly => None,
            (None, _) if self.has_seen_list => Some((
                HuddleHealth::Degraded,
                "The caption panel is hidden, so lines may be missed. Open the Captions tab in the huddle".to_owned(),
            )),
            (None, false) if quiet_for < CAPTIONS_GRACE => None,
            (None, false) => Some((
                HuddleHealth::CaptionsOff,
                "Turn on captions to get a note, or tick Slack's \"Automatically turn on captions\" preference".to_owned(),
            )),
            (None, true) => Some((
                HuddleHealth::Degraded,
                "Only the caption overlay is visible, so lines may be missed. Open the Captions tab in the huddle".to_owned(),
            )),
            (Some((_, ListMatch::Structure)), _) => Some((
                HuddleHealth::Degraded,
                format!(
                    "Slack {} changed the caption panel; it was found by its shape only",
                    self.slack_version
                ),
            )),
            (Some(_), _) if self.tiles.is_empty() => Some((
                HuddleHealth::Degraded,
                "Speaker list not found: the note will have no attendees".to_owned(),
            )),
            (Some(_), _) if !profile.is_tested_on(&self.slack_version) => Some((
                HuddleHealth::Untested,
                format!(
                    "Reading captions with layout {} on Slack {}, which it was not tested on",
                    profile.id, self.slack_version
                ),
            )),
            (Some(_), _) => Some((HuddleHealth::Healthy, "Reading captions".to_owned())),
        };
        self.set_health(health)
    }

    /// Logs a health change and keeps it for the watcher to report. A warning counts only once it
    /// held for `WARNING_SETTLE`; `None` means no verdict yet.
    fn set_health(&mut self, health: Option<(HuddleHealth, String)>) -> std::io::Result<()> {
        let Some((state, reason)) = health else {
            self.pending_warning = None;
            return Ok(());
        };
        if self.health == Some(state) {
            self.pending_warning = None;
            return Ok(());
        }
        if state.is_warning() {
            match self.pending_warning {
                Some((pending, since)) if pending == state => {
                    if since.elapsed() < WARNING_SETTLE {
                        return Ok(());
                    }
                }
                Some(_) | None => {
                    self.pending_warning = Some((state, Instant::now()));
                    return Ok(());
                }
            }
        }
        self.pending_warning = None;
        self.health = Some(state);
        tracing::info!(
            slack.version = self.slack_version,
            slack.layout_profile = profile_id(self.profile.as_deref()),
            huddle.health = state.as_str(),
            "Slack huddle health changed"
        );
        self.log.append(&LogLine::Health {
            seen_at: Timestamp::now(),
            state,
            reason: reason.clone(),
        })?;
        self.health_to_report = Some((state, reason));
        Ok(())
    }

    fn read_tiles(&mut self, profile: &LayoutProfile, nodes: &[Node]) -> std::io::Result<()> {
        let mut seen: BTreeMap<String, Tile> = BTreeMap::new();
        for node in nodes {
            if node.role.as_deref() != Some(profile.tiles.role.as_str()) {
                continue;
            }
            let Some(rest) = node
                .dom_id
                .as_deref()
                .and_then(|id| id.strip_prefix(&profile.tiles.id_prefix))
            else {
                continue;
            };
            // A screen share tile says "View <name>'s screen" and is not a person.
            let Some(name) = node
                .description
                .as_deref()
                .and_then(|description| profile.tile_name(description))
            else {
                continue;
            };
            let (user_id, is_self) = match rest.strip_prefix(&profile.tiles.self_marker) {
                Some(user_id) => (user_id, true),
                None => (rest, false),
            };
            seen.insert(
                user_id.to_owned(),
                Tile {
                    name: name.to_owned(),
                    is_self,
                },
            );
        }
        // Every huddle shows at least the user's own tile. None at all is a window still being
        // built, such as a reopened one, not everyone leaving.
        if seen.is_empty() {
            return Ok(());
        }
        let now = Timestamp::now();
        for (user_id, tile) in &seen {
            if self.tiles.get(user_id) != Some(tile) {
                self.log.append(&LogLine::Tile {
                    seen_at: now,
                    slack_user_id: user_id.clone(),
                    name: tile.name.clone(),
                    is_self: tile.is_self,
                })?;
            }
        }
        for user_id in self.tiles.keys() {
            if !seen.contains_key(user_id) {
                self.log.append(&LogLine::TileGone {
                    seen_at: now,
                    slack_user_id: user_id.clone(),
                })?;
            }
        }
        self.tiles = seen;
        Ok(())
    }

    // Until the panel first shows: one press, one retry after 10 s, then one every 2 minutes.
    // After that, a press waits until the panel has been hidden for 2 minutes, so a user who
    // picked the Thread tab keeps it for a while.
    fn press_captions_tab(
        &mut self,
        profile: &LayoutProfile,
        nodes: &[Node],
    ) -> std::io::Result<()> {
        if self.has_seen_list {
            if self
                .panel_hidden_since
                .is_none_or(|since| since.elapsed() < HIDDEN_PANEL_PATIENCE)
            {
                return Ok(());
            }
            // Restarts the wait, so the next press is 2 minutes after this one.
            self.panel_hidden_since = Some(Instant::now());
        } else {
            let wait = if self.presses < 2 {
                FIRST_RETRY_AFTER
            } else {
                HIDDEN_PANEL_PATIENCE
            };
            if self.pressed_at.is_some_and(|at| at.elapsed() < wait) {
                return Ok(());
            }
        }
        let tab = nodes.iter().find(|node| {
            node.dom_id.as_deref() == Some(profile.captions_tab.id.as_str())
                && node.element.title().as_deref() == Some(profile.captions_tab.title.as_str())
        });
        let Some(tab) = tab else {
            return Ok(());
        };
        if tab.has_class(&profile.captions_tab.active_class) {
            return Ok(());
        }
        self.presses += 1;
        self.pressed_at = Some(Instant::now());
        match tab.element.press() {
            Ok(()) => {
                tracing::info!("pressed the Captions tab");
                self.log.append(&LogLine::CaptionsTabPressed {
                    seen_at: Timestamp::now(),
                })
            }
            Err(failure) => {
                tracing::warn!(ax.error = failure.0, "could not press the Captions tab");
                Ok(())
            }
        }
    }

    /// Reads the cached caption list, or the overlay while the list is missing.
    fn read_captions(&mut self) -> std::io::Result<()> {
        let Some(profile) = self.profile.clone() else {
            return Ok(());
        };
        if let Some((list, level)) = self.list.clone() {
            match list.children() {
                Ok(rows) => {
                    self.flush_overlay()?;
                    return self.read_rows(&profile, &rows, level);
                }
                Err(failure) if failure.is_busy() => return Ok(()),
                Err(failure) => {
                    if !failure.is_gone() {
                        tracing::debug!(ax.error = failure.0, "caption list read failed");
                    }
                    self.list = None;
                    self.panel_hidden_since = Some(Instant::now());
                }
            }
        }
        self.read_overlay(&profile)
    }

    fn read_rows(
        &mut self,
        profile: &LayoutProfile,
        rows: &[Element],
        level: ListMatch,
    ) -> std::io::Result<()> {
        let found = self.unread_list.take();
        let is_list_new = found.is_some();
        // Rows found when the panel comes back were said during the watch.
        let is_backlog = match found {
            Some(ListFind::First) => true,
            Some(ListFind::Again) | None => false,
        };
        let ids: Vec<Option<String>> = rows.iter().map(Element::dom_id).collect();
        let live_from = ids.len().saturating_sub(LIVE_ROWS);
        let now = Timestamp::now();
        for (index, (row, id)) in rows.iter().zip(&ids).enumerate() {
            let Some(id) = id else {
                continue;
            };
            // Rows above the last few are final, except in a panel found again: those grew while
            // it was hidden.
            if !is_list_new && index < live_from && self.row_captions.contains_key(id) {
                continue;
            }
            let texts = row_texts(profile, row, level);
            if let Some(language) = texts.language {
                if self.language.as_deref() != Some(language.as_str()) {
                    self.language = Some(language.clone());
                    self.log.append(&LogLine::Language {
                        seen_at: now,
                        language,
                    })?;
                }
                continue;
            }
            let (Some(speaker), Some(text)) = (texts.speaker, texts.text) else {
                continue;
            };
            let caption = Caption { speaker, text };
            let previous = self.row_captions.get(id);
            if previous == Some(&caption) {
                continue;
            }
            if previous.is_some() {
                self.row_edits += 1;
            }
            self.log.append(&LogLine::Row {
                seen_at: now,
                row: id.clone(),
                speaker: caption.speaker.clone(),
                text: caption.text.clone(),
                backlog: is_backlog && previous.is_none(),
            })?;
            self.row_captions.insert(id.clone(), caption);
        }
        Ok(())
    }

    // Slack rewrites the newest overlay line while the person talks, sometimes changing earlier
    // words. A line is logged once it is final: when a newer line shows below it, or when the
    // overlay is no longer read.
    fn read_overlay(&mut self, profile: &LayoutProfile) -> std::io::Result<()> {
        let Some(Ok(elements)) = self.overlay.as_ref().map(Element::children) else {
            self.overlay = None;
            return self.flush_overlay();
        };
        let lines: Vec<Caption> = elements
            .iter()
            .filter(|line| line.has_class(&profile.overlay.line_class))
            .filter_map(overlay_line)
            .collect();
        let Some((newest, finals)) = lines.split_last() else {
            return self.flush_overlay();
        };
        let now = Timestamp::now();
        let live = self.overlay_live.take();
        // The live line's speaker got a new final line, so the live line was finished.
        let mut is_live_finished = false;
        for line in finals {
            let is_live_speaker = live
                .as_ref()
                .is_some_and(|live| live.caption.speaker == line.speaker);
            let seen_at = match &live {
                Some(live) if is_live_speaker && !is_live_finished => live.first_seen_at,
                Some(_) | None => now,
            };
            if self.log_overlay(line.clone(), seen_at)? && is_live_speaker {
                is_live_finished = true;
            }
        }
        let live = match live {
            Some(mut live) if !is_live_finished && live.caption.speaker == newest.speaker => {
                live.caption = newest.clone();
                live
            }
            previous => {
                // Another speaker took over and the old line never showed as final.
                if let Some(previous) = previous
                    && !is_live_finished
                {
                    self.log_overlay(previous.caption, previous.first_seen_at)?;
                }
                LiveOverlay {
                    caption: newest.clone(),
                    first_seen_at: now,
                }
            }
        };
        self.overlay_live = Some(live);
        Ok(())
    }

    /// Logs the newest overlay line, which no newer line will finish.
    fn flush_overlay(&mut self) -> std::io::Result<()> {
        match self.overlay_live.take() {
            Some(live) => self
                .log_overlay(live.caption, live.first_seen_at)
                .map(|_| ()),
            None => Ok(()),
        }
    }

    /// Returns whether the line was new. Final lines stay on screen for several reads.
    fn log_overlay(&mut self, caption: Caption, seen_at: Timestamp) -> std::io::Result<bool> {
        if self.overlay_logged.contains(&caption) {
            return Ok(false);
        }
        self.log.append(&LogLine::Overlay {
            seen_at,
            speaker: caption.speaker.clone(),
            text: caption.text.clone(),
        })?;
        self.overlay_lines += 1;
        self.overlay_logged.push_back(caption);
        if self.overlay_logged.len() > OVERLAY_MEMORY {
            self.overlay_logged.pop_front();
        }
        Ok(true)
    }
}

struct LiveOverlay {
    caption: Caption,
    first_seen_at: Timestamp,
}

/// An overlay line reads "<speaker>", ":", "<text>".
fn overlay_line(line: &Element) -> Option<Caption> {
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

/// One node of a walked window, with the attributes every check needs read once.
struct Node {
    element: Element,
    parent: Option<usize>,
    role: Option<String>,
    description: Option<String>,
    dom_id: Option<String>,
    classes: Vec<String>,
}

impl Node {
    fn has_class(&self, class: &str) -> bool {
        self.classes.iter().any(|known| known == class)
    }
}

/// Every node of `root`, breadth first. `None` when Slack is too busy to answer, or the tree is
/// not built yet. The caption list's rows are not walked: they grow without limit, and the list
/// is read on its own.
fn walk(root: &Element, profiles: &[Rc<LayoutProfile>]) -> Option<Vec<Node>> {
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

fn find_list(
    profile: &LayoutProfile,
    nodes: &[Node],
    tiles: &BTreeMap<String, Tile>,
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
    let names: Vec<&str> = tiles.values().map(|tile| tile.name.as_str()).collect();
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

#[derive(Default)]
struct RowTexts {
    speaker: Option<String>,
    text: Option<String>,
    language: Option<String>,
}

fn row_texts(profile: &LayoutProfile, row: &Element, level: ListMatch) -> RowTexts {
    let markers = &profile.transcript_list;
    let mut texts = RowTexts::default();
    match level {
        ListMatch::Semantic | ListMatch::Class => {
            for part in row.children().unwrap_or_default() {
                let classes = part.classes();
                let has = |class: &str| classes.iter().any(|known| known == class);
                if has(&markers.speaker_class) {
                    texts.speaker = Some(one_line(&static_texts(&part, 3).join("")));
                } else if has(&markers.text_class) {
                    texts.text = Some(one_line(&static_texts(&part, 3).join("")));
                } else if has(&markers.header_class) {
                    texts.language = static_texts(&part, 3).iter().find_map(|line| {
                        let language = line.strip_prefix(&markers.language_prefix)?;
                        Some(language.trim().trim_end_matches('.').trim().to_owned())
                    });
                }
            }
        }
        ListMatch::Structure => {
            let parts = static_texts(row, 4);
            if let Some((speaker, rest)) = parts.split_first() {
                texts.speaker = Some(one_line(speaker));
                texts.text = Some(one_line(&rest.join(" ")));
            }
        }
    }
    texts.speaker = texts.speaker.filter(|speaker| !speaker.is_empty());
    texts.text = texts.text.filter(|text| !text.is_empty());
    texts
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
