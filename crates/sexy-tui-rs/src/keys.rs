//! Keyboard input handling compatible with Pi TUI v0.81.1.
//!
//! The terminal sends strings, not abstract key codes.  Keep matching and
//! parsing here protocol-aware so components see the same key identities under
//! legacy VT input, xterm `modifyOtherKeys`, and Kitty CSI-u.

use std::sync::atomic::{AtomicBool, Ordering};

static KITTY_PROTOCOL_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn set_kitty_protocol_active(active: bool) {
    KITTY_PROTOCOL_ACTIVE.store(active, Ordering::Release);
}

pub fn is_kitty_protocol_active() -> bool {
    KITTY_PROTOCOL_ACTIVE.load(Ordering::Acquire)
}

pub type KeyId = &'static str;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventType {
    Press,
    Repeat,
    Release,
}

pub struct Key;

#[allow(non_upper_case_globals)]
impl Key {
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

    pub const backtick: KeyId = "`";
    pub const hyphen: KeyId = "-";
    pub const equals: KeyId = "=";
    pub const left_bracket: KeyId = "[";
    pub const right_bracket: KeyId = "]";
    pub const backslash: KeyId = "\\";
    pub const semicolon: KeyId = ";";
    pub const quote: KeyId = "'";
    pub const comma: KeyId = ",";
    pub const period: KeyId = ".";
    pub const slash: KeyId = "/";

    pub fn ctrl(key: &str) -> String {
        format!("ctrl+{key}")
    }
    pub fn shift(key: &str) -> String {
        format!("shift+{key}")
    }
    pub fn alt(key: &str) -> String {
        format!("alt+{key}")
    }
    pub fn super_key(key: &str) -> String {
        format!("super+{key}")
    }
    pub fn ctrl_shift(key: &str) -> String {
        format!("ctrl+shift+{key}")
    }
    pub fn shift_ctrl(key: &str) -> String {
        format!("shift+ctrl+{key}")
    }
    pub fn ctrl_alt(key: &str) -> String {
        format!("ctrl+alt+{key}")
    }
    pub fn alt_ctrl(key: &str) -> String {
        format!("alt+ctrl+{key}")
    }
    pub fn shift_alt(key: &str) -> String {
        format!("shift+alt+{key}")
    }
    pub fn alt_shift(key: &str) -> String {
        format!("alt+shift+{key}")
    }
    pub fn ctrl_super(key: &str) -> String {
        format!("ctrl+super+{key}")
    }
    pub fn super_ctrl(key: &str) -> String {
        format!("super+ctrl+{key}")
    }
    pub fn shift_super(key: &str) -> String {
        format!("shift+super+{key}")
    }
    pub fn super_shift(key: &str) -> String {
        format!("super+shift+{key}")
    }
    pub fn alt_super(key: &str) -> String {
        format!("alt+super+{key}")
    }
    pub fn super_alt(key: &str) -> String {
        format!("super+alt+{key}")
    }
    pub fn ctrl_shift_alt(key: &str) -> String {
        format!("ctrl+shift+alt+{key}")
    }
    pub fn ctrl_shift_super(key: &str) -> String {
        format!("ctrl+shift+super+{key}")
    }
}

mod modifiers {
    pub const SHIFT: u16 = 1;
    pub const ALT: u16 = 2;
    pub const CTRL: u16 = 4;
    pub const SUPER: u16 = 8;
    pub const LOCK_MASK: u16 = 64 | 128;
}

const ESCAPE: i32 = 27;
const TAB: i32 = 9;
const ENTER: i32 = 13;
const SPACE: i32 = 32;
const BACKSPACE: i32 = 127;
const KP_ENTER: i32 = 57414;
const UP: i32 = -1;
const DOWN: i32 = -2;
const RIGHT: i32 = -3;
const LEFT: i32 = -4;
const DELETE: i32 = -10;
const INSERT: i32 = -11;
const PAGE_UP: i32 = -12;
const PAGE_DOWN: i32 = -13;
const HOME: i32 = -14;
const END: i32 = -15;

