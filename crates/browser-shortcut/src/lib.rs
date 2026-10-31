//! Presses a Chrome extension's keyboard shortcut into one Chrome process as a real key event.
//! Chrome counts a `commands` shortcut as an invocation of the extension on the active tab, and
//! tab capture needs one. The press goes to the process by pid, so a switch to another app
//! between the check and the press cannot type the key into that app.
//!
//! macOS only. Elsewhere `readiness` reports `Unsupported` and `press` refuses.

#[cfg(target_os = "macos")]
mod mac;

/// A shortcut as Chrome reports it, ready to press.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "only the macOS sender reads the key and modifiers"
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    key: Key,
    modifiers: Modifiers,
}

/// The held modifiers, one bit per `Modifier`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Modifiers(u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modifier {
    Control,
    Option,
    Shift,
    Command,
}

impl Modifier {
    #[cfg_attr(
        not(target_os = "macos"),
        allow(dead_code, reason = "only the macOS sender lists the modifiers")
    )]
    const ALL: [Self; 4] = [Self::Control, Self::Option, Self::Shift, Self::Command];

    fn bit(self) -> u8 {
        match self {
            Self::Control => 1,
            Self::Option => 2,
            Self::Shift => 4,
            Self::Command => 8,
        }
    }
}

impl Modifiers {
    fn hold(&mut self, modifier: Modifier) {
        self.0 |= modifier.bit();
    }

    fn is_held(self, modifier: Modifier) -> bool {
        self.0 & modifier.bit() != 0
    }
}

/// The keys Chrome allows in a `commands` shortcut. Letters, digits and punctuation are the
/// characters they type, so the sender finds them on the current layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "only the macOS sender reads the key and modifiers"
    )
)]
enum Key {
    Letter(u8),
    Digit(u8),
    Comma,
    Period,
    Space,
    Home,
    End,
    PageUp,
    PageDown,
    ForwardDelete,
    Up,
    Down,
    Left,
    Right,
}

/// Why a shortcut cannot be pressed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShortcutError {
    /// Chrome has no key bound to the command: the user cleared it, or another extension took it.
    #[error("no shortcut is bound")]
    Unbound,
    #[error("the key {key:?} has no key code")]
    UnknownKey { key: String },
    #[error("the modifier {modifier:?} is not known")]
    UnknownModifier { modifier: String },
    /// Chrome needs Ctrl, Alt or Command in a shortcut, so a bare key is not one it took.
    #[error("the shortcut {shortcut:?} has no modifier")]
    NoModifier { shortcut: String },
}

impl Shortcut {
    /// Reads the `shortcut` field of `chrome.commands.getAll()`. On the Mac Chrome writes symbols
    /// with no separator, such as `⌥⇧V`; elsewhere it writes `Alt+Shift+V`.
    ///
    /// # Errors
    ///
    /// Returns why the text names no key this crate can press.
    pub fn from_chrome(text: &str) -> Result<Self, ShortcutError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ShortcutError::Unbound);
        }
        let (modifiers, key_text) = if text.contains('+') && text.chars().count() > 1 {
            parse_words(text)?
        } else {
            parse_symbols(text)
        };
        if modifiers == Modifiers::default() {
            return Err(ShortcutError::NoModifier {
                shortcut: text.to_owned(),
            });
        }
        let key = Key::from_name(key_text).ok_or_else(|| ShortcutError::UnknownKey {
            key: key_text.to_owned(),
        })?;
        Ok(Self { key, modifiers })
    }

    /// Holds Command or Control. On the Mac such a press inserts no text, so a press that lands
    /// in a window where the command is not bound, such as another Chrome profile, types nothing.
    /// Option and Shift alone type characters: Option+Shift+V types "◊".
    #[must_use]
    pub fn is_text_free(self) -> bool {
        self.modifiers.is_held(Modifier::Command) || self.modifiers.is_held(Modifier::Control)
    }
}

