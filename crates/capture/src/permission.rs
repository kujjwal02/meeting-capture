//! Whether macOS lets this process record the mic and the system audio. Reads the state only:
//! nothing here prompts. A check runs at a segment start and after a failure, never on a timer.

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
