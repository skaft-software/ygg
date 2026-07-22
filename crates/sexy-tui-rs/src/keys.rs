/// Keyboard input handling for terminal applications.
///
/// Supports both legacy terminal sequences and Kitty keyboard protocol.
/// Port of src/keys.ts (1400 lines).
use std::sync::atomic::{AtomicBool, Ordering};

// =============================================================================
// Global Kitty Protocol State
// =============================================================================

static KITTY_PROTOCOL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set the global Kitty keyboard protocol state.
pub fn set_kitty_protocol_active(active: bool) {
    KITTY_PROTOCOL_ACTIVE.store(active, Ordering::Release);
}

/// Query whether Kitty keyboard protocol is currently active.
pub fn is_kitty_protocol_active() -> bool {
    KITTY_PROTOCOL_ACTIVE.load(Ordering::Acquire)
}

// =============================================================================
// Key Identifiers
// =============================================================================

/// Key identifier string type.
pub type KeyId = &'static str;

/// Key event type from parseKey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyEventType<'a> {
    Key(&'a str),
    Char(char),
    Unknown(String),
}

/// Helper for creating typed key identifiers.
pub struct Key;

#[allow(non_upper_case_globals)]
impl Key {
    // Special keys
    pub const escape: KeyId = "escape";
    pub const esc: KeyId = "esc";
    pub const enter: KeyId = "enter";
    pub const return_: KeyId = "return";
    pub const tab: KeyId = "tab";
    pub const space: KeyId = "space";
    pub const backspace: KeyId = "backspace";
    pub const delete: KeyId = "delete";
    pub const insert: KeyId = "insert";
    pub const clear: KeyId = "clear";
    pub const home: KeyId = "home";
    pub const end: KeyId = "end";
    pub const page_up: KeyId = "pageUp";
    pub const page_down: KeyId = "pageDown";
    pub const up: KeyId = "up";
    pub const down: KeyId = "down";
    pub const left: KeyId = "left";
    pub const right: KeyId = "right";
    pub const f1: KeyId = "f1";
    pub const f2: KeyId = "f2";
    pub const f3: KeyId = "f3";
    pub const f4: KeyId = "f4";
    pub const f5: KeyId = "f5";
    pub const f6: KeyId = "f6";
    pub const f7: KeyId = "f7";
    pub const f8: KeyId = "f8";
    pub const f9: KeyId = "f9";
    pub const f10: KeyId = "f10";
    pub const f11: KeyId = "f11";
    pub const f12: KeyId = "f12";

    /// Create a ctrl+key identifier.
    pub fn ctrl(key: &str) -> String {
        format!("ctrl+{}", key)
    }

    /// Create a shift+key identifier.
    pub fn shift(key: &str) -> String {
        format!("shift+{}", key)
    }

    /// Create an alt+key identifier.
    pub fn alt(key: &str) -> String {
        format!("alt+{}", key)
    }

    /// Create a super+key identifier.
    pub fn super_key(key: &str) -> String {
        format!("super+{}", key)
    }

    /// Create a ctrl+shift+key identifier.
    pub fn ctrl_shift(key: &str) -> String {
        format!("ctrl+shift+{}", key)
    }

    /// Create a ctrl+alt+key identifier.
    pub fn ctrl_alt(key: &str) -> String {
        format!("ctrl+alt+{}", key)
    }

    /// Create a ctrl+super+key identifier.
    pub fn ctrl_super(key: &str) -> String {
        format!("ctrl+super+{}", key)
    }
}

// =============================================================================
// Modifier Constants
// =============================================================================

pub(crate) mod modifiers {
    pub const SHIFT: u8 = 1;
    pub const ALT: u8 = 2;
    pub const CTRL: u8 = 4;
    pub const SUPER: u8 = 8;
    #[allow(dead_code)]
    pub const CAPS_LOCK: u8 = 16;
    #[allow(dead_code)]
    pub const NUM_LOCK: u8 = 32;
    #[allow(dead_code)]
    pub const LOCK_MASK: u8 = CAPS_LOCK | NUM_LOCK;
}

// =============================================================================
// Key Codepoint Constants
// =============================================================================

