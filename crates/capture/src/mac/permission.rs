//! The mic state comes from `AVCaptureDevice`. System audio has no public call, so its state
//! comes from the private `TCCAccessPreflight`, loaded with dlopen so a macOS without it only
//! loses the check. Unsafe code: dlopen, dlsym and the call through the found pointer.
#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

use cidre::{av, cf};

use crate::{PermissionService, PermissionState};

const TCC_FRAMEWORK: &CStr = c"/System/Library/PrivateFrameworks/TCC.framework/TCC";
const PREFLIGHT_SYMBOL: &CStr = c"TCCAccessPreflight";
const AUDIO_CAPTURE_SERVICE: &str = "kTCCServiceAudioCapture";
const RTLD_LAZY: c_int = 1;

type Preflight = unsafe extern "C" fn(service: &cf::String, options: *const c_void) -> c_int;

pub(crate) fn state(service: PermissionService) -> PermissionState {
    let state = match service {
        PermissionService::Microphone => microphone_state(),
        PermissionService::AudioCapture => audio_capture_state(),
    };
    tracing::info!(
        permission.service = %service,
        permission.state = %state,
        "permission.checked"
    );
    state
}

fn microphone_state() -> PermissionState {
    match av::CaptureDevice::authorization_status_for_media_type(av::MediaType::audio()) {
        Ok(av::AuthorizationStatus::Authorized) => PermissionState::Granted,
        Ok(av::AuthorizationStatus::Denied) => PermissionState::Denied,
        Ok(av::AuthorizationStatus::NotDetermined) => PermissionState::NotDetermined,
        Ok(av::AuthorizationStatus::Restricted) => PermissionState::Restricted,
        Err(exception) => {
            tracing::warn!(exception.message = ?exception, "permission.mic_read_failed");
            PermissionState::Unknown
        }
    }
}

fn audio_capture_state() -> PermissionState {
    let Some(preflight) = preflight() else {
        return PermissionState::Unknown;
    };
    let service = cf::String::from_str(AUDIO_CAPTURE_SERVICE);
    // SAFETY: the pointer came from dlsym for this symbol, whose C signature is
    // `int TCCAccessPreflight(CFStringRef, CFDictionaryRef)`. The string outlives the call.
    let code = unsafe { preflight(&service, std::ptr::null()) };
    // 0 and 1 are the codes AudioCap and other tap recorders read; any other code is logged.
    match code {
        0 => PermissionState::Granted,
        1 => PermissionState::Denied,
        _ => {
            tracing::info!(
                permission.preflight_code = code,
                "permission.preflight_unknown"
            );
            PermissionState::Unknown
        }
    }
}

fn preflight() -> Option<Preflight> {
    static PREFLIGHT: OnceLock<Option<Preflight>> = OnceLock::new();
    *PREFLIGHT.get_or_init(|| {
        // SAFETY: both arguments are valid C strings. The handle is never closed, so the symbol
        // stays loaded for the process's life.
        let handle = unsafe { dlopen(TCC_FRAMEWORK.as_ptr(), RTLD_LAZY) };
        if handle.is_null() {
            tracing::warn!("permission.tcc_missing");
            return None;
        }
        // SAFETY: the handle is live and the name is a valid C string.
        let symbol = unsafe { dlsym(handle, PREFLIGHT_SYMBOL.as_ptr()) };
        if symbol.is_null() {
            tracing::warn!("permission.preflight_missing");
            return None;
        }
        // SAFETY: a non-null dlsym result for this symbol is its function, with the signature
        // `Preflight` names.
        Some(unsafe { std::mem::transmute::<*mut c_void, Preflight>(symbol) })
    })
}

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}
