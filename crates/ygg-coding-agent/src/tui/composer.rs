#![allow(missing_docs)]

//! Composer attachment machinery: paste classification, the attachment
//! ledger with placeholder chips, and submit-time composition into parts.

use std::path::{Path, PathBuf};

use ygg_ai::AudioFormat;

/// A paste larger than either bound collapses to a placeholder chip.
pub const LARGE_PASTE_LINES: usize = 10;
pub const LARGE_PASTE_CHARS: usize = 2048;
/// Attach-time size caps, aligned with common provider limits.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
pub const MAX_AUDIO_BYTES: u64 = 25 * 1024 * 1024;

/// Media classification of a file path, by extension (no content sniffing).
#[derive(Clone, Debug, PartialEq)]
pub enum MediaKind {
    Image(mime::Mime),
    Audio(AudioFormat),
}

pub fn media_kind_for_path(path: &Path) -> Option<MediaKind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some(MediaKind::Image(mime::IMAGE_PNG)),
        "jpg" | "jpeg" => Some(MediaKind::Image(mime::IMAGE_JPEG)),
        "gif" => Some(MediaKind::Image(mime::IMAGE_GIF)),
        "webp" => Some(MediaKind::Image("image/webp".parse().expect("static mime"))),
        "wav" => Some(MediaKind::Audio(AudioFormat::Wav)),
        "mp3" => Some(MediaKind::Audio(AudioFormat::Mp3)),
        "flac" => Some(MediaKind::Audio(AudioFormat::Flac)),
        "opus" => Some(MediaKind::Audio(AudioFormat::Opus)),
        "aac" | "m4a" => Some(MediaKind::Audio(AudioFormat::Aac)),
        _ => None,
    }
}

/// Interpret a paste payload as a dropped/pasted file path, if it is one.
///
/// Terminals deliver drag-drops as the path text, variously shell-escaped
/// (`My\ File.png`), quoted (`'My File.png'`), or as a `file://` URL.
/// Returns the path only when the file exists.
pub fn parse_dropped_path(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(trimmed);
    let unescaped = unquoted.replace("\\ ", " ");
    let expanded = if let Some(rest) = unescaped.strip_prefix("file://") {
        // file://localhost/... and percent-encoding are out of scope; plain
        // file:///path is the shape macOS terminals produce.
        rest.trim_start_matches("localhost").to_owned()
    } else if let Some(rest) = unescaped.strip_prefix("~/") {
        let home = dirs::home_dir()?;
        return existing_file(home.join(rest));
    } else {
        unescaped
    };
    existing_file(PathBuf::from(expanded))
}

fn existing_file(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

/// How a paste payload should enter the composer.
#[derive(Clone, Debug, PartialEq)]
pub enum PasteKind {
    Verbatim,
    LargeText,
    MediaFile(PathBuf),
    NonMediaFile(PathBuf),
}

pub fn classify_paste(text: &str) -> PasteKind {
    if let Some(path) = parse_dropped_path(text) {
        return if media_kind_for_path(&path).is_some() {
            PasteKind::MediaFile(path)
        } else {
            PasteKind::NonMediaFile(path)
        };
    }
    if text.lines().count() > LARGE_PASTE_LINES || text.chars().count() > LARGE_PASTE_CHARS {
        return PasteKind::LargeText;
    }
    PasteKind::Verbatim
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn extension_map_matches_the_spec() {
        assert_eq!(
            media_kind_for_path(Path::new("a.PNG")),
            Some(MediaKind::Image(mime::IMAGE_PNG))
        );
        assert_eq!(
            media_kind_for_path(Path::new("b.m4a")),
            Some(MediaKind::Audio(AudioFormat::Aac))
        );
        assert_eq!(media_kind_for_path(Path::new("c.rs")), None);
        assert_eq!(media_kind_for_path(Path::new("noext")), None);
    }

    #[test]
    fn dropped_paths_are_unescaped_unquoted_and_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("shot.png");
        let spaced = dir.path().join("my shot.png");
        fs::write(&plain, b"x").unwrap();
        fs::write(&spaced, b"x").unwrap();

        assert_eq!(
            parse_dropped_path(&plain.display().to_string()),
            Some(plain.clone())
        );
        let escaped = spaced.display().to_string().replace(' ', "\\ ");
        assert_eq!(parse_dropped_path(&escaped), Some(spaced.clone()));
        assert_eq!(
            parse_dropped_path(&format!("'{}'", spaced.display())),
            Some(spaced.clone())
        );
        assert_eq!(
            parse_dropped_path(&format!("file://{}", plain.display())),
            Some(plain.clone())
        );
        assert_eq!(
            parse_dropped_path(&dir.path().join("missing.png").display().to_string()),
            None
        );
        assert_eq!(parse_dropped_path("just some words"), None);
    }

    #[test]
    fn paste_classification_follows_spec_order() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        let source = dir.path().join("main.rs");
        fs::write(&image, b"x").unwrap();
        fs::write(&source, b"x").unwrap();

        assert_eq!(
            classify_paste(&image.display().to_string()),
            PasteKind::MediaFile(image)
        );
        assert_eq!(
            classify_paste(&source.display().to_string()),
            PasteKind::NonMediaFile(source)
        );
        assert_eq!(classify_paste("short text"), PasteKind::Verbatim);
        assert_eq!(classify_paste(&"line\n".repeat(11)), PasteKind::LargeText);
        assert_eq!(classify_paste(&"x".repeat(2049)), PasteKind::LargeText);
        // Exactly at the bounds stays verbatim.
        assert_eq!(classify_paste(&"x".repeat(2048)), PasteKind::Verbatim);
    }
}
