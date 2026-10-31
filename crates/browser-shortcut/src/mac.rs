//! The macOS sender: Accessibility trust, the front app, and key events by pid.

use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_core_graphics::{CGEvent, CGEventFlags, CGKeyCode};

use crate::{Key, Modifier, Modifiers, PressError, Shortcut};

/// Reads the grant only. Never the variant with the prompt option: the app must not raise a
/// permission prompt.
#[allow(unsafe_code)]
pub(crate) fn is_accessibility_trusted() -> bool {
    // SAFETY: no arguments; it reads this process's grant.
    unsafe { objc2_application_services::AXIsProcessTrusted() }
}

// The callers run on worker threads, which have no autorelease pool of their own.
pub(crate) fn frontmost_pid() -> Option<u32> {
    autoreleasepool(|_| {
        let front = NSWorkspace::sharedWorkspace().frontmostApplication()?;
        u32::try_from(front.processIdentifier()).ok()
    })
}

pub(crate) fn bundle_id_of(pid: u32) -> Option<String> {
    let pid = i32::try_from(pid).ok()?;
    autoreleasepool(|_| {
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
        app.bundleIdentifier().map(|id| id.to_string())
    })
}

pub(crate) fn press(pid: u32, shortcut: Shortcut) -> Result<(), PressError> {
    let target = i32::try_from(pid).map_err(|_| PressError::InvalidPid { pid })?;
    let key_code = key_code(shortcut.key, shortcut.modifiers.key_translate_state())?;
    let flags = shortcut.modifiers.flags();
    for is_down in [true, false] {
        let event = CGEvent::new_keyboard_event(None, key_code, is_down)
            .ok_or(PressError::EventNotCreated)?;
        CGEvent::set_flags(Some(&event), flags);
        CGEvent::post_to_pid(target, Some(&event));
    }
    Ok(())
}

impl Modifiers {
    fn flags(self) -> CGEventFlags {
        let mut flags = CGEventFlags::empty();
        for modifier in Modifier::ALL {
            if self.is_held(modifier) {
                flags |= match modifier {
                    Modifier::Control => CGEventFlags::MaskControl,
                    Modifier::Option => CGEventFlags::MaskAlternate,
                    Modifier::Shift => CGEventFlags::MaskShift,
                    Modifier::Command => CGEventFlags::MaskCommand,
                };
            }
        }
        flags
    }

    /// The Command and Shift bits of Carbon's modifier state, shifted right by 8 as
    /// `UCKeyTranslate` takes them. A layout such as Dvorak - QWERTY Command types other
    /// characters while Command is held. Option and Control are left out: Option turns a key
    /// into another character and Control into a control code, which never match the shortcut.
    fn key_translate_state(self) -> u32 {
        let mut state = 0;
        for modifier in Modifier::ALL {
            if self.is_held(modifier) {
                state |= match modifier {
                    Modifier::Command => layout::COMMAND_STATE,
                    Modifier::Shift => layout::SHIFT_STATE,
                    Modifier::Control | Modifier::Option => 0,
                };
            }
        }
        state
    }
}

/// The virtual key code to post. Chrome matches a letter, digit or punctuation shortcut by the
/// character the key types on the current layout, so those keys are looked up on it: on AZERTY
/// the key that types "q" is not the US one. The other keys have one code on every layout,
/// from the `HIToolbox` header `Events.h`.
fn key_code(key: Key, modifier_state: u32) -> Result<CGKeyCode, PressError> {
    match key {
        Key::Letter(letter) => key_code_on_layout(char::from(letter), modifier_state),
        Key::Digit(digit) => key_code_on_layout(char::from(digit), modifier_state),
        Key::Comma => key_code_on_layout(',', modifier_state),
        Key::Period => key_code_on_layout('.', modifier_state),
        Key::Space => Ok(0x31),
        Key::Home => Ok(0x73),
        Key::PageUp => Ok(0x74),
        Key::ForwardDelete => Ok(0x75),
        Key::End => Ok(0x77),
        Key::PageDown => Ok(0x79),
        Key::Left => Ok(0x7B),
        Key::Right => Ok(0x7C),
        Key::Down => Ok(0x7D),
        Key::Up => Ok(0x7E),
    }
}

/// Shift turns a digit or a comma into another character on most layouts, so a key that types
/// the shortcut's character only without the modifiers is the next best match.
fn key_code_on_layout(character: char, modifier_state: u32) -> Result<CGKeyCode, PressError> {
    on_main_thread(move || {
        layout::key_code_typing(character, modifier_state)
            .or_else(|| layout::key_code_typing(character, 0))
    })
    .flatten()
    .ok_or(PressError::KeyNotOnLayout { key: character })
}

