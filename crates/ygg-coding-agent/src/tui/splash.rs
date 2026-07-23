//! Animated braille tree used by the inline startup identity.

use crate::tui::theme::YggTheme;

pub(crate) const DURATION: f32 = 2.2;

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

#[derive(Clone, Copy)]
struct Dot {
    x: f32,
    y: f32,
    size: f32,
}

#[derive(Clone, Copy, Default)]
struct Pixel {
    bits: u8,
    r: f32,
    g: f32,
    b: f32,
    weight: f32,
}

/// Render one transparent frame of the point-cloud tree. Only foreground
/// colours are emitted: blank cells retain the terminal's own background.
pub(crate) fn render_logo(
    theme: &YggTheme,
    width: usize,
    symbol_rows: usize,
    elapsed: f32,
    model_accent: Option<(u8, u8, u8)>,
) -> Vec<String> {
    if width == 0 || symbol_rows == 0 {
        return Vec::new();
    }
    let width = width.min(42);
    let symbol_rows = symbol_rows.min(21);
    let sub_width = width * 2;
    let sub_height = symbol_rows * 4;
    let mut cells = vec![Pixel::default(); width * symbol_rows];
    let t = elapsed.clamp(0.0, DURATION);

    let density_stride = if symbol_rows <= 6 { 2 } else { 1 };
    for (index, dot) in geometry().into_iter().enumerate() {
        // Resampling every canopy point into a compact mark makes it a blob.
        // Keep all branches, but thin the canopy at six rows and below.
        if symbol_rows <= 6 && index < 156 && index % density_stride != 0 {
            continue;
        }
        let vertical = ((dot.y + 0.95) / 1.9).clamp(0.0, 1.0);
        let reveal_start = 0.15 + (1.0 - vertical) * 0.40;
        let reveal = smoothstep(reveal_start, reveal_start + 0.30, t);
        if reveal <= 0.001 {
            continue;
        }
        let travel = (1.0 - reveal) * 0.10;
        let settle = if (0.85..1.25).contains(&t) {
            1.0 + 0.012 * ((t - 0.85) / 0.40 * std::f32::consts::PI).sin()
        } else {
            1.0
        };
        let diagonal = ((dot.x + 0.9) + (0.95 - dot.y)) / 3.7;
        let front = ((t - 1.20) / 0.55).clamp(0.0, 1.0);
        let ripple = if (1.20..1.75).contains(&t) {
            (-((diagonal - front) / 0.13).powi(2)).exp()
        } else {
            0.0
        };
        let local_scale = 0.70 + 0.30 * reveal;
        let x = dot.x * settle;
        let y = (dot.y + travel) * settle;
        let sx = (sub_width as f32 * 0.5 + x * sub_width as f32 * 0.43).round() as i32;
        let sy = (sub_height as f32 * 0.49 + y * sub_height as f32 * 0.47).round() as i32;
        let radius = ((dot.size / 3.0) * local_scale * (1.0 + ripple * 0.10)).max(0.48);
        let color = gradient(vertical, ripple * 0.20, model_accent);
        let extent = radius.ceil() as i32;

        for py in (sy - extent)..=(sy + extent) {
            for px in (sx - extent)..=(sx + extent) {
                if px < 0 || py < 0 || px >= sub_width as i32 || py >= sub_height as i32 {
                    continue;
                }
                let distance = (((px - sx).pow(2) + (py - sy).pow(2)) as f32).sqrt();
                if distance > radius + 0.15 {
                    continue;
                }
                let cell_x = px as usize / 2;
                let cell_y = py as usize / 4;
                let bit = braille_bit(px as usize % 2, py as usize % 4);
                let alpha = reveal * (1.0 - (distance / (radius + 0.5)).powi(2)).max(0.28);
                let cell = &mut cells[cell_y * width + cell_x];
                cell.bits |= bit;
                cell.r += color.0 as f32 * alpha;
                cell.g += color.1 as f32 * alpha;
                cell.b += color.2 as f32 * alpha;
                cell.weight += alpha;
            }
        }
    }

    (0..symbol_rows)
        .map(|row| {
            let mut line = String::with_capacity(width * 4);
            for column in 0..width {
                let cell = cells[row * width + column];
                if cell.bits == 0 {
                    line.push(' ');
                } else {
                    let weight = cell.weight.max(0.001);
                    let color = (
                        (cell.r / weight) as u8,
                        (cell.g / weight) as u8,
                        (cell.b / weight) as u8,
                    );
                    let glyph = char::from_u32(0x2800 + cell.bits as u32).unwrap_or(' ');
                    line.push_str(&theme.rgb_fg(color, &glyph.to_string()));
                }
            }
            line
        })
        .collect()
}

