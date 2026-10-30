pub mod clock;
pub mod hal;
pub mod power;
pub mod tap;

use crate::CaptureError;

/// A failed Core Audio call, named so the log says which one.
pub(crate) fn os_error(call: &'static str, os_status: i32) -> CaptureError {
    CaptureError::CoreAudio { call, os_status }
}
