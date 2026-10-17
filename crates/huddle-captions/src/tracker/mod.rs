//! The OS-free half of a huddle: caption bookkeeping, the caption panel's state and the health
//! verdict. An OS reader turns what it sees into the plain observations here and passes the time
//! in. The tracker writes the huddle log and returns health changes for the watcher to report.

mod captions;
mod health;
mod panel;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use jiff::Timestamp;
use sources::slack_huddle::{self, LogLine};

pub(crate) use self::health::HealthChange;
use crate::Notice;
use crate::log::HuddleLog;
use crate::profile::LayoutProfile;

/// One reading of the clock, so every check in a step sees the same time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Moment {
    /// For waits and timers.
    pub(crate) instant: Instant,
    /// For the log.
    pub(crate) seen_at: Timestamp,
}

impl Moment {
    pub(crate) fn now() -> Self {
        Self {
            instant: Instant::now(),
            seen_at: Timestamp::now(),
        }
    }
}

/// How the caption list was found. Each level down survives more Slack changes and trusts the
/// result less.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListMatch {
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
pub(crate) struct Caption {
    pub(crate) speaker: String,
    pub(crate) text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tile {
    pub(crate) name: String,
    pub(crate) is_self: bool,
}

/// What one row of the caption list says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowSight {
    /// The header row that names the caption language.
    Language(String),
    Caption(Caption),
    /// A row without both a speaker and a text, such as one still being built.
    Unreadable,
}

/// What one walk of the huddle window found.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowSight {
    /// `None` while the caption panel is hidden.
    pub(crate) list: Option<ListMatch>,
    pub(crate) has_overlay: bool,
}

/// A huddle window's title, "<channel> - <workspace> - Slack", and its parts.
pub(crate) struct WindowTitle {
    pub(crate) title: String,
    pub(crate) channel: String,
    pub(crate) workspace: String,
}

pub(crate) struct HuddleTracker {
    log: HuddleLog,
    slack_version: String,
    profile: Option<Rc<LayoutProfile>>,
    panel: panel::Panel,
    book: captions::Book,
    /// The list was found since the last row read, so every row is read, not just the last few.
    unread_list: Option<ListFind>,
    judge: health::Judge,
    /// A log that cannot be written fails again every poll; one notification is enough.
    has_reported_log_error: bool,
}

impl HuddleTracker {
    /// Creates the huddle's log and writes its first line.
    pub(crate) fn start(
        log_dir: &Path,
        moment: Moment,
        slack_version: String,
        profile: Option<Rc<LayoutProfile>>,
        window_title: WindowTitle,
    ) -> std::io::Result<Self> {
        let mut log = HuddleLog::create(log_dir, moment.seen_at)?;
        log.append(&LogLine::Started {
            schema: slack_huddle::SCHEMA,
            seen_at: moment.seen_at,
            slack_version: slack_version.clone(),
            layout_profile: profile_id(profile.as_deref()).to_owned(),
            window_title: window_title.title,
            channel: window_title.channel,
            workspace: window_title.workspace,
        })?;
        Ok(Self {
            log,
            slack_version,
            profile,
            panel: panel::Panel::default(),
            book: captions::Book::default(),
            unread_list: None,
            judge: health::Judge::new(moment.instant),
            has_reported_log_error: false,
        })
    }

    pub(crate) fn log_path(&self) -> &Path {
        self.log.path()
    }

    pub(crate) fn profile(&self) -> Option<Rc<LayoutProfile>> {
        self.profile.clone()
    }

    pub(crate) fn layout_profile_id(&self) -> &str {
        profile_id(self.profile.as_deref())
    }

