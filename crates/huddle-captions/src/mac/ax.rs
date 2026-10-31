//! Safe wrappers over the macOS accessibility API. The only module with unsafe code.
#![allow(unsafe_code)]

use std::path::Path;
use std::ptr::NonNull;

use objc2_app_kit::NSRunningApplication;
use objc2_application_services::{
    AXError, AXIsProcessTrusted, AXIsProcessTrustedWithOptions, AXUIElement,
};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_foundation::NSString;

/// One node of another app's accessibility tree. Two elements are equal when they are the same
/// node.
#[derive(Clone)]
pub(crate) struct Element(CFRetained<AXUIElement>);

impl PartialEq for Element {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}

// A busy Slack would otherwise block each read for the 6 s default, and a walk reads thousands.
const MESSAGING_TIMEOUT_S: f32 = 0.5;

/// The node is gone, or the app stopped answering. Both mean "look again".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AxFailure(pub i32);

impl From<AXError> for AxFailure {
    fn from(error: AXError) -> Self {
        Self(error.0)
    }
}

impl AxFailure {
    pub(crate) fn is_gone(self) -> bool {
        self.0 == AXError::InvalidUIElement.0
    }

    /// Slack did not answer in time. The rest of a walk would wait as long for each read.
    pub(crate) fn is_busy(self) -> bool {
        self.0 == AXError::CannotComplete.0
    }
}

impl Element {
    pub(crate) fn application(pid: i32) -> Self {
        // SAFETY: any pid is accepted; an unknown one gives an element whose reads fail.
        let app = unsafe { AXUIElement::new_application(pid) };
        // SAFETY: the timeout is positive. Elements read through `app` inherit it.
        let _ = unsafe { app.set_messaging_timeout(MESSAGING_TIMEOUT_S) };
        Self(app)
    }

    fn attribute(&self, name: &str) -> Result<CFRetained<CFType>, AxFailure> {
        let name = CFString::from_str(name);
        let mut value: *const CFType = std::ptr::null();
        // SAFETY: `value` is a valid out pointer; on success it holds a +1 reference we own.
        let status = unsafe {
            self.0
                .copy_attribute_value(&name, NonNull::from(&mut value))
        };
        if status != AXError::Success {
            return Err(status.into());
        }
        let value = NonNull::new(value.cast_mut()).ok_or(AxFailure(AXError::NoValue.0))?;
        // SAFETY: the copy rule hands us ownership of the reference.
        Ok(unsafe { CFRetained::from_raw(value) })
    }

    /// The window server's id of a window element, which `ScreenCaptureKit` lists windows by.
    /// Accessibility has no public call for it; every window recorder uses this private one.
    pub(crate) fn window_id(&self) -> Option<u32> {
        let mut window_id = 0_u32;
        // SAFETY: the element is a live AXUIElement and `window_id` a valid out pointer. The
        // call reads the element and writes the id only.
        let status = unsafe { _AXUIElementGetWindow(&self.0, &raw mut window_id) };
        (status == AXError::Success.0 && window_id != 0).then_some(window_id)
    }

    pub(crate) fn string(&self, name: &str) -> Option<String> {
        let value = self.attribute(name).ok()?;
        value
            .downcast::<CFString>()
            .ok()
            .map(|text| text.to_string())
    }

    pub(crate) fn role(&self) -> Option<String> {
        self.string("AXRole")
    }

    pub(crate) fn title(&self) -> Option<String> {
        self.string("AXTitle")
    }

    pub(crate) fn description(&self) -> Option<String> {
        self.string("AXDescription")
    }

    pub(crate) fn value_text(&self) -> Option<String> {
        self.string("AXValue")
    }

    /// The DOM id Electron exposes, such as `huddle-grid-gridcell-self_U0123ABCDEF`.
    pub(crate) fn dom_id(&self) -> Option<String> {
        self.string("AXDOMIdentifier")
    }

