//! Whether macOS lets this process record the mic, the system audio and a window. The checks
//! read the state only and run at a segment start and after a failure, never on a timer. The one
//! prompt is `request_screen_recording`, which the app runs once at launch.

use stem_timing::{PermissionService, PermissionState};

/// The permission's state for this process. macOS files a helper's grants under the app that
/// launched it, so this reads the app's grant.
#[must_use]
pub fn permission_state(service: PermissionService) -> PermissionState {
    #[cfg(target_os = "macos")]
    return crate::mac::permission::state(service);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = service;
        PermissionState::Unknown
    }
}

/// Asks macOS for Screen Recording, with the system dialog when the app has never been asked,
/// and returns the state afterwards. The dialog is the only way the app ever gets on the Screen
/// Recording list: the preflight never adds it. Elsewhere than macOS nothing is asked.
#[must_use]
pub fn request_screen_recording() -> PermissionState {
    #[cfg(target_os = "macos")]
    return crate::mac::permission::request_screen_recording();
    #[cfg(not(target_os = "macos"))]
    PermissionState::Unknown
}
