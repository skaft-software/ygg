#![allow(missing_docs)]

//! Composer attachment machinery: paste classification, the attachment
//! ledger with placeholder chips, and submit-time composition into parts.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use ygg_agent::{InputPart, UserInput};
use ygg_ai::{AudioFormat, Media, Modality, ModalitySet};

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
        if let Some(path) = rest.strip_prefix("localhost") {
            path.to_owned()
        } else {
            rest.to_owned()
        }
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

/// List workspace files (relative, sorted, gitignore-aware), capped.
pub fn workspace_files(root: &Path, cap: usize) -> Vec<String> {
    let mut files = Vec::new();
    // `require_git(false)` honors .gitignore files even when the workspace is
    // not (yet) a git repository, which is also useful for new projects.
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .require_git(false)
        .build();
    for entry in walker.flatten() {
        if files.len() >= cap {
            break;
        }
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            if let Ok(relative) = entry.path().strip_prefix(root) {
                files.push(relative.to_string_lossy().into_owned());
            }
        }
    }
    files.sort();
    files
}

/// The mention query when the text ends in an `@`-prefixed token.
pub fn active_mention(text: &str) -> Option<&str> {
    if text.starts_with('/') || text.chars().last().is_some_and(char::is_whitespace) {
        return None;
    }
    let token = text.split_whitespace().next_back()?;
    token.strip_prefix('@')
}

/// Case-insensitive substring match on relative paths; earlier and shorter
/// matches rank first.
pub fn mention_matches<'a>(files: &'a [String], query: &str, limit: usize) -> Vec<&'a str> {
    let needle = query.to_lowercase();
    let mut scored: Vec<(usize, usize, &str)> = files
        .iter()
        .filter_map(|file| {
            file.to_lowercase()
                .find(&needle)
                .map(|at| (at, file.len(), file.as_str()))
        })
        .collect();
    scored.sort();
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, file)| file)
        .collect()
}

/// What a chip stands for.
#[derive(Clone, Debug)]
pub enum AttachmentPayload {
    PastedText(String),
    Media { media: Media, byte_len: u64 },
}

/// One chip-backed attachment awaiting submit.
#[derive(Clone, Debug)]
pub struct Attachment {
    pub id: u64,
    pub chip: String,
    pub payload: AttachmentPayload,
}

/// Why an attach was refused. Rendered as a composer notice.
#[derive(Debug)]
pub enum AttachError {
    Unreadable(String),
    TooLarge { limit_bytes: u64 },
    UnsupportedModality { modality: &'static str },
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(reason) => write!(f, "cannot read file: {reason}"),
            Self::TooLarge { limit_bytes } => {
                write!(
                    f,
                    "file exceeds the {} MB limit",
                    limit_bytes / (1024 * 1024)
                )
            }
            Self::UnsupportedModality { modality } => {
                write!(f, "the active model does not accept {modality} input")
            }
        }
    }
}

/// Chip-keyed attachments owned by the composer.
#[derive(Clone, Debug, Default)]
pub struct AttachmentLedger {
    next_id: u64,
    entries: Vec<Attachment>,
}

impl AttachmentLedger {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Collapse a large paste into a chip; the text returns at compose time.
    pub fn attach_pasted_text(&mut self, text: String) -> String {
        let id = self.next_id();
        let lines = text.lines().count();
        let chip = format!("[Pasted text #{id}: {lines} lines]");
        self.entries.push(Attachment {
            id,
            chip: chip.clone(),
            payload: AttachmentPayload::PastedText(text),
        });
        chip
    }