fn is_symbol(key: char) -> bool {
    matches!(
        key,
        '`' | '-'
            | '='
            | '['
            | ']'
            | '\\'
            | ';'
            | '\''
            | ','
            | '.'
            | '/'
            | '!'
            | '@'
            | '#'
            | '$'
            | '%'
            | '^'
            | '&'
            | '*'
            | '('
            | ')'
            | '_'
            | '+'
            | '|'
            | '~'
            | '{'
            | '}'
            | ':'
            | '<'
            | '>'
            | '?'
    )
}

fn normalize_functional(codepoint: i32) -> i32 {
    match codepoint {
        57399..=57408 => 48 + (codepoint - 57399),
        57409 => 46,
        57410 => 47,
        57411 => 42,
        57412 => 45,
        57413 => 43,
        57415 => 61,
        57416 => 44,
        57417 => LEFT,
        57418 => RIGHT,
        57419 => UP,
        57420 => DOWN,
        57421 => PAGE_UP,
        57422 => PAGE_DOWN,
        57423 => HOME,
        57424 => END,
        57425 => INSERT,
        57426 => DELETE,
        value => value,
    }
}

fn normalize_shifted_letter(codepoint: i32, modifier: u16) -> i32 {
    if modifier & !modifiers::LOCK_MASK & modifiers::SHIFT != 0 && (65..=90).contains(&codepoint) {
        codepoint + 32
    } else {
        codepoint
    }
}

#[derive(Debug, Clone, Copy)]
struct KittySequence {
    codepoint: i32,
    shifted: Option<i32>,
    base: Option<i32>,
    modifier: u16,
    _event_type: KeyEventType,
}

fn event_type(value: Option<&str>) -> KeyEventType {
    match value.and_then(|v| v.parse::<u8>().ok()) {
        Some(2) => KeyEventType::Repeat,
        Some(3) => KeyEventType::Release,
        _ => KeyEventType::Press,
    }
}

fn parse_u32(value: &str) -> Option<i32> {
    value.parse::<i32>().ok()
}

fn parse_kitty_sequence(data: &str) -> Option<KittySequence> {
    let body = data.strip_prefix("\x1b[")?;
    if let Some(body) = body.strip_suffix('u') {
        let (keys, modifier_event) = body
            .split_once(';')
            .map_or((body, None), |(a, b)| (a, Some(b)));
        let mut key_parts = keys.split(':');
        let codepoint = parse_u32(key_parts.next()?)?;
        let shifted = key_parts
            .next()
            .filter(|s| !s.is_empty())
            .and_then(parse_u32);
        let base = key_parts
            .next()
            .filter(|s| !s.is_empty())
            .and_then(parse_u32);
        if key_parts.next().is_some() {
            return None;
        }
        let (modifier, event) = modifier_event.map_or((1, None), |part| {
            let mut pieces = part.split(':');
            let modifier = pieces
                .next()
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(1);
            (modifier, pieces.next())
        });
        return Some(KittySequence {
            codepoint,
            shifted,
            base,
            modifier: modifier.saturating_sub(1),
            _event_type: event_type(event),
        });
    }

    let final_byte = body.chars().last()?;
    if matches!(final_byte, 'A' | 'B' | 'C' | 'D' | 'H' | 'F') {
        let params = &body[..body.len() - 1];
        let params = params.strip_prefix("1;")?;
        let mut pieces = params.split(':');
        let modifier = pieces.next()?.parse::<u16>().ok()?.saturating_sub(1);
        let event = event_type(pieces.next());
        if pieces.next().is_some() {
            return None;
        }
        let codepoint = match final_byte {
            'A' => UP,
            'B' => DOWN,
            'C' => RIGHT,
            'D' => LEFT,
            'H' => HOME,
            'F' => END,
            _ => unreachable!(),
        };
        return Some(KittySequence {
            codepoint,
            shifted: None,
            base: None,
            modifier,
            _event_type: event,
        });
    }

    if let Some(params) = body.strip_suffix('~') {
        let mut semi = params.split(';');
        let number = semi.next()?.parse::<u8>().ok()?;
        let modifier_event = semi.next();
        if semi.next().is_some() {
            return None;
        }
        let mut parts = modifier_event.unwrap_or("1").split(':');
        let modifier = parts.next()?.parse::<u16>().ok()?.saturating_sub(1);
        let event = event_type(parts.next());
        let codepoint = match number {
            2 => INSERT,
            3 => DELETE,
            5 => PAGE_UP,
            6 => PAGE_DOWN,
            7 => HOME,
            8 => END,
            _ => return None,
        };
        return Some(KittySequence {
            codepoint,
            shifted: None,
            base: None,
            modifier,
            _event_type: event,
        });
    }
    None
}

