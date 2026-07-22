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
    if !channel.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let max = 16u32.pow(channel.len() as u32) - 1;
    if max == 0 {
        return None;
    }
    let value = u32::from_str_radix(channel, 16).ok()?;
    Some(((value as f64 / max as f64) * 255.0).round() as u8)
}

/// Check if data matches the OSC 11 background color response pattern.
pub fn is_osc11_background_color_response(data: &str) -> bool {
    // Pattern: \x1b]11;<value>\x07 or \x1b]11;<value>\x1b\\
    if !data.starts_with("\x1b]11;") {
        return false;
    }
    let rest = &data[5..]; // skip "\x1b]11;"
    rest.ends_with('\x07') || rest.ends_with("\x1b\\")
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

    let rgb_value = value
        .strip_prefix("rgb:")
        .or_else(|| value.strip_prefix("rgba:"))
        .unwrap_or(value);
    let mut parts = rgb_value.split('/');
    let red = parts.next()?;
    let green = parts.next()?;
    let blue = parts.next()?;
    let r = parse_osc_hex_channel(red)?;
    let g = parse_osc_hex_channel(green)?;
    let b = parse_osc_hex_channel(blue)?;
    Some(RgbColor { r, g, b })
}