    /// Gate, cap, read, and record a media file. Returns the chip on success.
    pub fn attach_media(
        &mut self,
        path: &Path,
        modalities: ModalitySet,
    ) -> Result<String, AttachError> {
        let kind = media_kind_for_path(path)
            .ok_or_else(|| AttachError::Unreadable("unsupported media extension".into()))?;
        let (label, modality, modality_name, limit) = match &kind {
            MediaKind::Image(_) => ("Image", Modality::Image, "image", MAX_IMAGE_BYTES),
            MediaKind::Audio(_) => ("Audio", Modality::Audio, "audio", MAX_AUDIO_BYTES),
        };
        if !modalities.contains(modality) {
            return Err(AttachError::UnsupportedModality {
                modality: modality_name,
            });
        }
        let metadata = fs::metadata(path).map_err(|e| AttachError::Unreadable(e.to_string()))?;
        if metadata.len() > limit {
            return Err(AttachError::TooLarge { limit_bytes: limit });
        }
        let file = std::fs::File::open(path).map_err(|e| AttachError::Unreadable(e.to_string()))?;
        let mut limited = file.take(limit + 1);
        let mut data = Vec::new();
        limited
            .read_to_end(&mut data)
            .map_err(|e| AttachError::Unreadable(e.to_string()))?;
        if data.len() as u64 > limit {
            return Err(AttachError::TooLarge { limit_bytes: limit });
        }
        let byte_len = data.len() as u64;
        let media = match kind {
            MediaKind::Image(mime) => Media::image_bytes(bytes::Bytes::from(data), mime),
            MediaKind::Audio(format) => Media::audio_bytes(bytes::Bytes::from(data), format),
        };
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let id = self.next_id();
        let chip = format!("[{label} #{id}: {file_name}]");
        self.entries.push(Attachment {
            id,
            chip: chip.clone(),
            payload: AttachmentPayload::Media { media, byte_len },
        });
        Ok(chip)
    }

    /// Put restored steering attachments back (chips re-enter the editor
    /// alongside them). IDs continue from the highest ever issued.
    pub fn restore(&mut self, entries: Vec<Attachment>) {
        if let Some(highest) = entries.iter().map(|entry| entry.id).max() {
            self.next_id = self.next_id.max(highest);
        }
        self.entries.extend(entries);
    }

    fn take_all(&mut self) -> Vec<Attachment> {
        std::mem::take(&mut self.entries)
    }
}

/// The drained composer content: display text, ordered parts, and the
/// attachments the parts were built from (kept for steering restore).
#[derive(Clone, Debug)]
pub struct ComposedInput {
    pub display_text: String,
    pub parts: Vec<InputPart>,
    pub attachments: Vec<Attachment>,
}