fn parse_modify_other_keys(data: &str) -> Option<(i32, u16)> {
    let body = data.strip_prefix("\x1b[27;")?.strip_suffix('~')?;
    let (modifier, codepoint) = body.split_once(';')?;
    Some((
        codepoint.parse().ok()?,
        modifier.parse::<u16>().ok()?.saturating_sub(1),
    ))
}

fn matches_kitty(data: &str, expected: i32, expected_modifier: u16) -> bool {
    let Some(parsed) = parse_kitty_sequence(data) else {
        return false;
    };
    if parsed.modifier & !modifiers::LOCK_MASK != expected_modifier & !modifiers::LOCK_MASK {
        return false;
    }
    let actual = normalize_shifted_letter(normalize_functional(parsed.codepoint), parsed.modifier);
    let expected = normalize_shifted_letter(normalize_functional(expected), expected_modifier);
    if actual == expected {
        return true;
    }
    if parsed.base == Some(expected) {
        let latin = (97..=122).contains(&actual);
        let known_symbol = char::from_u32(actual as u32).is_some_and(is_symbol);
        return !latin && !known_symbol;
    }
    false
}

fn matches_modify(data: &str, expected: i32, modifier: u16) -> bool {
    parse_modify_other_keys(data) == Some((expected, modifier))
}

fn matches_printable_modify(data: &str, expected: i32, modifier: u16) -> bool {
    if modifier == 0 {
        return false;
    }
    parse_modify_other_keys(data).is_some_and(|(codepoint, actual)| {
        actual == modifier
            && normalize_shifted_letter(codepoint, actual)
                == normalize_shifted_letter(expected, modifier)
    })
}

fn is_windows_terminal_session() -> bool {
    std::env::var_os("WT_SESSION").is_some()
        && ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
            .iter()
            .all(|name| std::env::var_os(name).is_none())
}

fn matches_raw_backspace(data: &str, modifier: u16) -> bool {
    if data == "\x7f" {
        return modifier == 0;
    }
    data == "\x08"
        && if is_windows_terminal_session() {
            modifier == modifiers::CTRL
        } else {
            modifier == 0
        }
}

fn raw_ctrl_char(key: char) -> Option<char> {
    let lower = key.to_ascii_lowercase();
    if lower.is_ascii_lowercase() || matches!(lower, '[' | '\\' | ']' | '_') {
        Some(((lower as u8) & 0x1f) as char)
    } else if lower == '-' {
        Some('\x1f')
    } else {
        None
    }
}

#[derive(Default)]
struct ParsedKey<'a> {
    key: &'a str,
    ctrl: bool,
    shift: bool,
    alt: bool,
    super_mod: bool,
}

fn parse_key_id(key_id: &str) -> Option<ParsedKey<'_>> {
    let mut parsed = ParsedKey::default();
    for part in key_id.split('+') {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" => parsed.ctrl = true,
            "shift" => parsed.shift = true,
            "alt" => parsed.alt = true,
            "super" => parsed.super_mod = true,
            _ => parsed.key = part,
        }
    }
    (!parsed.key.is_empty()).then_some(parsed)
}

fn modifier(parsed: &ParsedKey<'_>) -> u16 {
    (if parsed.shift { modifiers::SHIFT } else { 0 })
        | (if parsed.alt { modifiers::ALT } else { 0 })
        | (if parsed.ctrl { modifiers::CTRL } else { 0 })
        | (if parsed.super_mod {
            modifiers::SUPER
        } else {
            0
        })
}

