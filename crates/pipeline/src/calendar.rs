//! Calendar match: the event behind a transcript, searched across every signed-in account.

use std::cmp::Reverse;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use sources::{Platform, Transcript};

use crate::google::{self, ApiEvent, GoogleError, OAuthClient, ResponseStatus};
use crate::settings::GoogleSettings;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarMatch {
    pub event: Option<CalendarEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// `None` for an event with no title.
    pub title: Option<String>,
    /// Everyone the event invites, rooms left out. Matches stored before the split from the
    /// people who joined call this `attendees`.
    #[serde(alias = "attendees")]
    pub invited: Vec<Invitee>,
    /// The signed-in account whose calendar holds the event.
    pub account: String,
    pub event_id: String,
}

/// A person the event invites. Being invited says nothing about joining the call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "StoredInvitee")]
pub struct Invitee {
    /// The display name, else the email.
    pub name: String,
    /// `None` on matches stored before responses were kept.
    pub response: Option<InviteResponse>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InviteResponse {
    NeedsAction,
    Declined,
    Tentative,
    Accepted,
    Unrecognised,
}

impl From<ResponseStatus> for InviteResponse {
    fn from(status: ResponseStatus) -> Self {
        match status {
            ResponseStatus::NeedsAction => Self::NeedsAction,
            ResponseStatus::Declined => Self::Declined,
            ResponseStatus::Tentative => Self::Tentative,
            ResponseStatus::Accepted => Self::Accepted,
            ResponseStatus::Unrecognised => Self::Unrecognised,
        }
    }
}

// Older matches stored each invitee as a bare name.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredInvitee {
    Name(String),
    Invitee {
        name: String,
        response: Option<InviteResponse>,
    },
}