impl ComposedInput {
    pub fn from_text(text: String) -> Self {
        Self {
            parts: vec![InputPart::Text(text.clone())],
            display_text: text,
            attachments: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.parts.iter().all(|part| match part {
            InputPart::Text(text) => text.trim().is_empty(),
            InputPart::Media(_) => false,
        })
    }

    pub fn text_for_estimate(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                InputPart::Text(text) => Some(text.as_str()),
                InputPart::Media(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn into_user_input(self) -> UserInput {
        UserInput::from(self.parts)
    }
}

/// Resolve chips against the ledger, draining it entirely.
pub fn compose(display_text: String, ledger: &mut AttachmentLedger) -> ComposedInput {
    let entries = ledger.take_all();
    // Locate the first occurrence of each entry's chip; unmatched entries drop.
    let mut found: Vec<(usize, &Attachment)> = entries
        .iter()
        .filter_map(|entry| display_text.find(&entry.chip).map(|at| (at, entry)))
        .collect();
    found.sort_by_key(|(at, _)| *at);

    let mut parts: Vec<InputPart> = Vec::new();
    let mut text_run = String::new();
    let mut cursor = 0usize;
    for (at, entry) in &found {
        // Overlapping matches cannot happen: chips contain a unique "#id".
        text_run.push_str(&display_text[cursor..*at]);
        match &entry.payload {
            AttachmentPayload::PastedText(pasted) => text_run.push_str(pasted),
            AttachmentPayload::Media { media, byte_len } => {
                let limit = match media {
                    Media::Image(_) => MAX_IMAGE_BYTES,
                    Media::Audio(_) => MAX_AUDIO_BYTES,
                };
                debug_assert!(*byte_len <= limit, "attached media exceeded its size cap");
                if !text_run.is_empty() {
                    parts.push(InputPart::Text(std::mem::take(&mut text_run)));
                }
                parts.push(InputPart::Media(media.clone()));
            }
        }
        cursor = at + entry.chip.len();
    }
    text_run.push_str(&display_text[cursor..]);
    if !text_run.is_empty() || parts.is_empty() {
        parts.push(InputPart::Text(text_run));
    }

    let matched = found.into_iter().map(|(_, entry)| entry.clone()).collect();
    ComposedInput {
        display_text,
        parts,
        attachments: matched,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use ygg_ai::{Modality, ModalitySet};

    fn all_modalities() -> ModalitySet {
        ModalitySet::none()
            .with(Modality::Image)
            .with(Modality::Audio)
    }

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

    #[test]
    fn workspace_files_lists_relative_paths_and_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), b"x").unwrap();
        fs::write(dir.path().join("shot.png"), b"x").unwrap();
        fs::write(dir.path().join(".gitignore"), b"ignored.txt\n").unwrap();
        fs::write(dir.path().join("ignored.txt"), b"x").unwrap();

        let files = workspace_files(dir.path(), 100);
        assert!(files.contains(&"src/main.rs".to_owned()));
        assert!(files.contains(&"shot.png".to_owned()));
        assert!(!files.iter().any(|file| file.contains("ignored")));
    }

    #[test]
    fn active_mention_is_the_trailing_at_token() {
        assert_eq!(active_mention("look at @sr"), Some("sr"));
        assert_eq!(active_mention("@"), Some(""));
        assert_eq!(active_mention("email a@b.com"), None);
        assert_eq!(active_mention("no mention"), None);
        assert_eq!(active_mention("ends with space @x "), None);
    }

    #[test]
    fn mention_matches_rank_by_position_then_length() {
        let files = vec![
            "src/main.rs".to_owned(),
            "docs/main-notes.md".to_owned(),
            "main.rs".to_owned(),
        ];
        let matches = mention_matches(&files, "main", 10);
        assert_eq!(matches[0], "main.rs");
        assert!(matches.contains(&"src/main.rs"));
        assert!(mention_matches(&files, "zzz", 10).is_empty());
    }

    #[test]
    fn attach_media_reads_bytes_and_returns_a_chip() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        fs::write(&image, b"pngbytes").unwrap();

        let mut ledger = AttachmentLedger::default();
        let chip = ledger.attach_media(&image, all_modalities()).unwrap();
        assert_eq!(chip, "[Image #1: shot.png]");
        assert!(!ledger.is_empty());
    }

    #[test]
    fn attach_media_gates_on_modality_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("memo.wav");
        fs::write(&audio, b"wav").unwrap();

        let image_only = ModalitySet::none().with(Modality::Image);
        let mut ledger = AttachmentLedger::default();
        assert!(matches!(
            ledger.attach_media(&audio, image_only),
            Err(AttachError::UnsupportedModality { modality: "audio" })
        ));