fn legacy_sequences(key: &str) -> &'static [&'static str] {
    match key {
        "up" => &["\x1b[A", "\x1bOA"],
        "down" => &["\x1b[B", "\x1bOB"],
        "right" => &["\x1b[C", "\x1bOC"],
        "left" => &["\x1b[D", "\x1bOD"],
        "home" => &["\x1b[H", "\x1bOH", "\x1b[1~", "\x1b[7~"],
        "end" => &["\x1b[F", "\x1bOF", "\x1b[4~", "\x1b[8~"],
        "insert" => &["\x1b[2~"],
        "delete" => &["\x1b[3~"],
        "pageup" => &["\x1b[5~", "\x1b[[5~"],
        "pagedown" => &["\x1b[6~", "\x1b[[6~"],
        "clear" => &["\x1b[E", "\x1bOE"],
        "f1" => &["\x1bOP", "\x1b[11~", "\x1b[[A"],
        "f2" => &["\x1bOQ", "\x1b[12~", "\x1b[[B"],
        "f3" => &["\x1bOR", "\x1b[13~", "\x1b[[C"],
        "f4" => &["\x1bOS", "\x1b[14~", "\x1b[[D"],
        "f5" => &["\x1b[15~", "\x1b[[E"],
        "f6" => &["\x1b[17~"],
        "f7" => &["\x1b[18~"],
        "f8" => &["\x1b[19~"],
        "f9" => &["\x1b[20~"],
        "f10" => &["\x1b[21~"],
        "f11" => &["\x1b[23~"],
        "f12" => &["\x1b[24~"],
        _ => &[],
    }
}

fn legacy_modified(data: &str, key: &str, modifier: u16) -> bool {
    let sequence = match (key, modifier) {
        ("up", modifiers::SHIFT) => "\x1b[a",
        ("down", modifiers::SHIFT) => "\x1b[b",
        ("right", modifiers::SHIFT) => "\x1b[c",
        ("left", modifiers::SHIFT) => "\x1b[d",
        ("clear", modifiers::SHIFT) => "\x1b[e",
        ("insert", modifiers::SHIFT) => "\x1b[2$",
        ("delete", modifiers::SHIFT) => "\x1b[3$",
        ("pageup", modifiers::SHIFT) => "\x1b[5$",
        ("pagedown", modifiers::SHIFT) => "\x1b[6$",
        ("home", modifiers::SHIFT) => "\x1b[7$",
        ("end", modifiers::SHIFT) => "\x1b[8$",
        ("up", modifiers::CTRL) => "\x1bOa",
        ("down", modifiers::CTRL) => "\x1bOb",
        ("right", modifiers::CTRL) => "\x1bOc",
        ("left", modifiers::CTRL) => "\x1bOd",
        ("clear", modifiers::CTRL) => "\x1bOe",
        ("insert", modifiers::CTRL) => "\x1b[2^",
        ("delete", modifiers::CTRL) => "\x1b[3^",
        ("pageup", modifiers::CTRL) => "\x1b[5^",
        ("pagedown", modifiers::CTRL) => "\x1b[6^",
        ("home", modifiers::CTRL) => "\x1b[7^",
        ("end", modifiers::CTRL) => "\x1b[8^",
        _ => return false,
    };
    data == sequence
}

pub fn is_key_release(data: &str) -> bool {
    !data.contains("\x1b[200~")
        && [":3u", ":3~", ":3A", ":3B", ":3C", ":3D", ":3H", ":3F"]
            .iter()
            .any(|needle| data.contains(needle))
}

pub fn is_key_repeat(data: &str) -> bool {
    !data.contains("\x1b[200~")
        && [":2u", ":2~", ":2A", ":2B", ":2C", ":2D", ":2H", ":2F"]
            .iter()
            .any(|needle| data.contains(needle))
}

