//! Terminal image support (Kitty/iTerm2 graphics protocols).

use std::sync::atomic::{AtomicU32, Ordering};

pub use crate::capabilities::TerminalCapabilities;

/// Supported image protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    ITerm2,
    None,
}

/// Terminal cell dimensions.
#[derive(Debug, Clone, Copy)]
pub struct CellDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

impl Default for CellDimensions {
    fn default() -> Self {
        CellDimensions {
            width_px: 10,
            height_px: 20,
        }
    }
}

/// Image dimensions.
#[derive(Debug, Clone, Copy)]
pub struct ImageDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

/// Options for rendering an image.
#[derive(Debug, Clone)]
pub struct ImageRenderOptions {
    pub max_width_cells: Option<u32>,
    pub max_height_cells: Option<u32>,
    pub filename: Option<String>,
}

static NEXT_IMAGE_ID: AtomicU32 = AtomicU32::new(1);

/// Allocate a unique image ID for Kitty graphics protocol.
pub fn allocate_image_id() -> u32 {
    NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Check if a line contains a Kitty image sequence.
pub fn is_image_line(line: &str) -> bool {
    line.contains("\x1b_G")
}

/// Encode image data in Kitty graphics protocol format.
pub fn encode_kitty(
    image_id: u32,
    base64_data: &str,
    dims: ImageDimensions,
    opts: &ImageRenderOptions,
    cell_dims: CellDimensions,
) -> String {
    let base64_data = canonical_base64(base64_data).unwrap_or_default();
    let rows = calculate_image_rows(dims, opts, cell_dims);
    let cell_width = cell_dims.width_px.max(1);
    let cols = if let Some(max_w) = opts.max_width_cells {
        ((dims.width_px as f64 / cell_width as f64).ceil() as u32).min(max_w)
    } else {
        (dims.width_px as f64 / cell_width as f64).ceil() as u32
    }
    .max(1);

    format!(
        "\x1b_Ga=T,f=100,i={},s={},v={},c={},r={};{}\x1b\\",
        image_id, dims.width_px, dims.height_px, cols, rows, base64_data
    )
}

/// Encode image data in iTerm2 inline image format.
pub fn encode_iterm2(
    base64_data: &str,
    dims: ImageDimensions,
    opts: &ImageRenderOptions,
    _cell_dims: CellDimensions,
) -> String {
    let base64_data = canonical_base64(base64_data).unwrap_or_default();
    let width = opts.max_width_cells.unwrap_or(dims.width_px);
    let height = calculate_image_rows(dims, opts, CellDimensions::default());
    format!(
        "\x1b]1337;File=inline=1;width={}px;height={}px;preserveAspectRatio=1:{}\x07",
        width, height, base64_data
    )
}

/// Calculate the number of terminal rows an image will occupy.
pub fn calculate_image_rows(
    dims: ImageDimensions,
    opts: &ImageRenderOptions,
    cell_dims: CellDimensions,
) -> u32 {
    let max_h = opts.max_height_cells.unwrap_or(u32::MAX).max(1);
    let width = dims.width_px.max(1);
    let height = dims.height_px.max(1);
    let cell_width = cell_dims.width_px.max(1);
    let cell_height = cell_dims.height_px.max(1);
    let aspect = width as f64 / height as f64;
    let cell_aspect = cell_width as f64 / cell_height as f64;
    let adjusted_height = (width as f64 / cell_width as f64 / aspect * cell_aspect).ceil() as u32;
    adjusted_height.min(max_h).max(1)
}

/// Delete a specific Kitty image from the terminal.
pub fn delete_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={}\x1b\\", image_id)
}

/// Delete all Kitty images from the terminal.
pub fn delete_all_kitty_images() -> String {
    "\x1b_Ga=d\x1b\\".to_string()
}

/// Create a capability-aware, injection-safe OSC 8 hyperlink. When OSC 8 is
/// disabled or unsafe, the destination remains visible as plain text.
pub fn hyperlink(text: &str, url: &str) -> String {
    let capabilities = get_capabilities();
    crate::sanitize::safe_hyperlink(text, url, capabilities.hyperlinks, !capabilities.unicode)
}

/// Fallback text for image display on unsupported terminals.
pub fn image_fallback(
    mime_type: &str,
    dims: Option<ImageDimensions>,
    filename: Option<&str>,
) -> String {
    let name = crate::sanitize::sanitize_line(filename.unwrap_or("image"), true);
    let mime_type = crate::sanitize::sanitize_line(mime_type, true);
    if let Some(d) = dims {
        format!("[{name}: {mime_type} {}x{}px]", d.width_px, d.height_px)
    } else {
        format!("[{name}: {mime_type}]")
    }
}