impl From<StoredInvitee> for Invitee {
    fn from(stored: StoredInvitee) -> Self {
        match stored {
            StoredInvitee::Name(name) => Self {
                name,
                response: None,
            },
            StoredInvitee::Invitee { name, response } => Self { name, response },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountStatus {
    pub email: String,
    /// The refresh token was revoked or is missing. Set by a failed refresh, cleared by sign-in.
    pub needs_sign_in: bool,
}

/// The signed-in accounts and whether each still works. The tray shows them; the settings window
/// changes them.
#[derive(Default)]
pub struct GoogleAccounts {
    state: Mutex<AccountsState>,
}

#[derive(Default)]
struct AccountsState {
    settings: GoogleSettings,
    accounts: Vec<AccountStatus>,
}

impl GoogleAccounts {
    /// Takes the client, the account list and the refresh tokens from new settings. Accounts
    /// already known keep their status.
    pub(crate) fn replace(&self, settings: &GoogleSettings) {
        let mut state = self.lock();
        let accounts = settings
            .accounts
            .iter()
            .map(|email| {
                state
                    .accounts
                    .iter()
                    .find(|known| &known.email == email)
                    .cloned()
                    .unwrap_or_else(|| AccountStatus {
                        email: email.clone(),
                        needs_sign_in: false,
                    })
            })
            .collect();
        *state = AccountsState {
            settings: settings.clone(),
            accounts,
        };
    }

    /// Records that `email` signed in again.
    pub fn signed_in(&self, email: &str) {
        self.set_needs_sign_in(email, false);
    }

    #[must_use]
    pub fn statuses(&self) -> Vec<AccountStatus> {
        self.lock().accounts.clone()
    }

    /// Refreshes each account's token, so the tray shows a revoked account before a call needs it.
    pub fn check(&self) {
        let (settings, accounts) = self.snapshot();
        let client = match OAuthClient::from_settings(&settings) {
            Ok(Some(client)) => client,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(exception.message = %error, "Google accounts need to sign in again");
                for account in &accounts {
                    self.set_needs_sign_in(&account.email, true);
                }
                return;
            }
        };
        for account in accounts {
            let refresh_token = settings
                .refresh_tokens
                .get(&account.email)
                .map(String::as_str);
            let checked = google::access_token(&client, &account.email, refresh_token);
            self.record_token_outcome(&account.email, checked.map(|_| ()));
        }
    }

    /// Finds the calendar event behind `transcript`. An account that must sign in again is
    /// skipped; with no usable account the match is empty.
    ///
    /// # Errors
    ///
    /// Returns `GoogleError` when Google cannot be reached, so the run can retry.
    pub(crate) fn match_event(
        &self,
        transcript: &Transcript,
    ) -> Result<CalendarMatch, GoogleError> {
        match transcript.platform {
            // A huddle often starts next to a meeting it has nothing to do with, so it keeps its
            // channel title and the people Slack showed.
            Platform::Slack => {
                tracing::info!("Slack huddles are not matched to calendar events");
                return Ok(CalendarMatch { event: None });
            }
            Platform::Meet | Platform::Teams | Platform::Zoom | Platform::Other => {}
        }
        let (settings, accounts) = self.snapshot();
        if accounts.is_empty() {
            tracing::info!("no Google account is signed in");
            return Ok(CalendarMatch { event: None });
        }
        let client = match OAuthClient::from_settings(&settings) {
            Ok(Some(client)) => client,
            Ok(None) => {
                tracing::info!("no Google client ID is set");
                return Ok(CalendarMatch { event: None });
            }
            Err(GoogleError::NoClientSecret) => {
                tracing::warn!("the Google client secret is missing");
                for account in &accounts {
                    self.set_needs_sign_in(&account.email, true);
                }
                return Ok(CalendarMatch { event: None });
            }
            Err(error) => return Err(error),
        };

        let mut candidates = Vec::new();
        for (account_rank, account) in accounts.iter().enumerate() {
            let refresh_token = settings
                .refresh_tokens
                .get(&account.email)
                .map(String::as_str);
            let events = google::access_token(&client, &account.email, refresh_token).and_then(
                |access_token| {
                    google::overlapping_events(
                        &access_token,
                        &account.email,
                        transcript.started_at,
                        transcript.ended_at,
                    )
                },
            );
            let events = match events {
                Ok(events) => events,
                Err(error @ GoogleError::SignInAgain { .. }) => {
                    self.record_token_outcome(&account.email, Err(error));
                    continue;
                }
                Err(error) => return Err(error),
            };
            self.set_needs_sign_in(&account.email, false);
            candidates.extend(
                events
                    .into_iter()
                    .filter_map(|event| Candidate::new(transcript, event, account_rank)),
            );
        }
        tracing::info!(
            calendar.candidates = candidates.len(),
            "calendar events found"
        );

        let best = candidates.into_iter().min_by_key(Candidate::rank);
        Ok(CalendarMatch {
            event: best.map(|candidate| candidate.into_event(&accounts)),
        })
    }

    fn snapshot(&self) -> (GoogleSettings, Vec<AccountStatus>) {
        let state = self.lock();
        (state.settings.clone(), state.accounts.clone())
    }

    fn record_token_outcome(&self, email: &str, outcome: Result<(), GoogleError>) {
        match outcome {
            Ok(()) => self.set_needs_sign_in(email, false),
            Err(GoogleError::SignInAgain { .. } | GoogleError::NoClientSecret) => {
                tracing::warn!(user.email = email, "Google account needs to sign in again");
                self.set_needs_sign_in(email, true);
            }
            Err(error) => tracing::warn!(
                user.email = email,
                exception.message = %error,
                "could not check the Google account"
            ),
        }
    }

    fn set_needs_sign_in(&self, email: &str, needs_sign_in: bool) {
        if let Some(account) = self
            .lock()
            .accounts
            .iter_mut()
            .find(|account| account.email == email)
        {
            account.needs_sign_in = needs_sign_in;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AccountsState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Candidate {
    event: ApiEvent,
    account_rank: usize,
    overlap_s: i64,
    is_same_meet: bool,
    has_meet_link: bool,
}

impl Candidate {
    fn new(transcript: &Transcript, event: ApiEvent, account_rank: usize) -> Option<Self> {
        // All-day events carry no time and say nothing about a call.
        let (Some(event_start), Some(event_end)) = (event.start.date_time, event.end.date_time)
        else {
            return None;
        };
        let has_declined = event.attendees.iter().any(|attendee| {
            attendee.is_self && attendee.response_status == Some(ResponseStatus::Declined)
        });
        if has_declined {
            return None;
        }
        let overlap_s = event_end.min(transcript.ended_at).as_second()
            - event_start.max(transcript.started_at).as_second();
        if overlap_s <= 0 {
            return None;
        }
        let meet_links: Vec<&str> = event
            .hangout_link
            .iter()
            .chain(
                event
                    .conference_data
                    .iter()
                    .flat_map(|conference| &conference.entry_points)
                    .filter_map(|entry_point| entry_point.uri.as_ref()),
            )
            .map(|link| link.trim_end_matches('/'))
            .filter(|link| link.starts_with("https://meet.google.com/"))
            .collect();
        let is_same_meet = transcript
            .meeting_url
            .as_deref()
            .is_some_and(|meeting_url| meet_links.contains(&meeting_url));
        Some(Self {
            has_meet_link: !meet_links.is_empty(),
            event,
            account_rank,
            overlap_s,
            is_same_meet,
        })
    }

    // Lowest wins. The same Meet code beats any overlap: two back-to-back events both overlap a
    // call that ran late, and only one is the call. Then the largest overlap, then an event with
    // a Meet link, then the account listed first.
    fn rank(&self) -> (Reverse<bool>, Reverse<i64>, Reverse<bool>, usize) {
        (
            Reverse(self.is_same_meet),
            Reverse(self.overlap_s),
            Reverse(self.has_meet_link),
            self.account_rank,
        )
    }

    fn into_event(self, accounts: &[AccountStatus]) -> CalendarEvent {
        let invited = self
            .event
            .attendees
            .into_iter()
            .filter(|attendee| !attendee.resource)
            .filter_map(|attendee| {
                Some(Invitee {
                    name: attendee.display_name.or(attendee.email)?,
                    response: attendee.response_status.map(InviteResponse::from),
                })
            })
            .collect();
        CalendarEvent {
            title: self
                .event
                .summary
                .map(|summary| summary.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|summary| !summary.is_empty()),
            invited,
            account: accounts[self.account_rank].email.clone(),
            event_id: self.event.id,
        }
    }
}