pub fn matches_key(data: &str, key_id: &str) -> bool {
    let Some(parsed) = parse_key_id(key_id) else {
        return false;
    };
    let key_lower = parsed.key.to_ascii_lowercase();
    let key = key_lower.as_str();
    let modifier = modifier(&parsed);
    let kitty = is_kitty_protocol_active();
    match key {
        "escape" | "esc" => {
            modifier == 0
                && (data == "\x1b"
                    || matches_kitty(data, ESCAPE, 0)
                    || matches_modify(data, ESCAPE, 0))
        }
        "space" => {
            if !kitty
                && ((modifier == modifiers::CTRL && data == "\0")
                    || (modifier == modifiers::ALT && data == "\x1b "))
            {
                return true;
            }
            (modifier == 0 && data == " ")
                || matches_kitty(data, SPACE, modifier)
                || matches_modify(data, SPACE, modifier)
        }
        "tab" => {
            (modifier == modifiers::SHIFT && data == "\x1b[Z")
                || (modifier == 0 && data == "\t")
                || matches_kitty(data, TAB, modifier)
                || (modifier != 0 && matches_modify(data, TAB, modifier))
        }
        "enter" | "return" => {
            if modifier == modifiers::SHIFT && kitty && matches!(data, "\x1b\r" | "\n") {
                return true;
            }
            if modifier == modifiers::ALT && !kitty && data == "\x1b\r" {
                return true;
            }
            if modifier == 0 && (data == "\r" || (!kitty && data == "\n") || data == "\x1bOM") {
                return true;
            }
            matches_kitty(data, ENTER, modifier)
                || matches_kitty(data, KP_ENTER, modifier)
                || (modifier != 0 && matches_modify(data, ENTER, modifier))
        }
        "backspace" => {
            if modifier == modifiers::ALT && matches!(data, "\x1b\x7f" | "\x1b\x08") {
                return true;
            }
            matches_raw_backspace(data, modifier)
                || matches_kitty(data, BACKSPACE, modifier)
                || matches_modify(data, BACKSPACE, modifier)
        }
        "insert" | "delete" | "home" | "end" | "pageup" | "pagedown" => {
            let code = match key {
                "insert" => INSERT,
                "delete" => DELETE,
                "home" => HOME,
                "end" => END,
                "pageup" => PAGE_UP,
                _ => PAGE_DOWN,
            };
            (modifier == 0 && legacy_sequences(key).contains(&data))
                || legacy_modified(data, key, modifier)
                || matches_kitty(data, code, modifier)
        }
        "clear" => {
            (modifier == 0 && legacy_sequences(key).contains(&data))
                || legacy_modified(data, key, modifier)
        }
        "up" | "down" | "left" | "right" => {
            let code = match key {
                "up" => UP,
                "down" => DOWN,
                "left" => LEFT,
                _ => RIGHT,
            };
            if modifier == modifiers::ALT {
                let legacy = match key {
                    "up" => data == "\x1bp",
                    "down" => data == "\x1bn",
                    "left" => data == "\x1b[1;3D" || data == "\x1bb" || (!kitty && data == "\x1bB"),
                    _ => data == "\x1b[1;3C" || data == "\x1bf" || (!kitty && data == "\x1bF"),
                };
                return legacy || matches_kitty(data, code, modifier);
            }
            if modifier == modifiers::CTRL && matches!(key, "left" | "right") {
                let explicit = if key == "left" {
                    "\x1b[1;5D"
                } else {
                    "\x1b[1;5C"
                };
                if data == explicit {
                    return true;
                }
            }
            (modifier == 0 && legacy_sequences(key).contains(&data))
                || legacy_modified(data, key, modifier)
                || matches_kitty(data, code, modifier)
        }
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            modifier == 0 && legacy_sequences(key).contains(&data)
        }
        _ => {
            let mut chars = key.chars();
            let Some(character) = chars.next() else {
                return false;
            };
            if chars.next().is_some()
                || !(character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || is_symbol(character))
            {
                return false;
            }
            let codepoint = character as i32;
            let raw_ctrl = raw_ctrl_char(character);
            if modifier == (modifiers::CTRL | modifiers::ALT)
                && !kitty
                && raw_ctrl.is_some_and(|control| data == format!("\x1b{control}"))
            {
                return true;
            }
            if modifier == modifiers::ALT && !kitty && data == format!("\x1b{character}") {
                return true;
            }
            if modifier == modifiers::CTRL
                && raw_ctrl.is_some_and(|control| data == control.to_string())
            {
                return true;
            }
            if modifier == modifiers::SHIFT
                && character.is_ascii_lowercase()
                && data == character.to_ascii_uppercase().to_string()
            {
                return true;
            }
            if modifier == 0 && data == character.to_string() {
                return true;
            }
            matches_kitty(data, codepoint, modifier)
                || matches_printable_modify(data, codepoint, modifier)
        }
    }
}