pub(crate) mod codepoints {
    pub const ESCAPE: u32 = 27;
    pub const ENTER: u32 = 13;
    pub const TAB: u32 = 9;
    pub const BACKSPACE: u32 = 127;
    pub const DELETE: u32 = 0x7f; // same as backspace in many terminals
    pub const SPACE: u32 = 32;
    pub const HOME: u32 = 1;
    pub const END: u32 = 4;
    pub const INSERT: u32 = 2;
    pub const PAGE_UP: u32 = 5;
    pub const PAGE_DOWN: u32 = 6;
    pub const UP: u32 = 65;
    pub const DOWN: u32 = 66;
    pub const LEFT: u32 = 68;
    pub const RIGHT: u32 = 67;
    pub const F1: u32 = 11;
    pub const F2: u32 = 12;
    pub const F3: u32 = 13;
    pub const F4: u32 = 14;
    pub const F5: u32 = 15;
    pub const F6: u32 = 17;
    pub const F7: u32 = 18;
    pub const F8: u32 = 19;
    pub const F9: u32 = 20;
    pub const F10: u32 = 21;
    pub const F11: u32 = 23;
    pub const F12: u32 = 24;
}

// =============================================================================
// Kitty Keyboard Protocol Sequence Handling
// =============================================================================

/// Decode a Kitty protocol printable key, preferring its layout-resolved
/// alternate codepoint when one is present.
pub fn decode_kitty_printable(data: &str) -> Option<char> {
    // Kitty: ESC [ base:shifted ; modifier u. Plain CSI-u omits `:shifted`.
    let body = data.strip_prefix("\x1b[")?;
    let body = body.strip_suffix('u')?;
    let mut codepoints = body.split(';').next()?.split(':');
    let base = codepoints
        .next()?
        .parse::<u32>()
        .ok()
        .and_then(char::from_u32)?;
    codepoints
        .next()
        .and_then(|codepoint| codepoint.parse::<u32>().ok())
        .and_then(char::from_u32)
        .or(Some(base))
}

/// Decode a CSI-u sequence only when it represents insertable text.
///
/// This keeps legacy string-only widget routing safe: Ctrl and Super remain
/// controls, while Alt and Ctrl+Alt text (including AltGr) still insert.
pub fn decode_kitty_text(data: &str) -> Option<char> {
    let body = data.strip_prefix("\x1b[")?.strip_suffix('u')?;
    let modifier = body
        .split(';')
        .nth(1)
        .and_then(|field| field.split(':').next())
        .and_then(|field| field.parse::<u8>().ok())
        .unwrap_or(1)
        .saturating_sub(1);
    let ctrl = modifier & modifiers::CTRL != 0;
    let alt = modifier & modifiers::ALT != 0;
    let super_mod = modifier & modifiers::SUPER != 0;
    (!super_mod && (!ctrl || alt))
        .then(|| decode_kitty_printable(data))
        .flatten()
        .filter(|character| !character.is_control())
}

/// Kitty protocol: modifier bit 3 (value 8) indicates a key release event.
const KITTY_RELEASE_FLAG: u8 = 8;

/// Check if input data is a key release event.
pub fn is_key_release(data: &str) -> bool {
    // Release format: ESC [ evt:3 ; <modifier> u  (when using event-type subparameter)
    // or ESC [ <codepoint> ; <modifier> u where modifier has bit 3 set.
    if let Some((_, modifier)) = parse_kitty_sequence(data) {
        return modifier & KITTY_RELEASE_FLAG != 0;
    }
    // Legacy terminals: ESC [ evt:3 ~
    if data.starts_with("\x1b[") && data.contains(":3") {
        return true;
    }
    false
}

/// Check if input data is a key repeat event.
pub fn is_key_repeat(data: &str) -> bool {
    // In Kitty protocol, repeat events have event-type 2: ESC [ evt:2 ; ... u
    if data.starts_with("\x1b[") && data.contains(":2") {
        return true;
    }
    if let Some((_, modifier)) = parse_kitty_sequence(data) {
        // Kitty encodes repeat in the event-type subparameter, not the modifier.
        // The modifier byte alone can't distinguish repeat from press.
        // For now, return false for pure CSI u sequences.
        let _ = modifier;
    }
    false
}

/// Parse a Kitty keyboard sequence.
/// Format: ESC [ <codepoint / key> ; <modifier> u
fn parse_kitty_sequence(data: &str) -> Option<(u32, u8)> {
    let body = data.strip_prefix("\x1b[")?;
    let body = body.strip_suffix('u')?;
    let mut parts = body.split(';');
    let codepoint: u32 = parts.next()?.parse().ok()?;
    let modifier: u8 = parts.next().unwrap_or("1").parse().ok()?;
    Some((codepoint, modifier - 1)) // Kitty mods are 1-indexed
}