    /// The DOM classes Electron exposes.
    pub(crate) fn classes(&self) -> Vec<String> {
        self.array("AXDOMClassList")
            .map(|items| {
                items
                    .into_iter()
                    .filter_map(|item| item.downcast::<CFString>().ok())
                    .map(|text| text.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn has_class(&self, class: &str) -> bool {
        self.classes().iter().any(|known| known == class)
    }

    pub(crate) fn children(&self) -> Result<Vec<Element>, AxFailure> {
        self.elements("AXChildren")
    }

    pub(crate) fn windows(&self) -> Result<Vec<Element>, AxFailure> {
        self.elements("AXWindows")
    }

    fn elements(&self, name: &str) -> Result<Vec<Element>, AxFailure> {
        Ok(self
            .array(name)?
            .into_iter()
            .filter_map(|item| item.downcast::<AXUIElement>().ok())
            .map(Element)
            .collect())
    }

    fn array(&self, name: &str) -> Result<Vec<CFRetained<CFType>>, AxFailure> {
        let value = self.attribute(name)?;
        let array = value
            .downcast::<CFArray>()
            .map_err(|_| AxFailure(AXError::IllegalArgument.0))?;
        // SAFETY: every CFArray holds CF objects.
        let array: CFRetained<CFArray<CFType>> = unsafe { CFRetained::cast_unchecked(array) };
        Ok(array.to_vec())
    }

    /// Asks an Electron app to build its accessibility tree. Electron builds it only for a client
    /// that sets this, and the first read after it can still be empty.
    pub(crate) fn enable_manual_accessibility(&self) {
        let name = CFString::from_str("AXManualAccessibility");
        // SAFETY: both arguments are valid CF objects.
        let _ = unsafe { self.0.set_attribute_value(&name, CFBoolean::new(true)) };
    }

    pub(crate) fn press(&self) -> Result<(), AxFailure> {
        let action = CFString::from_str("AXPress");
        // SAFETY: the action name is a valid CFString.
        let status = unsafe { self.0.perform_action(&action) };
        if status == AXError::Success {
            Ok(())
        } else {
            Err(status.into())
        }
    }
}

/// Whether the user granted this app the Accessibility permission.
pub(crate) fn is_trusted() -> bool {
    // SAFETY: no arguments.
    unsafe { AXIsProcessTrusted() }
}

/// Shows the system prompt that sends the user to the Accessibility settings. It returns at once.
pub(crate) fn ask_for_trust() {
    let key = CFString::from_str("AXTrustedCheckOptionPrompt");
    let options =
        CFDictionary::<CFString, CFBoolean>::from_slices(&[&key], &[CFBoolean::new(true)]);
    // SAFETY: the dictionary maps the documented key to a CFBoolean.
    let _ = unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) };
}

/// The running Slack desktop app: its pid and version.
pub(crate) struct SlackProcess {
    pub pid: i32,
    pub version: String,
}

const SLACK_BUNDLE_ID: &str = "com.tinyspeck.slackmacgap";

pub(crate) fn find_slack() -> Option<SlackProcess> {
    let bundle_id = NSString::from_str(SLACK_BUNDLE_ID);
    let running = NSRunningApplication::runningApplicationsWithBundleIdentifier(&bundle_id);
    let slack = running.iter().next()?;
    let version = slack
        .bundleURL()
        .and_then(|url| url.path())
        .and_then(|path| bundle_version(Path::new(&path.to_string())))
        .unwrap_or_else(|| "unknown".to_owned());
    Some(SlackProcess {
        pid: slack.processIdentifier(),
        version,
    })
}

// Read from the file each time: NSBundle caches a bundle by path for the life of the process, so
// it would keep the old version after Slack updates itself.
fn bundle_version(bundle: &Path) -> Option<String> {
    let plist = std::fs::read_to_string(bundle.join("Contents/Info.plist")).ok()?;
    let after_key = plist.split_once("<key>CFBundleShortVersionString</key>")?.1;
    let value = after_key.trim_start().strip_prefix("<string>")?;
    let (version, _) = value.split_once("</string>")?;
    Some(version.trim().to_owned())
}

unsafe extern "C" {
    /// Private in `HIServices` since macOS 10.x; Loom, OBS and Hammerspoon call it.
    fn _AXUIElementGetWindow(element: &AXUIElement, window_id: *mut u32) -> i32;
}