fn format_with_modifiers(key: &str, modifier: u16) -> Option<String> {
    let effective = modifier & !modifiers::LOCK_MASK;
    if effective & !(modifiers::SHIFT | modifiers::CTRL | modifiers::ALT | modifiers::SUPER) != 0 {
        return None;
    }
    let mut names = Vec::new();
    if effective & modifiers::SHIFT != 0 {
        names.push("shift");
    }
    if effective & modifiers::CTRL != 0 {
        names.push("ctrl");
    }
    if effective & modifiers::ALT != 0 {
        names.push("alt");
    }
    if effective & modifiers::SUPER != 0 {
        names.push("super");
    }
    if names.is_empty() {
        Some(key.to_owned())
    } else {
        Some(format!("{}+{key}", names.join("+")))
    }
}

fn format_parsed(codepoint: i32, modifier: u16, base: Option<i32>) -> Option<String> {
    let normalized = normalize_shifted_letter(normalize_functional(codepoint), modifier);
    let recognized = (97..=122).contains(&normalized)
        || (48..=57).contains(&normalized)
        || char::from_u32(normalized as u32).is_some_and(is_symbol);
    let code = if recognized {
        normalized
    } else {
        base.unwrap_or(normalized)
    };
    let key = match code {
        ESCAPE => "escape".into(),
        TAB => "tab".into(),
        ENTER | KP_ENTER => "enter".into(),
        SPACE => "space".into(),
        BACKSPACE => "backspace".into(),
        DELETE => "delete".into(),
        INSERT => "insert".into(),
        HOME => "home".into(),
        END => "end".into(),
        PAGE_UP => "pageUp".into(),
        PAGE_DOWN => "pageDown".into(),
        UP => "up".into(),
        DOWN => "down".into(),
        LEFT => "left".into(),
        RIGHT => "right".into(),
        value
            if (48..=57).contains(&value)
                || (97..=122).contains(&value)
                || char::from_u32(value as u32).is_some_and(is_symbol) =>
        {
            char::from_u32(value as u32)?.to_string()
        }
        _ => return None,
    };
    format_with_modifiers(&key, modifier)
}