/// Match a Kitty keyboard sequence against an expected key and modifier.
fn matches_kitty_sequence(data: &str, expected_codepoint: u32, expected_modifier: u8) -> bool {
    if let Some((codepoint, modifier)) = parse_kitty_sequence(data) {
        codepoint == expected_codepoint && modifier == expected_modifier
    } else {
        false
    }
}

// =============================================================================
// Legacy Terminal Sequence Handling
// =============================================================================

/// Parse a modifyOtherKeys sequence: CSI <codepoint> ; <modifier> ~
fn parse_modify_other_keys_sequence(data: &str) -> Option<(u32, u8)> {
    let body = data.strip_prefix("\x1b[")?;
    let body = body.strip_suffix('~')?;
    let mut parts = body.split(';');
    let codepoint: u32 = parts.next()?.parse().ok()?;
    let modifier: u8 = parts.next().unwrap_or("1").parse().ok()?;
    Some((codepoint, modifier - 1))
}

fn matches_modify_other_keys(data: &str, expected_codepoint: u32, expected_modifier: u8) -> bool {
    if let Some((codepoint, modifier)) = parse_modify_other_keys_sequence(data) {
        codepoint == expected_codepoint && modifier == expected_modifier
    } else {
        false
    }
}

// =============================================================================
// Legacy Escape Sequence Matching
// =============================================================================

fn matches_legacy_escape(data: &str, expected_byte: char, expected_modifier: u8) -> bool {
    if expected_modifier == 0 {
        // No modifier — check for plain CSI sequence without modifier prefix
        return data == format!("\x1b[{}", expected_byte);
    }

    // Legacy: ESC [ 1 ; <mod+1> <byte>
    let pattern = format!("\x1b[1;{}{}", expected_modifier + 1, expected_byte);
    data == pattern
}

// =============================================================================
// Core: matchesKey
// =============================================================================

fn parse_key_id(key_id: &str) -> Option<ParsedKey> {
    let lower = key_id.to_lowercase();
    let parts: Vec<&str> = lower.split('+').collect();
    if parts.is_empty() {
        return None;
    }
    let key = *parts.last()?;
    Some(ParsedKey {
        key: key.to_string(),
        ctrl: parts[..parts.len() - 1].contains(&"ctrl"),
        shift: parts[..parts.len() - 1].contains(&"shift"),
        alt: parts[..parts.len() - 1].contains(&"alt"),
        super_mod: parts[..parts.len() - 1].contains(&"super"),
    })
}

#[derive(Debug)]
struct ParsedKey {
    key: String,
    ctrl: bool,
    shift: bool,
    alt: bool,
    super_mod: bool,
}

impl ParsedKey {
    fn modifier_byte(&self) -> u8 {
        use modifiers::*;
        let mut m = 0u8;
        if self.shift {
            m |= SHIFT;
        }
        if self.alt {
            m |= ALT;
        }
        if self.ctrl {
            m |= CTRL;
        }
        if self.super_mod {
            m |= SUPER;
        }
        m
    }
}