fn geometry() -> Vec<Dot> {
    let mut dots = Vec::with_capacity(220);
    for (band, ry) in [0.22_f32, 0.32, 0.42, 0.52, 0.62, 0.72]
        .into_iter()
        .enumerate()
    {
        let rx = ry * 1.18;
        let count = 11 + band * 6;
        for i in 0..count {
            let u = (i as f32 + 0.5 * (band % 2) as f32) / count as f32;
            let angle = -2.92 + u * 2.70;
            dots.push(dot(rx * angle.cos(), -0.23 + ry * angle.sin(), dots.len()));
        }
    }
    add_curve(&mut dots, 18, |u| (0.0, 0.57 - u * 1.02));
    add_mirrored_curve(&mut dots, 12, |u| {
        (
            0.52 * u - 0.08 * u * (1.0 - u),
            0.05 - 0.57 * u + 0.16 * u * u,
        )
    });
    add_curve(&mut dots, 7, |u| (0.0, -0.38 - 0.42 * u));
    add_mirrored_curve(&mut dots, 10, |u| {
        (0.42 * u, 0.53 + 0.33 * u - 0.08 * u * (1.0 - u))
    });
    add_curve(&mut dots, 6, |u| (0.0, 0.58 + 0.35 * u));
    dots
}

fn dot(x: f32, y: f32, index: usize) -> Dot {
    let variation = ((index as f32 * 2.399_963).sin() + 1.0) * 0.5;
    Dot {
        x,
        y,
        size: 1.5 + 1.5 * variation,
    }
}

fn add_curve<F: Fn(f32) -> (f32, f32)>(dots: &mut Vec<Dot>, count: usize, curve: F) {
    for i in 0..count {
        let u = (i + 1) as f32 / (count + 1) as f32;
        let (x, y) = curve(u);
        dots.push(dot(x, y, dots.len()));
    }
}

fn add_mirrored_curve<F: Fn(f32) -> (f32, f32)>(dots: &mut Vec<Dot>, count: usize, curve: F) {
    for i in 0..count {
        let u = (i + 1) as f32 / (count + 1) as f32;
        let (x, y) = curve(u);
        let seed = dots.len();
        let mut right = dot(x, y, seed);
        dots.push(right);
        right.x = -right.x;
        dots.push(right);
    }
}

fn braille_bit(x: usize, y: usize) -> u8 {
    match (x, y) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (0, 3) => 0x40,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (1, 3) => 0x80,
        _ => 0,
    }
}

fn gradient(position: f32, lift: f32, model_accent: Option<(u8, u8, u8)>) -> Rgb {
    let stops = [
        Rgb(0x4b, 0x8d, 0xff),
        Rgb(0x45, 0xd9, 0xe8),
        Rgb(0x54, 0xe6, 0xb5),
        Rgb(0x8d, 0xff, 0x6a),
    ];
    let p = (1.0 - position).clamp(0.0, 1.0) * 3.0;
    let index = (p.floor() as usize).min(2);
    let mut color = mix(stops[index], stops[index + 1], p - index as f32);
    if let Some((red, green, blue)) = model_accent {
        // Keep the moving multi-stop gradient while adapting it to the active
        // model family in the compiled default theme.
        color = mix(color, Rgb(red, green, blue), 0.58);
    }
    mix(color, Rgb(255, 255, 255), lift)
}

fn smoothstep(a: f32, b: f32, value: f32) -> f32 {
    let x = ((value - a) / (b - a)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn mix(a: Rgb, b: Rgb, amount: f32) -> Rgb {
    let channel = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * amount) as u8;
    Rgb(channel(a.0, b.0), channel(a.1, b.1), channel(a.2, b.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_count_is_within_art_direction() {
        assert!((180..=240).contains(&geometry().len()));
    }
}