fn legacy_key_id(data: &str) -> Option<&'static str> {
    Some(match data {
        "\x1bOA" => "up",
        "\x1bOB" => "down",
        "\x1bOC" => "right",
        "\x1bOD" => "left",
        "\x1bOH" => "home",
        "\x1bOF" => "end",
        "\x1b[E" | "\x1bOE" => "clear",
        "\x1bOe" => "ctrl+clear",
        "\x1b[e" => "shift+clear",
        "\x1b[2~" => "insert",
        "\x1b[2$" => "shift+insert",
        "\x1b[2^" => "ctrl+insert",
        "\x1b[3$" => "shift+delete",
        "\x1b[3^" => "ctrl+delete",
        "\x1b[[5~" => "pageUp",
        "\x1b[[6~" => "pageDown",
        "\x1b[a" => "shift+up",
        "\x1b[b" => "shift+down",
        "\x1b[c" => "shift+right",
        "\x1b[d" => "shift+left",
        "\x1bOa" => "ctrl+up",
        "\x1bOb" => "ctrl+down",
        "\x1bOc" => "ctrl+right",
        "\x1bOd" => "ctrl+left",
        "\x1b[5$" => "shift+pageUp",
        "\x1b[6$" => "shift+pageDown",
        "\x1b[7$" => "shift+home",
        "\x1b[8$" => "shift+end",
        "\x1b[5^" => "ctrl+pageUp",
        "\x1b[6^" => "ctrl+pageDown",
        "\x1b[7^" => "ctrl+home",
        "\x1b[8^" => "ctrl+end",
        "\x1bOP" | "\x1b[11~" | "\x1b[[A" => "f1",
        "\x1bOQ" | "\x1b[12~" | "\x1b[[B" => "f2",
        "\x1bOR" | "\x1b[13~" | "\x1b[[C" => "f3",
        "\x1bOS" | "\x1b[14~" | "\x1b[[D" => "f4",
        "\x1b[15~" | "\x1b[[E" => "f5",
        "\x1b[17~" => "f6",
        "\x1b[18~" => "f7",
        "\x1b[19~" => "f8",
        "\x1b[20~" => "f9",
        "\x1b[21~" => "f10",
        "\x1b[23~" => "f11",
        "\x1b[24~" => "f12",
        "\x1bb" => "alt+left",
        "\x1bf" => "alt+right",
        "\x1bp" => "alt+up",
        "\x1bn" => "alt+down",
        _ => return None,
    })
}

pub fn parse_key(data: &str) -> Option<String> {
    if let Some(parsed) = parse_kitty_sequence(data) {
        return format_parsed(parsed.codepoint, parsed.modifier, parsed.base);
    }
    if let Some((codepoint, modifier)) = parse_modify_other_keys(data) {
        return format_parsed(codepoint, modifier, None);
    }
    let kitty = is_kitty_protocol_active();
    if kitty && matches!(data, "\x1b\r" | "\n") {
        return Some("shift+enter".into());
    }
    if let Some(key) = legacy_key_id(data) {
        return Some(key.into());
    }
    let fixed = match data {
        "\x1b" => Some("escape"),
        "\x1c" => Some("ctrl+\\"),
        "\x1d" => Some("ctrl+]"),
        "\x1f" => Some("ctrl+-"),
        "\x1b\x1b" => Some("ctrl+alt+["),
        "\x1b\x1c" => Some("ctrl+alt+\\"),
        "\x1b\x1d" => Some("ctrl+alt+]"),
        "\x1b\x1f" => Some("ctrl+alt+-"),
        "\t" => Some("tab"),
        "\r" | "\x1bOM" => Some("enter"),
        "\0" => Some("ctrl+space"),
        " " => Some("space"),
        "\x7f" => Some("backspace"),
        "\x1b[Z" => Some("shift+tab"),
        "\x1b\x7f" | "\x1b\x08" => Some("alt+backspace"),
        "\x1b[A" => Some("up"),
        "\x1b[B" => Some("down"),
        "\x1b[C" => Some("right"),
        "\x1b[D" => Some("left"),
        "\x1b[H" => Some("home"),
        "\x1b[F" => Some("end"),
        "\x1b[3~" => Some("delete"),
        "\x1b[5~" => Some("pageUp"),
        "\x1b[6~" => Some("pageDown"),
        _ => None,
    };
    if let Some(key) = fixed {
        return Some(key.into());
    }
    if data == "\n" && !kitty {
        return Some("enter".into());
    }
    if data == "\x08" {
        return Some(
            if is_windows_terminal_session() {
                "ctrl+backspace"
            } else {
                "backspace"
            }
            .into(),
        );
    }
    if !kitty && data == "\x1b\r" {
        return Some("alt+enter".into());
    }
    if !kitty && data == "\x1b " {
        return Some("alt+space".into());
    }
    if !kitty && data.len() == 2 && data.as_bytes()[0] == 0x1b {
        let code = data.as_bytes()[1];
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+alt+{}", (code + 96) as char));
        }
        let character = code as char;
        if character.is_ascii_lowercase() || character.is_ascii_digit() || is_symbol(character) {
            return Some(format!("alt+{character}"));
        }
    }
    if data.len() == 1 {
        let code = data.as_bytes()[0];
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+{}", (code + 96) as char));
        }
        if (32..=126).contains(&code) {
            return Some((code as char).to_string());
        }
    }
    None
}