/// Match input data against a key identifier string.
pub fn matches_key(data: &str, key_id: &str) -> bool {
    let parsed = match parse_key_id(key_id) {
        Some(p) => p,
        None => return false,
    };
    let modifier = parsed.modifier_byte();

    match parsed.key.as_str() {
        "escape" | "esc" => {
            if modifier != 0 {
                return false;
            }
            data == "\x1b"
                || matches_kitty_sequence(data, codepoints::ESCAPE, 0)
                || matches_modify_other_keys(data, codepoints::ESCAPE, 0)
        }

        "enter" | "return" => {
            if modifier == 0 && data == "\r" {
                return true;
            }
            if modifier == modifiers::ALT && data == "\x1b\r" {
                return true;
            }
            matches_kitty_sequence(data, codepoints::ENTER, modifier)
                || matches_modify_other_keys(data, codepoints::ENTER, modifier)
        }

        "tab" => {
            if modifier == 0 && data == "\t" {
                return true;
            }
            if modifier == modifiers::SHIFT && data == "\x1b[Z" {
                return true;
            }
            matches_kitty_sequence(data, codepoints::TAB, modifier)
                || matches_modify_other_keys(data, codepoints::TAB, modifier)
        }

        "space" => {
            if modifier == 0 && data == " " {
                return true;
            }
            if !is_kitty_protocol_active() {
                if modifier == modifiers::CTRL && data == "\x00" {
                    return true;
                }
                if modifier == modifiers::ALT && data == "\x1b " {
                    return true;
                }
            }
            matches_kitty_sequence(data, codepoints::SPACE, modifier)
                || matches_modify_other_keys(data, codepoints::SPACE, modifier)
        }

        "backspace" => {
            if modifier == 0 {
                if data == "\x7f" {
                    return true;
                }
                if data == "\x08" {
                    return true;
                } // Windows Terminal
            }
            matches_kitty_sequence(data, codepoints::BACKSPACE, modifier)
                || matches_modify_other_keys(data, codepoints::BACKSPACE, modifier)
        }

        "delete" => {
            if modifier == 0 && data == "\x1b[3~" {
                return true;
            }
            matches_kitty_sequence(data, codepoints::DELETE, modifier)
        }

        "home" => {
            if modifier == 0 && data == "\x1b[H" {
                return true;
            }
            matches_kitty_sequence(data, codepoints::HOME, modifier)
                || matches_modify_other_keys(data, codepoints::HOME, modifier)
        }

        "end" => {
            if modifier == 0 && data == "\x1b[F" {
                return true;
            }
            matches_kitty_sequence(data, codepoints::END, modifier)
                || matches_modify_other_keys(data, codepoints::END, modifier)
        }

        "pageup" => {
            if modifier == 0 && data == "\x1b[5~" {
                return true;
            }
            matches_kitty_sequence(data, codepoints::PAGE_UP, modifier)
        }

        "pagedown" => {
            if modifier == 0 && data == "\x1b[6~" {
                return true;
            }
            matches_kitty_sequence(data, codepoints::PAGE_DOWN, modifier)
        }

        "up" => {
            if modifier == 0 && data == "\x1b[A" {
                return true;
            }
            matches_legacy_escape(data, 'A', modifier)
                || matches_kitty_sequence(data, codepoints::UP, modifier)
        }

        "down" => {
            if modifier == 0 && data == "\x1b[B" {
                return true;
            }
            matches_legacy_escape(data, 'B', modifier)
                || matches_kitty_sequence(data, codepoints::DOWN, modifier)
        }

        "left" => {
            if modifier == 0 && data == "\x1b[D" {
                return true;
            }
            matches_legacy_escape(data, 'D', modifier)
                || matches_kitty_sequence(data, codepoints::LEFT, modifier)
        }

        "right" => {
            if modifier == 0 && data == "\x1b[C" {
                return true;
            }
            matches_legacy_escape(data, 'C', modifier)
                || matches_kitty_sequence(data, codepoints::RIGHT, modifier)
        }

        "insert" => {
            if modifier == 0 && data == "\x1b[2~" {
                return true;
            }
            matches_kitty_sequence(data, codepoints::INSERT, modifier)
        }

        "clear" => {
            if modifier == 0 && data == "\x1b[3~" {
                return true;
            }
            false
        }

        // Function keys
        fk @ "f1"
        | fk @ "f2"
        | fk @ "f3"
        | fk @ "f4"
        | fk @ "f5"
        | fk @ "f6"
        | fk @ "f7"
        | fk @ "f8"
        | fk @ "f9"
        | fk @ "f10"
        | fk @ "f11"
        | fk @ "f12" => {
            let f_code = match fk {
                "f1" => codepoints::F1,
                "f2" => codepoints::F2,
                "f3" => codepoints::F3,
                "f4" => codepoints::F4,
                "f5" => codepoints::F5,
                "f6" => codepoints::F6,
                "f7" => codepoints::F7,
                "f8" => codepoints::F8,
                "f9" => codepoints::F9,
                "f10" => codepoints::F10,
                "f11" => codepoints::F11,
                "f12" => codepoints::F12,
                _ => return false,
            };
            matches_kitty_sequence(data, f_code, modifier)
        }

        // Printable keys: letters, digits, symbols
        key_str => {
            // For ctrl+letter combinations
            if modifier == modifiers::CTRL && key_str.len() == 1 {
                let ch = key_str.chars().next().unwrap();
                if ch.is_ascii_lowercase() {
                    let ctrl_char = ((ch as u8) & 0x1f) as char;
                    if data.len() == 1 && data.chars().next().unwrap() == ctrl_char {
                        return true;
                    }
                }
            }
            // Plain character match (no modifiers)
            if modifier == 0 && data == key_str {
                return true;
            }
            // Kitty protocol match
            if let Some(ch) = key_str.chars().next() {
                if key_str.len() == 1 {
                    return matches_kitty_sequence(data, ch as u32, modifier);
                }
            }
            false
        }
    }
}

