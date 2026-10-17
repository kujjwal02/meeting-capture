//! Finds Slack and its huddle windows, follows the Accessibility permission, and starts and ends
//! huddles.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use sources::slack_huddle;

use super::ax::{self, Element, SlackProcess};
use super::huddle::Huddle;
use super::tree;
use crate::profile::{self, LayoutProfile};
use crate::tracker::{HealthChange, Moment, WindowTitle};
use crate::{Notice, WatchPaths, WatchSettings};

// Walking a window costs one IPC call per attribute per node, so windows are walked less often
// than the cached caption list is read.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);
const PERMISSION_CHECK_INTERVAL: Duration = Duration::from_secs(5);
// One failed read of Slack's window list or a busy Slack must not end a huddle.
const MISSED_DISCOVERIES_TO_END: u32 = 3;

pub(super) struct Watcher<N> {
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
    pub(super) fn new(paths: &WatchPaths, notify: N) -> Self {
        let (profiles, errors) = profile::load(&paths.layout_override_dir);
        let mut watcher = Self {
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
        watcher
    }

    pub(super) fn tick(&mut self, settings: WatchSettings) {
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
                huddle.tracker.report_log_error(&mut self.notify, &error);
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
            if !tree::is_child_window(window, &self.profiles) {
                return None;
            }
            place = Some(WindowTitle {
                title,
                channel,
                workspace,
            });
        }
        let Some(nodes) = tree::walk(window, &self.profiles) else {
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
            (None, Some(window_title)) => {
                // A conversation opened in its own window is a child window too. An unreadable
                // child window counts as a huddle only while Slack offers "Leave huddle".
                if profile.is_none() && !tree::is_in_huddle(windows, Some(window), &self.profiles) {
                    return None;
                }
                self.attach_window(slack, window, window_title, profile.clone())?
            }
            (None, None) => return None,
        };
        let huddle = &mut self.huddles[index];
        huddle.missed_discoveries = 0;
        huddle.last_seen_at = Timestamp::now();
        let checked = profile
            .map_or(Ok(()), |profile| {
                huddle.tracker.saw_profile(profile, Timestamp::now())
            })
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
        window_title: WindowTitle,
        profile: Option<Rc<LayoutProfile>>,
    ) -> Option<usize> {
        if let Some(index) = self
            .huddles
            .iter()
            .position(|huddle| huddle.window.is_none() && huddle.channel == window_title.channel)
        {
            tracing::info!("Slack huddle window reopened");
            self.huddles[index].window = Some(window.clone());
            return Some(index);
        }
        match Huddle::start(&self.log_dir, slack, window, window_title, profile) {
            Ok(huddle) => {
                self.has_reported_start_error = false;
                tracing::info!(
                    slack.version = slack.version,
                    slack.layout_profile = huddle.tracker.layout_profile_id(),
                    "Slack huddle started"
                );
                (self.notify)(Notice::HuddleStarted {
                    log_path: huddle.tracker.log_path().to_path_buf(),
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
            let is_in_huddle = *is_in_huddle
                .get_or_insert_with(|| tree::is_in_huddle(windows, None, &self.profiles));
            if !is_in_huddle {
                over.push(huddle.tracker.log_path().to_path_buf());
                continue;
            }
            let moment = Moment::now();
            huddle.last_seen_at = moment.seen_at;
            if huddle.window.take().is_some() {
                tracing::info!("Slack huddle window closed; the huddle goes on");
                huddle.detach(moment.instant);
            }
            let checked = huddle.tracker.saw_window_closed(moment);
            self.report_check(index, checked);
        }
        self.end_huddles(|huddle| over.iter().any(|path| path == huddle.tracker.log_path()));
    }

    fn report_check(&mut self, index: usize, checked: std::io::Result<Option<HealthChange>>) {
        let huddle = &mut self.huddles[index];
        match checked {
            Ok(None) => {}
            Ok(Some(HealthChange { state, reason })) => {
                (self.notify)(Notice::Health {
                    log_path: huddle.tracker.log_path().to_path_buf(),
                    channel: huddle.channel.clone(),
                    state,
                    reason,
                });
            }
            Err(error) => huddle.tracker.report_log_error(&mut self.notify, &error),
        }
    }

    fn end_huddles(&mut self, is_over: impl Fn(&Huddle) -> bool) {
        let (ended, open): (Vec<Huddle>, Vec<Huddle>) = std::mem::take(&mut self.huddles)
            .into_iter()
            .partition(is_over);
        self.huddles = open;
        for huddle in ended {
            let ended_at = if huddle.missed_discoveries > 0 {
                huddle.last_seen_at
            } else {
                Timestamp::now()
            };
            let log_path = huddle.tracker.end(ended_at, &mut self.notify);
            (self.notify)(Notice::HuddleEnded {
                log_path,
                channel: huddle.channel,
            });
        }
    }
}