        let big = dir.path().join("big.png");
        fs::write(&big, vec![0u8; (MAX_IMAGE_BYTES + 1) as usize]).unwrap();
        assert!(matches!(
            ledger.attach_media(&big, all_modalities()),
            Err(AttachError::TooLarge { .. })
        ));
        assert!(ledger.is_empty());
    }

    #[test]
    fn compose_splits_text_and_media_preserving_order() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        fs::write(&image, b"pngbytes").unwrap();

        let mut ledger = AttachmentLedger::default();
        let chip = ledger.attach_media(&image, all_modalities()).unwrap();
        let composed = compose(format!("before {chip} after"), &mut ledger);

        assert_eq!(composed.display_text, format!("before {chip} after"));
        assert_eq!(composed.parts.len(), 3);
        assert!(
            matches!(&composed.parts[0], ygg_agent::InputPart::Text(t) if t.trim() == "before")
        );
        assert!(matches!(
            &composed.parts[1],
            ygg_agent::InputPart::Media(ygg_ai::Media::Image(_))
        ));
        assert!(matches!(&composed.parts[2], ygg_agent::InputPart::Text(t) if t.trim() == "after"));
        assert!(ledger.is_empty());
    }

    #[test]
    fn compose_splices_pasted_text_in_place() {
        let mut ledger = AttachmentLedger::default();
        let pasted = "l1\nl2\nl3".to_owned();
        let chip = ledger.attach_pasted_text(pasted.clone());
        assert_eq!(chip, "[Pasted text #1: 3 lines]");

        let composed = compose(format!("context: {chip}"), &mut ledger);
        assert_eq!(composed.parts.len(), 1);
        assert!(
            matches!(&composed.parts[0], ygg_agent::InputPart::Text(t) if t == &format!("context: {pasted}"))
        );
    }

    #[test]
    fn compose_drops_orphans_and_keeps_mangled_chips_literal() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        fs::write(&image, b"pngbytes").unwrap();

        let mut ledger = AttachmentLedger::default();
        let _chip = ledger.attach_media(&image, all_modalities()).unwrap();
        // The user deleted part of the chip: entry is orphaned, text is literal.
        let composed = compose("[Image #1: shot.pn".to_owned(), &mut ledger);
        assert_eq!(composed.parts.len(), 1);
        assert!(
            matches!(&composed.parts[0], ygg_agent::InputPart::Text(t) if t == "[Image #1: shot.pn")
        );
        assert!(ledger.is_empty());
    }

    #[test]
    fn composed_input_emptiness_and_estimate_text() {
        assert!(ComposedInput::from_text("   \n".to_owned()).is_empty());
        assert!(!ComposedInput::from_text("hi".to_owned()).is_empty());

        let mut ledger = AttachmentLedger::default();
        let chip = ledger.attach_pasted_text("body".to_owned());
        let composed = compose(chip, &mut ledger);
        assert_eq!(composed.text_for_estimate(), "body");
        assert!(!composed.is_empty());
    }

    #[test]
    fn file_url_localhost_strips_only_the_hostname_segment() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("images").join("file.png");
        fs::create_dir_all(plain.parent().unwrap()).unwrap();
        fs::write(&plain, b"x").unwrap();

        let url = format!("file://localhost{}", plain.display());
        let parsed = parse_dropped_path(&url);
        assert_eq!(parsed, Some(plain.clone()));
    }

    #[test]
    fn file_url_triple_slash_localhostimages_remains_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("localhostimages").join("file.png");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"x").unwrap();

        let url = format!("file://{}", file.display());
        let parsed = parse_dropped_path(&url);
        assert_eq!(parsed, Some(file.clone()));
    }

    #[test]
    fn attach_media_bounds_read_to_limit_plus_one_to_prevent_toctou_alloc() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        let honest_data = vec![0u8; MAX_IMAGE_BYTES as usize];
        fs::write(&image, &honest_data).unwrap();

        let mut ledger = AttachmentLedger::default();
        let result = ledger.attach_media(&image, all_modalities());
        assert!(result.is_ok(), "file at exact limit should succeed");

        let oversized = dir.path().join("big.png");
        fs::write(&oversized, vec![0u8; (MAX_IMAGE_BYTES + 1) as usize]).unwrap();
        let mut ledger2 = AttachmentLedger::default();
        let result2 = ledger2.attach_media(&oversized, all_modalities());
        assert!(
            matches!(result2, Err(AttachError::TooLarge { .. })),
            "file over limit must be rejected without allocating the full file"
        );
    }
}