/// Parse raw input data and return the key identifier.
pub fn parse_key(data: &str) -> KeyEventType<'_> {
    if data.is_empty() {
        return KeyEventType::Unknown(String::new());
    }

    // Single byte ASCII printable
    if data.len() == 1 {
        let b = data.as_bytes()[0];
        if (32..=126).contains(&b) {
            return KeyEventType::Char(b as char);
        }
        // Control characters
        match b {
            0x1b => return KeyEventType::Key(Key::escape),
            0x0d => return KeyEventType::Key(Key::enter),
            0x09 => return KeyEventType::Key(Key::tab),
            0x7f => return KeyEventType::Key(Key::backspace),
            0x08 => return KeyEventType::Key(Key::backspace),
            0x00 => return KeyEventType::Key("ctrl+ "),
            _ => {}
        }
    }

    // Check known escape sequences
    match data {
        "\x1b[A" => return KeyEventType::Key(Key::up),
        "\x1b[B" => return KeyEventType::Key(Key::down),
        "\x1b[C" => return KeyEventType::Key(Key::right),
        "\x1b[D" => return KeyEventType::Key(Key::left),
        "\x1b[H" => return KeyEventType::Key(Key::home),
        "\x1b[F" => return KeyEventType::Key(Key::end),
        "\x1b[2~" => return KeyEventType::Key(Key::insert),
        "\x1b[3~" => return KeyEventType::Key(Key::delete),
        "\x1b[5~" => return KeyEventType::Key(Key::page_up),
        "\x1b[6~" => return KeyEventType::Key(Key::page_down),
        "\x1b[Z" => return KeyEventType::Key("shift+tab"),
        "\x1b\r" => return KeyEventType::Key("alt+enter"),
        "\x1b " => return KeyEventType::Key("alt+ "),
        _ => {}
    }

    // Kitty protocol text sequences. Control chords remain key events.
    if let Some(ch) = decode_kitty_text(data) {
        return KeyEventType::Char(ch);
    }

    // CSI u (Kitty) sequences
    if let Some((codepoint, _modifier)) = parse_kitty_sequence(data) {
        if let Some(_ch) = char::from_u32(codepoint) {
            return KeyEventType::Key("unknown");
        }
    }

    KeyEventType::Unknown(data.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matches_escape() {
        assert!(matches_key("\x1b", Key::escape));
        assert!(!matches_key("x", Key::escape));
    }

    #[test]
    fn test_matches_enter() {
        assert!(matches_key("\r", Key::enter));
    }

    #[test]
    fn test_matches_tab() {
        assert!(matches_key("\t", Key::tab));
    }

    #[test]
    fn test_matches_ctrl_c() {
        let key = Key::ctrl("c");
        assert!(matches_key("\x03", &key));
    }

    #[test]
    fn test_matches_arrow_up() {
        assert!(matches_key("\x1b[A", Key::up));
    }

    #[test]
    fn test_matches_arrow_down() {
        assert!(matches_key("\x1b[B", Key::down));
    }

    #[test]
    fn test_matches_shift_tab() {
        let key = Key::shift("tab");
        assert!(matches_key("\x1b[Z", &key));
    }

    #[test]
    fn test_matches_backspace() {
        assert!(matches_key("\x7f", Key::backspace));
    }

    #[test]
    fn test_parse_key_char() {
        assert_eq!(parse_key("a"), KeyEventType::Char('a'));
        assert_eq!(parse_key("Z"), KeyEventType::Char('Z'));
    }

    #[test]
    fn test_parse_key_escape() {
        assert_eq!(parse_key("\x1b"), KeyEventType::Key("escape"));
    }

    #[test]
    fn csi_u_uses_alternate_text_and_keeps_controls_out_of_text() {
        assert_eq!(decode_kitty_printable("\x1b[97:65;2u"), Some('A'));
        assert_eq!(decode_kitty_printable("\x1b[49:33;2u"), Some('!'));
        assert_eq!(decode_kitty_text("\x1b[97:65;2u"), Some('A'));
        assert_eq!(decode_kitty_text("\x1b[8364;7u"), Some('€'));
        assert_eq!(decode_kitty_text("\x1b[99;5u"), None);
        assert_eq!(parse_key("\x1b[97:65;2u"), KeyEventType::Char('A'));
    }

    #[test]
    fn test_parse_key_arrows() {
        assert_eq!(parse_key("\x1b[A"), KeyEventType::Key("up"));
        assert_eq!(parse_key("\x1b[B"), KeyEventType::Key("down"));
    }
}