/// Runs `work` on the main thread. macOS asserts that the Text Input Source calls run there.
fn on_main_thread<R: Send>(work: impl FnOnce() -> R + Send) -> Option<R> {
    if objc2::MainThreadMarker::new().is_some() {
        return Some(work());
    }
    let mut outcome = None;
    // The tray app's main thread runs the AppKit loop, which drains the main queue. The callers
    // are worker threads, so this cannot wait on itself.
    dispatch2::DispatchQueue::main().exec_sync(|| outcome = Some(work()));
    outcome
}

/// The current keyboard layout, read through Carbon's Text Input Source and Unicode utilities.
/// No Rust binding covers them, so the calls are declared here.
#[allow(unsafe_code)]
mod layout {
    use std::ffi::c_void;

    use objc2_core_graphics::CGKeyCode;

    /// The main typing block. Higher codes are the keypad and function keys, which a layout does
    /// not move.
    const TYPING_KEY_CODES: std::ops::RangeInclusive<u16> = 0x00..=0x32;
    /// `kUCKeyActionDisplay`: the character the key shows, with no dead key state.
    const KEY_ACTION_DISPLAY: u16 = 3;
    /// `kUCKeyTranslateNoDeadKeysMask`.
    const NO_DEAD_KEYS: u32 = 1;
    /// `cmdKey` (0x100) and `shiftKey` (0x200) from `Events.h`, shifted right by 8.
    pub(super) const COMMAND_STATE: u32 = 0x01;
    pub(super) const SHIFT_STATE: u32 = 0x02;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        static kTISPropertyUnicodeKeyLayoutData: *const c_void;
        fn TISCopyCurrentKeyboardLayoutInputSource() -> *mut c_void;
        fn TISGetInputSourceProperty(source: *mut c_void, key: *const c_void) -> *const c_void;
        fn LMGetKbdType() -> u8;
        fn UCKeyTranslate(
            layout: *const c_void,
            virtual_key_code: u16,
            key_action: u16,
            modifier_key_state: u32,
            keyboard_type: u32,
            options: u32,
            dead_key_state: *mut u32,
            max_length: usize,
            actual_length: *mut usize,
            unicode: *mut u16,
        ) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
        fn CFRelease(object: *const c_void);
    }

    /// The key code whose key types `character` on the current layout while the modifiers in
    /// `modifier_state` are held.
    pub(super) fn key_code_typing(character: char, modifier_state: u32) -> Option<CGKeyCode> {
        // SAFETY: the copy follows the Create rule and is released below. The property follows
        // the Get rule, so it lives as long as the source.
        let source = unsafe { TISCopyCurrentKeyboardLayoutInputSource() };
        if source.is_null() {
            return None;
        }
        // SAFETY: `source` is a live input source and the key is Carbon's constant.
        let data = unsafe { TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData) };
        let found = if data.is_null() {
            None
        } else {
            // SAFETY: the property is a CFData that holds a `UCKeyboardLayout`.
            let layout = unsafe { CFDataGetBytePtr(data) }.cast::<c_void>();
            // SAFETY: no arguments; it reads the keyboard type of the last event.
            let keyboard_type = u32::from(unsafe { LMGetKbdType() });
            TYPING_KEY_CODES.into_iter().find(|&code| {
                typed_char(layout, code, modifier_state, keyboard_type)
                    .is_some_and(|typed| typed.eq_ignore_ascii_case(&character))
            })
        };
        // SAFETY: `source` came from a Copy call and is released once.
        unsafe { CFRelease(source.cast_const()) };
        found
    }

    fn typed_char(
        layout: *const c_void,
        code: u16,
        modifier_state: u32,
        keyboard_type: u32,
    ) -> Option<char> {
        let mut dead_key_state = 0_u32;
        let mut length = 0_usize;
        let mut unicode = [0_u16; 4];
        // SAFETY: `layout` points into the live CFData; the buffers outlive the call and the
        // length passed matches `unicode`.
        let status = unsafe {
            UCKeyTranslate(
                layout,
                code,
                KEY_ACTION_DISPLAY,
                modifier_state,
                keyboard_type,
                NO_DEAD_KEYS,
                &raw mut dead_key_state,
                unicode.len(),
                &raw mut length,
                unicode.as_mut_ptr(),
            )
        };
        if status != 0 || length != 1 {
            return None;
        }
        char::from_u32(u32::from(unicode[0]))
    }
}