use std::sync::Mutex;
static CAPABILITIES: Mutex<Option<TerminalCapabilities>> = Mutex::new(None);
static CELL_DIMS: Mutex<Option<CellDimensions>> = Mutex::new(None);

/// Recover from a poisoned mutex. If the lock is poisoned, return the inner
/// value anyway (a panic in one holder shouldn't break the whole TUI).
fn lock_or_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Detect terminal capabilities without writing probe sequences.
pub fn detect_capabilities() -> TerminalCapabilities {
    TerminalCapabilities::detect()
}

/// Get cached capabilities.
pub fn get_capabilities() -> TerminalCapabilities {
    let mut guard = lock_or_recover(&CAPABILITIES);
    if guard.is_none() {
        *guard = Some(detect_capabilities());
    }
    (*guard).unwrap()
}

/// Reset the capabilities cache.
pub fn reset_capabilities_cache() {
    *lock_or_recover(&CAPABILITIES) = None;
}

/// Set capabilities explicitly.
pub fn set_capabilities(caps: TerminalCapabilities) {
    *lock_or_recover(&CAPABILITIES) = Some(caps);
}

/// Get cell dimensions (for image size calculations).
pub fn get_cell_dimensions() -> CellDimensions {
    lock_or_recover(&CELL_DIMS).unwrap_or_default()
}

/// Set cell dimensions.
pub fn set_cell_dimensions(dims: CellDimensions) {
    *lock_or_recover(&CELL_DIMS) = Some(dims);
}

/// Decode a base64 string into raw bytes.
fn canonical_base64(data: &str) -> Option<String> {
    let mut compact = String::with_capacity(data.len());
    for byte in data.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=') {
            compact.push(char::from(byte));
        } else {
            return None;
        }
    }
    (!compact.is_empty()).then_some(compact)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn decode_base64(data: &str) -> Option<Vec<u8>> {
    let compact = canonical_base64(data)?;
    let bytes: Vec<u8> = compact
        .bytes()
        .take_while(|byte| *byte != b'=')
        .map(base64_value)
        .collect::<Option<_>>()?;

    if bytes.is_empty() {
        return None;
    }

    let mut out = Vec::with_capacity((bytes.len() * 3) / 4 + 1);
    for chunk in bytes.chunks(4) {
        let a = chunk.first().copied().unwrap_or(0) as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let d = chunk.get(3).copied().unwrap_or(0) as u32;
        let triple = (a << 18) | (b << 12) | (c << 6) | d;
        out.push(((triple >> 16) & 0xFF) as u8);
        if chunk.len() > 2 {
            out.push(((triple >> 8) & 0xFF) as u8);
        }
        if chunk.len() > 3 {
            out.push((triple & 0xFF) as u8);
        }
    }
    Some(out)
}

/// Read a big-endian u32 at `offset` from `buf`.
fn read_u32_be(buf: &[u8], offset: usize) -> Option<u32> {
    let bytes = buf.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Get PNG image dimensions from base64 data.
pub fn get_png_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let data = decode_base64(base64_data)?;
    // PNG signature: 8 bytes, then IHDR chunk at offset 8.
    // IHDR: 4 bytes length (always 13), 4 bytes "IHDR",
    //        4 bytes width, 4 bytes height.
    if data.len() < 24 {
        return None;
    }
    let width = read_u32_be(&data, 16)?;
    let height = read_u32_be(&data, 20)?;
    Some(ImageDimensions {
        width_px: width,
        height_px: height,
    })
}

/// Get JPEG image dimensions from base64 data.
pub fn get_jpeg_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let data = decode_base64(base64_data)?;
    // Walk JPEG markers looking for SOF0 (0xC0) – SOF2 (0xC2).
    let mut pos = 2; // skip SOI marker (0xFF 0xD8)
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            break;
        }
        let marker = data[pos + 1];
        pos += 2;
        // SOF0 – SOF2 markers contain dimensions
        if (0xC0..=0xC2).contains(&marker) && pos + 7 <= data.len() {
            // Skip length (2 bytes), precision (1 byte)
            let height = u16::from_be_bytes([data[pos + 3], data[pos + 4]]) as u32;
            let width = u16::from_be_bytes([data[pos + 5], data[pos + 6]]) as u32;
            return Some(ImageDimensions {
                width_px: width,
                height_px: height,
            });
        }
        // Other markers: skip over them
        if pos + 2 > data.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos = pos.saturating_add(seg_len);
    }
    None
}

/// Get GIF image dimensions from base64 data.
pub fn get_gif_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let data = decode_base64(base64_data)?;
    if data.len() < 10 {
        return None;
    }
    // GIF header: 6 bytes ("GIF87a" or "GIF89a"), then little-endian width/height.
    let width = u16::from_le_bytes([data[6], data[7]]) as u32;
    let height = u16::from_le_bytes([data[8], data[9]]) as u32;
    Some(ImageDimensions {
        width_px: width,
        height_px: height,
    })
}

