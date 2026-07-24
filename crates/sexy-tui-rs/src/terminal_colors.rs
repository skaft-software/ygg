/// RGB color with 0-255 channel values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

fn hex_to_rgb(hex: &str) -> RgbColor {
    let normalized = hex.strip_prefix('#').unwrap_or(hex);
    let r = u8::from_str_radix(&normalized[0..2], 16).unwrap_or(0);
    let g = u8::from_str_radix(&normalized[2..4], 16).unwrap_or(0);
    let b = u8::from_str_radix(&normalized[4..6], 16).unwrap_or(0);
    RgbColor { r, g, b }
}

fn parse_osc_hex_channel(channel: &str) -> Option<u8> {
    if channel.is_empty() || !channel.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bits = channel.len().checked_mul(4)?;
    let max = 1u128
        .checked_shl(u32::try_from(bits).ok()?)?
        .checked_sub(1)?;
    let value = u128::from_str_radix(channel, 16).ok()?;
    Some(((value as f64 / max as f64) * 255.0).round() as u8)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColorScheme {
    Dark,
    Light,
}

/// Check if data matches the OSC 11 background color response pattern.
pub fn is_osc11_background_color_response(data: &str) -> bool {
    let Some(rest) = data.strip_prefix("\x1b]11;") else {
        return false;
    };
    let body = rest
        .strip_suffix('\x07')
        .or_else(|| rest.strip_suffix("\x1b\\"));
    body.is_some_and(|value| !value.contains(['\x07', '\x1b']))
}

/// Parse an OSC 11 background color response into an RgbColor.
pub fn parse_osc11_background_color(data: &str) -> Option<RgbColor> {
    if !is_osc11_background_color_response(data) {
        return None;
    }
    // Extract the value between "\x1b]11;" and the terminator
    let value_part = &data[5..]; // skip "\x1b]11;"
    let value = value_part
        .strip_suffix("\x1b\\")
        .or_else(|| value_part.strip_suffix('\x07'))?;
    let value = value.trim();

    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hex_to_rgb(value));
        }
        if hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let r = parse_osc_hex_channel(&hex[0..4])?;
            let g = parse_osc_hex_channel(&hex[4..8])?;
            let b = parse_osc_hex_channel(&hex[8..12])?;
            return Some(RgbColor { r, g, b });
        }
        return None;
    }

    let lower = value.to_ascii_lowercase();
    let rgb_value = if lower.starts_with("rgba:") {
        &value[5..]
    } else if lower.starts_with("rgb:") {
        &value[4..]
    } else {
        value
    };
    let mut parts = rgb_value.split('/');
    let red = parts.next()?;
    let green = parts.next()?;
    let blue = parts.next()?;
    let r = parse_osc_hex_channel(red)?;
    let g = parse_osc_hex_channel(green)?;
    let b = parse_osc_hex_channel(blue)?;
    Some(RgbColor { r, g, b })
}

/// Parse Pi's DECRQSS-style terminal color-scheme report.
pub fn parse_terminal_color_scheme_report(data: &str) -> Option<TerminalColorScheme> {
    match data {
        "\x1b[?997;1n" => Some(TerminalColorScheme::Dark),
        "\x1b[?997;2n" => Some(TerminalColorScheme::Light),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_parses_16_bit_osc11_rgb_responses() {
        assert_eq!(
            parse_osc11_background_color("\x1b]11;rgb:0000/8000/ffff\x07"),
            Some(RgbColor {
                r: 0,
                g: 128,
                b: 255
            })
        );
    }

    #[test]
    fn pi_parses_osc11_hex_responses() {
        assert_eq!(
            parse_osc11_background_color("\x1b]11;#ffffff\x1b\\"),
            Some(RgbColor {
                r: 255,
                g: 255,
                b: 255
            })
        );
        assert_eq!(
            parse_osc11_background_color("\x1b]11;#000000\x07"),
            Some(RgbColor { r: 0, g: 0, b: 0 })
        );
    }

    #[test]
    fn pi_rejects_non_strict_osc11_responses() {
        assert_eq!(parse_osc11_background_color("x\x1b]11;#ffffff\x07"), None);
        assert_eq!(parse_osc11_background_color("\x1b]10;#ffffff\x07"), None);
        assert_eq!(parse_osc11_background_color("\x1b]11;#ffffff\x07x"), None);
    }

    #[test]
    fn pi_parses_terminal_color_scheme_reports() {
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;1n"),
            Some(TerminalColorScheme::Dark)
        );
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;2n"),
            Some(TerminalColorScheme::Light)
        );
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;3n"), None);
        assert_eq!(parse_terminal_color_scheme_report("x\x1b[?997;1n"), None);
    }
}