    pub(crate) fn report_log_error(
        &mut self,
        notify: &mut impl FnMut(Notice),
        error: &std::io::Error,
    ) {
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

    /// A layout profile matched. The first match after the start is logged; the tracker keeps
    /// that one. A late match means the window's tree was still being built at the start.
    pub(crate) fn saw_profile(
        &mut self,
        profile: Rc<LayoutProfile>,
        seen_at: Timestamp,
    ) -> std::io::Result<()> {
        if self.profile.is_some() {
            return Ok(());
        }
        tracing::info!(
            slack.layout_profile = profile.id,
            "Slack huddle layout matched"
        );
        self.log.append(&LogLine::LayoutProfile {
            seen_at,
            layout_profile: profile.id.clone(),
        })?;
        self.profile = Some(profile);
        Ok(())
    }

    /// Takes the participant tiles by Slack user id.
    pub(crate) fn saw_tiles(
        &mut self,
        tiles: BTreeMap<String, Tile>,
        seen_at: Timestamp,
    ) -> std::io::Result<()> {
        self.book.saw_tiles(&mut self.log, tiles, seen_at)
    }

    pub(crate) fn tile_names(&self) -> impl Iterator<Item = &str> {
        self.book.tile_names()
    }

    /// The caption list showed, so the next row read covers every row.
    pub(crate) fn list_found(&mut self) {
        self.unread_list = Some(self.panel.found());
    }

    /// The caption list is gone: the panel was hidden or the window closed.
    pub(crate) fn list_lost(&mut self, now: Instant) {
        self.panel.lost(now);
    }

    /// Takes the next press turn when one is due, and returns whether it did. After the panel
    /// was shown, taking a turn restarts the wait, so the next turn comes one wait later.
    pub(crate) fn claim_press_turn(&mut self, now: Instant) -> bool {
        self.panel.claim_press_turn(now)
    }

    /// Counts a press attempt, which starts the wait for the next one.
    pub(crate) fn count_press(&mut self, now: Instant) {
        self.panel.count_press(now);
    }

    pub(crate) fn captions_tab_pressed(&mut self, seen_at: Timestamp) -> std::io::Result<()> {
        self.log.append(&LogLine::CaptionsTabPressed { seen_at })
    }

    /// Works out the health from one walk of the window.
    pub(crate) fn saw_window(
        &mut self,
        sight: WindowSight,
        moment: Moment,
    ) -> std::io::Result<Option<HealthChange>> {
        let Some(profile) = &self.profile else {
            return self.saw_unreadable_window(moment);
        };
        let verdict = self.judge.window_verdict(
            sight,
            &self.panel,
            self.book.has_tiles(),
            profile,
            &self.slack_version,
            moment.instant,
        );
        self.set_health(verdict, moment)
    }

    /// Works out the health when no layout profile matches the window, so nothing could be read.
    /// The huddle is broken once the captions grace has passed.
    pub(crate) fn saw_unreadable_window(
        &mut self,
        moment: Moment,
    ) -> std::io::Result<Option<HealthChange>> {
        let verdict = self
            .judge
            .no_profile_verdict(&self.slack_version, moment.instant);
        self.set_health(verdict, moment)
    }

    /// Works out the health while the user has closed the huddle window and the huddle goes on.
    pub(crate) fn saw_window_closed(
        &mut self,
        moment: Moment,
    ) -> std::io::Result<Option<HealthChange>> {
        let verdict = health::closed_window_verdict(&self.panel, moment.instant);
        self.set_health(verdict, moment)
    }

    /// Reads the caption list's rows, given their ids in list order. Calls `read_row` with a
    /// row's index only for rows that may have changed, since each read costs the OS reader.
    pub(crate) fn saw_rows(
        &mut self,
        row_ids: &[Option<String>],
        read_row: impl FnMut(usize) -> RowSight,
        seen_at: Timestamp,
    ) -> std::io::Result<()> {
        let found = self.unread_list.take();
        self.book
            .saw_rows(&mut self.log, row_ids, found, read_row, seen_at)
    }

    /// Takes the overlay's lines, oldest first. No lines means the overlay is gone or empty.
    pub(crate) fn saw_overlay(
        &mut self,
        lines: &[Caption],
        seen_at: Timestamp,
    ) -> std::io::Result<()> {
        self.book.saw_overlay(&mut self.log, lines, seen_at)
    }

    /// Logs the overlay's newest line, which no newer line will finish: the caption list is read
    /// instead.
    pub(crate) fn flush_overlay(&mut self) -> std::io::Result<()> {
        self.book.flush_overlay(&mut self.log)
    }

    /// Writes the last overlay line and the `ended` line, and returns the log's path.
    pub(crate) fn end(mut self, ended_at: Timestamp, notify: &mut impl FnMut(Notice)) -> PathBuf {
        let written = self
            .book
            .flush_overlay(&mut self.log)
            .and_then(|()| self.log.append(&LogLine::Ended { seen_at: ended_at }));
        if let Err(error) = written {
            self.report_log_error(notify, &error);
        }
        self.book.trace_ended();
        self.log.path().to_path_buf()
    }

    /// Logs a health change and returns it for the watcher to report.
    fn set_health(
        &mut self,
        verdict: Option<HealthChange>,
        moment: Moment,
    ) -> std::io::Result<Option<HealthChange>> {
        let Some(change) = self.judge.settle(verdict, moment.instant) else {
            return Ok(None);
        };
        tracing::info!(
            slack.version = self.slack_version,
            slack.layout_profile = profile_id(self.profile.as_deref()),
            huddle.health = change.state.as_str(),
            "Slack huddle health changed"
        );
        self.log.append(&LogLine::Health {
            seen_at: moment.seen_at,
            state: change.state,
            reason: change.reason.clone(),
        })?;
        Ok(Some(change))
    }
}

fn profile_id(profile: Option<&LayoutProfile>) -> &str {
    profile.map_or("none", |profile| &profile.id)
}
