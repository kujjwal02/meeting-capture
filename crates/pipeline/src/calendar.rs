//! Calendar match. Google sign-in arrives in build step 4; until then no event matches.

use serde::{Deserialize, Serialize};
use sources::Transcript;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarMatch {
    pub event: Option<CalendarEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub title: String,
    pub attendees: Vec<String>,
}

pub(crate) fn match_event(_transcript: &Transcript) -> CalendarMatch {
    CalendarMatch { event: None }
}
