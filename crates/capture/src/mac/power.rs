//! Holds off idle system sleep while a segment's stems are open. The kernel drops the assertion
//! when the process dies, so a killed helper never keeps the Mac awake.

use cidre::{cf, io};

use crate::CaptureError;

/// Idle system sleep stays off while this lives. The display may still sleep.
pub struct IdleSleepBlock {
    _assertion: io::pm::Assertion,
}

impl IdleSleepBlock {
    /// `reason` is what `pmset -g assertions` shows.
    ///
    /// # Errors
    ///
    /// Fails when the power manager refuses the assertion.
    pub fn hold(reason: &str) -> Result<Self, CaptureError> {
        let name = cf::String::from_str(reason);
        let assertion = io::pm::Assertion::with_name(
            &name,
            io::pm::assertions::prevent_user_idle_system_sleep(),
            io::pm::AssertionLevel::ON,
        )
        .map_err(|code| CaptureError::PowerAssertion { code: code.0 })?;
        Ok(Self {
            _assertion: assertion,
        })
    }
}