/// Get WebP image dimensions from base64 data.
pub fn get_webp_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let data = decode_base64(base64_data)?;
    // WebP: "RIFF" header at 0, "WEBP" at 8.
    // VP8  (lossy):  dimensions in 14-bit LE + 14-bit LE from offset 26
    // VP8L (lossless): dimensions in 14-bit LE from offset 25
    // VP8X (extended):  dimensions are 24-bit LE +1 at offset 24
    if data.len() < 30 {
        return None;
    }
    // Check RIFF....WEBP header
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return None;
    }
    let chunk = &data[12..16];
    match chunk {
        b"VP8 " if data.len() > 30 => {
            // Lossy: bytes 26–27 encode width (14 bits), 28–29 encode height (14 bits)
            let w = u16::from_le_bytes([data[26], data[27]]) as u32 & 0x3FFF;
            let h = u16::from_le_bytes([data[28], data[29]]) as u32 & 0x3FFF;
            Some(ImageDimensions {
                width_px: w,
                height_px: h,
            })
        }
        b"VP8L" if data.len() > 25 => {
            // Lossless: bytes 21–24 hold a 32-bit LE value
            // bits 0–13 = width+1, bits 14–27 = height+1
            let bits = u32::from_le_bytes([data[21], data[22], data[23], data[24]]);
            let w = (bits & 0x3FFF) + 1;
            let h = ((bits >> 14) & 0x3FFF) + 1;
            Some(ImageDimensions {
                width_px: w,
                height_px: h,
            })
        }
        b"VP8X" if data.len() > 30 => {
            // Extended: bytes 24–26 = width+1 (LE 24-bit), 27–29 = height+1
            let w = u32::from_le_bytes([data[24], data[25], data[26], 0]) + 1;
            let h = u32::from_le_bytes([data[27], data[28], data[29], 0]) + 1;
            Some(ImageDimensions {
                width_px: w,
                height_px: h,
            })
        }
        _ => None,
    }
}

/// Get image dimensions from base64 data (auto-detect format).
pub fn get_image_dimensions(base64_data: &str, mime_type: &str) -> Option<ImageDimensions> {
    match mime_type {
        "image/png" => get_png_dimensions(base64_data),
        "image/jpeg" | "image/jpg" => get_jpeg_dimensions(base64_data),
        "image/gif" => get_gif_dimensions(base64_data),
        "image/webp" => get_webp_dimensions(base64_data),
        _ => None,
    }
}

/// Render an image to terminal escape sequences.
pub fn render_image(base64_data: &str, mime_type: &str, opts: &ImageRenderOptions) -> String {
    let caps = get_capabilities();
    let cell_dims = get_cell_dimensions();
    let Some(base64_data) = canonical_base64(base64_data) else {
        return image_fallback(mime_type, None, opts.filename.as_deref());
    };
    let dims = get_image_dimensions(&base64_data, mime_type);

    if let Some(d) = dims {
        if caps.kitty_graphics {
            let id = allocate_image_id();
            return encode_kitty(id, &base64_data, d, opts, cell_dims);
        }
        if caps.iterm2_images {
            return encode_iterm2(&base64_data, d, opts, cell_dims);
        }
    }

    image_fallback(mime_type, dims, opts.filename.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_protocol_payloads_cannot_inject_terminal_controls() {
        let options = ImageRenderOptions {
            max_width_cells: Some(10),
            max_height_cells: Some(5),
            filename: Some("bad\x1b]0;title\x07".into()),
        };
        let dimensions = ImageDimensions {
            width_px: 10,
            height_px: 10,
        };
        let kitty = encode_kitty(
            1,
            "AAAA\x1b\\\x1b]52;c;bad\x07",
            dimensions,
            &options,
            CellDimensions::default(),
        );
        assert_eq!(kitty.matches('\x1b').count(), 2);
        assert!(!kitty.contains("52;c"));
        let fallback = image_fallback("image/png\x1b[2J", None, options.filename.as_deref());
        assert!(!fallback.contains('\x1b'));
        assert!(!fallback.contains('\x07'));
    }

    #[test]
    fn zero_dimensions_do_not_divide_by_zero() {
        let rows = calculate_image_rows(
            ImageDimensions {
                width_px: 0,
                height_px: 0,
            },
            &ImageRenderOptions {
                max_width_cells: None,
                max_height_cells: None,
                filename: None,
            },
            CellDimensions {
                width_px: 0,
                height_px: 0,
            },
        );
        assert_eq!(rows, 1);
    }
}