fn parse_words(text: &str) -> Result<(Modifiers, &str), ShortcutError> {
    let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
    // "Ctrl++" would name the plus key, which Chrome does not allow; it ends empty here.
    let key_text = parts.pop().unwrap_or_default();
    let mut modifiers = Modifiers::default();
    for part in parts {
        match part.to_ascii_lowercase().as_str() {
            // Chrome's Mac wording: "Ctrl" is Command there, and "MacCtrl" is Control.
            "ctrl" if cfg!(target_os = "macos") => modifiers.hold(Modifier::Command),
            "ctrl" | "macctrl" | "control" => modifiers.hold(Modifier::Control),
            "alt" | "option" => modifiers.hold(Modifier::Option),
            "shift" => modifiers.hold(Modifier::Shift),
            "command" | "cmd" => modifiers.hold(Modifier::Command),
            _ => {
                return Err(ShortcutError::UnknownModifier {
                    modifier: part.to_owned(),
                });
            }
        }
    }
    Ok((modifiers, key_text))
}

fn parse_symbols(text: &str) -> (Modifiers, &str) {
    let mut modifiers = Modifiers::default();
    let mut rest = text;
    while let Some(symbol) = rest.chars().next() {
        match symbol {
            '⌃' => modifiers.hold(Modifier::Control),
            '⌥' => modifiers.hold(Modifier::Option),
            '⇧' => modifiers.hold(Modifier::Shift),
            '⌘' => modifiers.hold(Modifier::Command),
            _ => break,
        }
        rest = &rest[symbol.len_utf8()..];
    }
    (modifiers, rest)
}

impl Key {
    /// Chrome's names, as words or as the Mac symbols it shows.
    fn from_name(name: &str) -> Option<Self> {
        if let [byte] = name.as_bytes() {
            if byte.is_ascii_alphabetic() {
                return Some(Self::Letter(byte.to_ascii_uppercase()));
            }
            if byte.is_ascii_digit() {
                return Some(Self::Digit(*byte));
            }
        }
        let key = match name.to_ascii_lowercase().as_str() {
            "," | "comma" => Self::Comma,
            "." | "period" => Self::Period,
            "space" | "␣" => Self::Space,
            "home" | "↖" => Self::Home,
            "end" | "↘" => Self::End,
            "pageup" | "⇞" => Self::PageUp,
            "pagedown" | "⇟" => Self::PageDown,
            "delete" | "⌦" => Self::ForwardDelete,
            "up" | "↑" => Self::Up,
            "down" | "↓" => Self::Down,
            "left" | "←" => Self::Left,
            "right" | "→" => Self::Right,
            _ => return None,
        };
        Some(key)
    }
}

/// Whether this process can press a shortcut now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    /// macOS sends key events from trusted Accessibility clients only. The user grants it in
    /// System Settings; this crate never asks.
    AccessibilityMissing,
    /// No key sender exists for this OS yet.
    Unsupported,
}

/// Reads the Accessibility grant on each call: the user can change it at any time.
#[must_use]
pub fn readiness() -> Readiness {
    #[cfg(target_os = "macos")]
    {
        if mac::is_accessibility_trusted() {
            Readiness::Ready
        } else {
            Readiness::AccessibilityMissing
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Readiness::Unsupported
    }
}

/// The pid of the app in front, which takes typed keys.
#[must_use]
pub fn frontmost_pid() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        mac::frontmost_pid()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// The bundle id of the app with this pid, or `None` when no app with a bundle has it.
#[must_use]
pub fn bundle_id_of(pid: u32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        mac::bundle_id_of(pid)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        None
    }
}

/// Why a press did not go out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PressError {
    #[error("no key sender exists for this OS")]
    Unsupported,
    #[error("the pid {pid} is out of range")]
    InvalidPid { pid: u32 },
    #[error("macOS did not create the key event")]
    EventNotCreated,
    /// No key on the current keyboard layout types the shortcut's character, so no press can
    /// match it.
    #[error("no key on the keyboard layout types {key:?}")]
    KeyNotOnLayout { key: char },
}

/// Posts the shortcut's key down and key up to the process `pid`.
///
/// # Errors
///
/// Returns why no event went out. A posted event can still be dropped by the process.
pub fn press(pid: u32, shortcut: Shortcut) -> Result<(), PressError> {
    #[cfg(target_os = "macos")]
    {
        mac::press(pid, shortcut)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (pid, shortcut);
        Err(PressError::Unsupported)
    }
}