pub fn decode_kitty_printable(data: &str) -> Option<char> {
    let parsed = parse_kitty_sequence(data)?;
    let allowed = modifiers::SHIFT | modifiers::LOCK_MASK;
    if parsed.modifier & !allowed != 0 || parsed.modifier & (modifiers::ALT | modifiers::CTRL) != 0
    {
        return None;
    }
    let mut codepoint = parsed.codepoint;
    if parsed.modifier & modifiers::SHIFT != 0 {
        codepoint = parsed.shifted.unwrap_or(codepoint);
    }
    let codepoint = normalize_functional(codepoint);
    (codepoint >= 32)
        .then(|| char::from_u32(codepoint as u32))
        .flatten()
}

pub fn decode_printable_key(data: &str) -> Option<char> {
    decode_kitty_printable(data).or_else(|| {
        let (codepoint, modifier) = parse_modify_other_keys(data)?;
        let modifier = modifier & !modifiers::LOCK_MASK;
        (modifier & !modifiers::SHIFT == 0 && codepoint >= 32)
            .then(|| char::from_u32(codepoint as u32))
            .flatten()
    })
}

/// Legacy Rust semantic-text adapter. Pi's core decoder intentionally rejects
/// Alt/Ctrl CSI-u; this adapter remains only for pre-port widgets, where AltGr
/// text was already part of the public behavior.
pub fn decode_kitty_text(data: &str) -> Option<char> {
    let parsed = parse_kitty_sequence(data)?;
    let control = parsed.modifier & modifiers::CTRL != 0;
    let alt = parsed.modifier & modifiers::ALT != 0;
    let system = parsed.modifier & modifiers::SUPER != 0;
    if system || (control && !alt) {
        return None;
    }
    let codepoint = if parsed.modifier & modifiers::SHIFT != 0 {
        parsed.shifted.unwrap_or(parsed.codepoint)
    } else {
        parsed.codepoint
    };
    let codepoint = normalize_functional(codepoint);
    (codepoint >= 32)
        .then(|| char::from_u32(codepoint as u32))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_kitty_alternate_layout_and_modifiers() {
        set_kitty_protocol_active(true);
        assert!(matches_key("\x1b[1089::99;5u", "ctrl+c"));
        assert!(!matches_key("\x1b[107::118;5u", "ctrl+v"));
        assert!(matches_key("\x1b[107::118;5u", "ctrl+k"));
        assert_eq!(
            parse_key("\x1b[107;14u").as_deref(),
            Some("shift+ctrl+super+k")
        );
        set_kitty_protocol_active(false);
    }

    #[test]
    fn pi_modify_other_keys_and_keypad() {
        assert!(matches_key("\x1b[27;6;69~", "ctrl+shift+e"));
        assert_eq!(parse_key("\x1b[27;6;69~").as_deref(), Some("shift+ctrl+e"));
        assert_eq!(decode_printable_key("\x1b[27;2;196~"), Some('Ä'));
        assert_eq!(parse_key("\x1b[57417u").as_deref(), Some("left"));
    }

    #[test]
    fn pi_legacy_sequences() {
        set_kitty_protocol_active(false);
        assert!(matches_key("\x1bOA", "up"));
        assert!(matches_key("\x1b[2^", "ctrl+insert"));
        assert!(matches_key("\x1b\x03", "ctrl+alt+c"));
        assert_eq!(parse_key("\x1b[[5~").as_deref(), Some("pageUp"));
    }

    #[test]
    fn event_types_do_not_scan_paste_payloads() {
        assert!(is_key_release("\x1b[97;1:3u"));
        assert!(is_key_repeat("\x1b[1;1:2A"));
        assert!(!is_key_release("\x1b[200~90:62:3F:A5\x1b[201~"));
    }
}
