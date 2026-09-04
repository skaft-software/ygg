//! Bounded, out-of-band terminal image primitives.
//!
//! This module accepts only owned image bytes supplied by the caller; it never
//! reads paths, URLs, environment values, or network resources. It validates a
//! small set of container headers without decompression, derives a bounded cell
//! reservation, and keeps protocol output opaque and separate from semantic
//! text. A renderer can put [`ImageRenderPlan::semantic_rows`] into its
//! copyable frame and emit [`ImageTerminalCommand`] only through a terminal
//! output sink.
//!
//! The protocol matrix is intentionally conservative: Kitty receives only PNG
//! (`f=100`); iTerm2 receives PNG, JPEG, and GIF through OSC 1337. WebP and all
//! other combinations receive deterministic ASCII fallback text rather than a
//! guessed conversion. Animated PNG, GIF, and WebP containers are rejected so a
//! bounded source cannot request unbounded terminal-side frame decoding. iTerm2
//! has no target-ID delete operation, so replace and delete are explicitly
//! unavailable there.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, Write};
use std::num::NonZeroU32;
use std::time::Duration;

use crate::capabilities::CellPixelSize;
use crate::terminal::Terminal;
use crate::TerminalCapabilities;

/// Hard ceiling for an accepted encoded image payload.
pub const HARD_MAX_IMAGE_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
/// Hard ceiling for all bytes emitted by one image protocol command.
pub const HARD_MAX_ENCODED_OUTPUT_BYTES: usize = 24 * 1024 * 1024;
/// Hard ceiling for one base64 protocol chunk.
pub const HARD_MAX_PROTOCOL_CHUNK_BYTES: usize = 4 * 1024;
/// Hard ceiling for chunks emitted by one Kitty transmission.
pub const HARD_MAX_PROTOCOL_CHUNKS: usize = 8_192;
/// Hard ceiling for a validated image width or height.
pub const HARD_MAX_IMAGE_DIMENSION: u32 = 16_384;
/// Hard ceiling for width times height before any decoder is involved.
///
/// At most eight decoded bytes per pixel are conservatively assumed for image
/// containers that can carry 16-bit RGBA samples, keeping even a hostile
/// terminal-side decompressor below a bounded working-set estimate.
pub const HARD_MAX_IMAGE_PIXELS: u64 = 16_000_000;
/// Hard ceiling for container records or sub-blocks inspected during validation.
pub const HARD_MAX_CONTAINER_ITEMS: usize = 8_192;
/// Hard ceiling for JPEG headers inspected before the scan payload.
pub const HARD_MAX_HEADER_BYTES: usize = 64 * 1024;
/// Hard ceiling for a metadata filename.
pub const HARD_MAX_FILENAME_BYTES: usize = 128;
/// Hard ceiling for one terminal capability reply.
pub const HARD_MAX_TERMINAL_REPLY_BYTES: usize = 1_024;
/// Hard ceiling a caller may use as a terminal-query deadline.
pub const HARD_MAX_QUERY_TIMEOUT: Duration = Duration::from_millis(250);
/// Largest number of semantic rows an image reservation may create.
pub const MAX_RESERVED_IMAGE_ROWS: u16 = 512;
/// Largest number of cells requested in one image placement direction.
pub const MAX_IMAGE_CELL_COLUMNS: u16 = 512;
/// Hard ceiling for concurrently live targetable image IDs.
///
/// This bounds the registry's bookkeeping allocation independently of the
/// payload limit. Retiring an ID releases one live slot but never makes its ID
/// value reusable.
pub const HARD_MAX_LIVE_IMAGES: usize = 4_096;

const DEFAULT_MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_ENCODED_OUTPUT_BYTES: usize = 6 * 1024 * 1024;
const DEFAULT_MAX_PROTOCOL_CHUNKS: usize = 2_048;
const DEFAULT_MAX_IMAGE_DIMENSION: u32 = 8_192;
const DEFAULT_MAX_IMAGE_PIXELS: u64 = 4_000_000;
const DEFAULT_MAX_CONTAINER_ITEMS: usize = 2_048;
const DEFAULT_MAX_HEADER_BYTES: usize = 32 * 1024;
const DEFAULT_MAX_FILENAME_BYTES: usize = 96;
const DEFAULT_MAX_TERMINAL_REPLY_BYTES: usize = 512;
const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_millis(75);

const KITTY_ST: &[u8] = b"\x1b\\";
const ITERM_ST: &[u8] = b"\x1b\\";

/// Errors from bounded image validation, layout, or protocol planning.
///
/// Variants deliberately omit hostile payloads, filenames, replies, and raw
/// protocol bytes so logging an error cannot become an escape-injection path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// A caller attempted to configure a value outside a fixed hard bound.
    InvalidLimit,
    /// Input bytes exceeded the configured payload cap.
    PayloadTooLarge,
    /// A protocol command would exceed the configured output cap.
    EncodedOutputTooLarge,
    /// A protocol transmission would require too many chunks.
    TooManyChunks,
    /// The input did not form a minimally valid, bounded image container.
    InvalidImage,
    /// The input did not begin with PNG, JPEG, GIF, or WebP container bytes.
    UnsupportedFormat,
    /// The container declares more than one animation frame.
    UnsupportedAnimation,
    /// A dimension was zero or cannot be represented by the requested layout.
    InvalidDimensions,
    /// A dimension exceeded the configured width or height cap.
    DimensionsTooLarge,
    /// Width times height exceeded the configured pixel cap.
    PixelCountTooLarge,
    /// Container records or header bytes exceeded a configured parsing cap.
    MetadataTooLarge,
    /// A filename was empty, too long, path-like, or contained unsafe bytes.
    UnsafeFilename,
    /// Caller-provided dimensions did not match the validated container header.
    MetadataDimensionMismatch,
    /// A requested cell layout was zero or exceeded the semantic-row cap.
    InvalidLayout,
    /// A bounded copy could not reserve its destination allocation.
    AllocationFailed,
    /// Image ID zero is not valid for a targetable Kitty operation.
    InvalidImageId,
    /// The monotonic ID allocator has no non-reused values left.
    ImageIdExhausted,
    /// Registry bookkeeping reached its hard concurrent-live-ID cap.
    TooManyLiveImages,
    /// Replace or delete referenced an ID that is not currently live.
    StaleImageId,
    /// The selected protocol does not accept the source image format directly.
    UnsupportedFormatForProtocol,
    /// The selected protocol has no safe implementation for this operation.
    UnsupportedOperation,
    /// An operation was supplied to an incompatible planner or encoder method.
    InvalidAction,
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidLimit => "invalid terminal image limit",
            Self::PayloadTooLarge => "terminal image payload exceeds its bound",
            Self::EncodedOutputTooLarge => "terminal image protocol output exceeds its bound",
            Self::TooManyChunks => "terminal image transmission has too many chunks",
            Self::InvalidImage => "invalid or truncated terminal image container",
            Self::UnsupportedFormat => "unsupported terminal image format",
            Self::UnsupportedAnimation => "animated terminal images are unsupported",
            Self::InvalidDimensions => "invalid terminal image dimensions",
            Self::DimensionsTooLarge => "terminal image dimensions exceed their bound",
            Self::PixelCountTooLarge => "terminal image pixel count exceeds its bound",
            Self::MetadataTooLarge => "terminal image metadata exceeds its bound",
            Self::UnsafeFilename => "unsafe terminal image filename",
            Self::MetadataDimensionMismatch => "terminal image metadata dimensions do not match",
            Self::InvalidLayout => "invalid terminal image cell layout",
            Self::AllocationFailed => "bounded terminal image allocation failed",
            Self::InvalidImageId => "invalid terminal image ID",
            Self::ImageIdExhausted => "terminal image IDs are exhausted",
            Self::TooManyLiveImages => "too many live terminal images",
            Self::StaleImageId => "stale terminal image ID",
            Self::UnsupportedFormatForProtocol => {
                "terminal image format is unsupported by the selected protocol"
            }
            Self::UnsupportedOperation => {
                "terminal image operation is unsupported by the selected protocol"
            }
            Self::InvalidAction => "invalid terminal image action",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ImageError {}

/// A direct image container format accepted by the validator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    /// Portable Network Graphics.
    Png,
    /// Joint Photographic Experts Group image data.
    Jpeg,
    /// Graphics Interchange Format image data.
    Gif,
    /// RIFF/WebP image data.
    Webp,
}

impl ImageFormat {
    /// Detect a leading container marker without accepting the container as
    /// valid. Use [`TerminalImage::from_bytes`] for full bounded validation.
    pub fn detect(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xff, 0xd8]) {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }

    /// Stable ASCII name suitable for a fallback row.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::Webp => "WebP",
        }
    }
}

/// Validated image dimensions in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageDimensions {
    width: u32,
    height: u32,
}

impl ImageDimensions {
    /// Construct nonzero dimensions. Image payload validation additionally
    /// applies the configured width, height, and pixel-count limits.
    pub const fn new(width: u32, height: u32) -> Result<Self, ImageError> {
        if width == 0 || height == 0 {
            Err(ImageError::InvalidDimensions)
        } else {
            Ok(Self { width, height })
        }
    }

    /// Width in pixels.
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub const fn height(self) -> u32 {
        self.height
    }
}

/// A deliberately narrow, display-safe image filename.
///
/// Filenames are metadata only; this type never reads a path. It permits ASCII
/// letters, digits, `.`, `_`, and `-`, which can be base64 encoded safely for
/// iTerm2 without preserving attacker-controlled control characters.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ImageFilename(String);

impl ImageFilename {
    /// Validate a metadata filename against the fixed hard bound.
    pub fn new(value: &str) -> Result<Self, ImageError> {
        if value.is_empty()
            || matches!(value, "." | "..")
            || value.len() > HARD_MAX_FILENAME_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(ImageError::UnsafeFilename);
        }
        Ok(Self(value.to_owned()))
    }

    /// Return the already validated filename.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Optional, validated metadata supplied with a byte payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageMetadata {
    filename: Option<ImageFilename>,
    expected_dimensions: Option<ImageDimensions>,
}

impl ImageMetadata {
    /// Attach a validated filename. The selected [`ImageLimits`] applies a
    /// second, potentially smaller, bound when bytes are accepted.
    pub fn with_filename(mut self, filename: ImageFilename) -> Self {
        self.filename = Some(filename);
        self
    }

    /// Require the source header to match a caller-known dimension pair.
    pub fn with_expected_dimensions(mut self, dimensions: ImageDimensions) -> Self {
        self.expected_dimensions = Some(dimensions);
        self
    }

    /// The optional validated filename.
    pub fn filename(&self) -> Option<&ImageFilename> {
        self.filename.as_ref()
    }

    /// The optional caller-provided dimensions.
    pub const fn expected_dimensions(&self) -> Option<ImageDimensions> {
        self.expected_dimensions
    }
}

/// All adjustable image bounds.
///
/// Every builder rejects values outside non-bypassable hard ceilings. Defaults
/// favor small in-memory terminal attachments; callers may lower limits for a
/// tighter boundary but cannot raise them past the documented hard caps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageLimits {
    max_payload_bytes: usize,
    max_encoded_output_bytes: usize,
    max_protocol_chunk_bytes: usize,
    max_protocol_chunks: usize,
    max_width: u32,
    max_height: u32,
    max_pixels: u64,
    max_container_items: usize,
    max_header_bytes: usize,
    max_filename_bytes: usize,
    max_terminal_reply_bytes: usize,
    query_timeout: Duration,
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self {
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            max_encoded_output_bytes: DEFAULT_MAX_ENCODED_OUTPUT_BYTES,
            max_protocol_chunk_bytes: HARD_MAX_PROTOCOL_CHUNK_BYTES,
            max_protocol_chunks: DEFAULT_MAX_PROTOCOL_CHUNKS,
            max_width: DEFAULT_MAX_IMAGE_DIMENSION,
            max_height: DEFAULT_MAX_IMAGE_DIMENSION,
            max_pixels: DEFAULT_MAX_IMAGE_PIXELS,
            max_container_items: DEFAULT_MAX_CONTAINER_ITEMS,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            max_filename_bytes: DEFAULT_MAX_FILENAME_BYTES,
            max_terminal_reply_bytes: DEFAULT_MAX_TERMINAL_REPLY_BYTES,
            query_timeout: DEFAULT_QUERY_TIMEOUT,
        }
    }
}

impl ImageLimits {
    /// Maximum accepted source bytes.
    pub const fn max_payload_bytes(&self) -> usize {
        self.max_payload_bytes
    }

    /// Maximum complete encoded protocol output bytes.
    pub const fn max_encoded_output_bytes(&self) -> usize {
        self.max_encoded_output_bytes
    }

    /// Maximum base64 bytes in each bounded emission buffer.
    pub const fn max_protocol_chunk_bytes(&self) -> usize {
        self.max_protocol_chunk_bytes
    }

    /// Maximum Kitty chunks in one transmission.
    pub const fn max_protocol_chunks(&self) -> usize {
        self.max_protocol_chunks
    }

    /// Maximum accepted source width.
    pub const fn max_width(&self) -> u32 {
        self.max_width
    }

    /// Maximum accepted source height.
    pub const fn max_height(&self) -> u32 {
        self.max_height
    }

    /// Maximum accepted source pixel count.
    pub const fn max_pixels(&self) -> u64 {
        self.max_pixels
    }

    /// Maximum container records or sub-blocks examined by the validators.
    pub const fn max_container_items(&self) -> usize {
        self.max_container_items
    }

    /// Maximum JPEG header bytes examined before scan data.
    pub const fn max_header_bytes(&self) -> usize {
        self.max_header_bytes
    }

    /// Maximum filename bytes accepted after filename validation.
    pub const fn max_filename_bytes(&self) -> usize {
        self.max_filename_bytes
    }

    /// Maximum terminal reply bytes accepted by the strict reply parser.
    pub const fn max_terminal_reply_bytes(&self) -> usize {
        self.max_terminal_reply_bytes
    }

    /// Caller-owned deadline recommended for one nonblocking query attempt.
    pub const fn query_timeout(&self) -> Duration {
        self.query_timeout
    }

    /// Set a source payload bound.
    pub fn with_max_payload_bytes(mut self, value: usize) -> Result<Self, ImageError> {
        if value == 0 || value > HARD_MAX_IMAGE_PAYLOAD_BYTES {
            return Err(ImageError::InvalidLimit);
        }
        self.max_payload_bytes = value;
        Ok(self)
    }

    /// Set a complete protocol output bound.
    pub fn with_max_encoded_output_bytes(mut self, value: usize) -> Result<Self, ImageError> {
        if value == 0 || value > HARD_MAX_ENCODED_OUTPUT_BYTES {
            return Err(ImageError::InvalidLimit);
        }
        self.max_encoded_output_bytes = value;
        Ok(self)
    }

    /// Set an emission chunk bound. It must be a nonzero base64 quartet count.
    pub fn with_max_protocol_chunk_bytes(mut self, value: usize) -> Result<Self, ImageError> {
        if !(4..=HARD_MAX_PROTOCOL_CHUNK_BYTES).contains(&value) || value % 4 != 0 {
            return Err(ImageError::InvalidLimit);
        }
        self.max_protocol_chunk_bytes = value;
        Ok(self)
    }

    /// Set a Kitty chunk-count bound.
    pub fn with_max_protocol_chunks(mut self, value: usize) -> Result<Self, ImageError> {
        if value == 0 || value > HARD_MAX_PROTOCOL_CHUNKS {
            return Err(ImageError::InvalidLimit);
        }
        self.max_protocol_chunks = value;
        Ok(self)
    }

    /// Set width and height bounds together.
    pub fn with_max_dimensions(mut self, width: u32, height: u32) -> Result<Self, ImageError> {
        if width == 0
            || height == 0
            || width > HARD_MAX_IMAGE_DIMENSION
            || height > HARD_MAX_IMAGE_DIMENSION
        {
            return Err(ImageError::InvalidLimit);
        }
        self.max_width = width;
        self.max_height = height;
        Ok(self)
    }

    /// Set a pixel-count bound.
    pub fn with_max_pixels(mut self, value: u64) -> Result<Self, ImageError> {
        if value == 0 || value > HARD_MAX_IMAGE_PIXELS {
            return Err(ImageError::InvalidLimit);
        }
        self.max_pixels = value;
        Ok(self)
    }

    /// Set a container item bound.
    pub fn with_max_container_items(mut self, value: usize) -> Result<Self, ImageError> {
        if value == 0 || value > HARD_MAX_CONTAINER_ITEMS {
            return Err(ImageError::InvalidLimit);
        }
        self.max_container_items = value;
        Ok(self)
    }

    /// Set the bounded JPEG header scan length.
    pub fn with_max_header_bytes(mut self, value: usize) -> Result<Self, ImageError> {
        if value == 0 || value > HARD_MAX_HEADER_BYTES {
            return Err(ImageError::InvalidLimit);
        }
        self.max_header_bytes = value;
        Ok(self)
    }

    /// Set a smaller accepted filename length.
    pub fn with_max_filename_bytes(mut self, value: usize) -> Result<Self, ImageError> {
        if value > HARD_MAX_FILENAME_BYTES {
            return Err(ImageError::InvalidLimit);
        }
        self.max_filename_bytes = value;
        Ok(self)
    }

    /// Set the terminal reply parser bound.
    pub fn with_max_terminal_reply_bytes(mut self, value: usize) -> Result<Self, ImageError> {
        if value == 0 || value > HARD_MAX_TERMINAL_REPLY_BYTES {
            return Err(ImageError::InvalidLimit);
        }
        self.max_terminal_reply_bytes = value;
        Ok(self)
    }

    /// Set the caller-owned terminal query deadline.
    pub fn with_query_timeout(mut self, value: Duration) -> Result<Self, ImageError> {
        if value.is_zero() || value > HARD_MAX_QUERY_TIMEOUT {
            return Err(ImageError::InvalidLimit);
        }
        self.query_timeout = value;
        Ok(self)
    }
}

/// A validated, owned terminal image whose payload is intentionally private.
///
/// The type has no path or URL constructor. Validation occurs before any copy
/// from a borrowed slice, and its `Debug` representation includes only safe
/// format, dimensions, and byte-count summaries.
pub struct TerminalImage {
    bytes: Vec<u8>,
    format: ImageFormat,
    dimensions: ImageDimensions,
    metadata: ImageMetadata,
}

impl fmt::Debug for TerminalImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TerminalImage")
            .field("format", &self.format)
            .field("dimensions", &self.dimensions)
            .field("byte_len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

impl TerminalImage {
    /// Validate owned bytes with the default bounded limits.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, ImageError> {
        Self::from_bytes_with_metadata(bytes, ImageMetadata::default(), &ImageLimits::default())
    }

    /// Validate owned bytes and metadata under explicit limits.
    ///
    /// A source `Vec` with excess capacity is copied into one bounded allocation
    /// before retention, so caller-side spare capacity cannot bypass the payload
    /// allocation cap.
    pub fn from_bytes_with_metadata(
        bytes: Vec<u8>,
        metadata: ImageMetadata,
        limits: &ImageLimits,
    ) -> Result<Self, ImageError> {
        let (format, dimensions) = inspect_image(&bytes, &metadata, limits)?;
        let bytes = if bytes.capacity() > limits.max_payload_bytes {
            copy_image_bytes(&bytes, limits.max_payload_bytes)?
        } else {
            bytes
        };
        Ok(Self {
            bytes,
            format,
            dimensions,
            metadata,
        })
    }

    /// Validate a borrowed source before making one bounded owned copy.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, ImageError> {
        Self::from_slice_with_metadata(bytes, ImageMetadata::default(), &ImageLimits::default())
    }

    /// Validate a borrowed source and metadata before making one bounded copy.
    pub fn from_slice_with_metadata(
        bytes: &[u8],
        metadata: ImageMetadata,
        limits: &ImageLimits,
    ) -> Result<Self, ImageError> {
        let (format, dimensions) = inspect_image(bytes, &metadata, limits)?;
        Ok(Self {
            bytes: copy_image_bytes(bytes, limits.max_payload_bytes)?,
            format,
            dimensions,
            metadata,
        })
    }

    /// Validated source format.
    pub const fn format(&self) -> ImageFormat {
        self.format
    }

    /// Validated source dimensions.
    pub const fn dimensions(&self) -> ImageDimensions {
        self.dimensions
    }

    /// Number of owned source bytes.
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    /// Validated metadata. Raw payload bytes remain private.
    pub const fn metadata(&self) -> &ImageMetadata {
        &self.metadata
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

fn copy_image_bytes(bytes: &[u8], max_capacity: usize) -> Result<Vec<u8>, ImageError> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|_| ImageError::AllocationFailed)?;
    if owned.capacity() > max_capacity {
        return Err(ImageError::AllocationFailed);
    }
    owned.extend_from_slice(bytes);
    Ok(owned)
}

/// A stable nonzero terminal image ID.
///
/// [`ImageRegistry`] allocates IDs monotonically and never reuses a retired
/// value, which prevents a delayed delete from targeting a newer image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageId(NonZeroU32);

impl ImageId {
    /// Validate a caller-supplied nonzero image ID.
    pub fn new(value: u32) -> Result<Self, ImageError> {
        NonZeroU32::new(value)
            .map(Self)
            .ok_or(ImageError::InvalidImageId)
    }

    /// Numeric protocol ID.
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// A targetable image lifecycle operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageAction {
    /// Display a newly allocated image ID.
    Place(ImageId),
    /// Replace an existing live image ID.
    Replace(ImageId),
    /// Delete an existing live image ID.
    Delete(ImageId),
}

impl ImageAction {
    /// The target image ID.
    pub const fn id(self) -> ImageId {
        match self {
            Self::Place(id) | Self::Replace(id) | Self::Delete(id) => id,
        }
    }
}

/// Monotonic, stale-delete-resistant image ID bookkeeping.
///
/// The registry tracks at most [`HARD_MAX_LIVE_IMAGES`] logical IDs at once.
/// Callers should encode and emit the returned action in order; a terminal write
/// failure may require a renderer to redraw its surface, but cannot cause a
/// future allocation to reuse an ID.
#[derive(Debug)]
pub struct ImageRegistry {
    next: u32,
    live: BTreeSet<ImageId>,
}

impl Default for ImageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageRegistry {
    /// Construct an empty registry whose first placed ID is one.
    pub const fn new() -> Self {
        Self {
            next: 1,
            live: BTreeSet::new(),
        }
    }

    /// Allocate and mark a new stable image ID live.
    pub fn place(&mut self) -> Result<ImageAction, ImageError> {
        if self.next == 0 {
            return Err(ImageError::ImageIdExhausted);
        }
        if self.live.len() >= HARD_MAX_LIVE_IMAGES {
            return Err(ImageError::TooManyLiveImages);
        }
        let id = ImageId::new(self.next)?;
        // Zero is an exhaustion sentinel, never a reusable ID.
        self.next = self.next.checked_add(1).unwrap_or(0);
        self.live.insert(id);
        Ok(ImageAction::Place(id))
    }

    /// Build a replacement action only for a currently live ID.
    pub fn replace(&self, id: ImageId) -> Result<ImageAction, ImageError> {
        self.live
            .contains(&id)
            .then_some(ImageAction::Replace(id))
            .ok_or(ImageError::StaleImageId)
    }

    /// Retire an ID and build its delete action. Retired values never re-enter
    /// the allocator, so a delayed protocol delete cannot target a later image.
    pub fn delete(&mut self, id: ImageId) -> Result<ImageAction, ImageError> {
        self.live
            .remove(&id)
            .then_some(ImageAction::Delete(id))
            .ok_or(ImageError::StaleImageId)
    }

    /// Whether an ID is still logically live.
    pub fn is_live(&self, id: ImageId) -> bool {
        self.live.contains(&id)
    }
}

/// The directly encoded terminal image protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageProtocol {
    /// Kitty graphics APC commands with bounded direct PNG chunks.
    Kitty,
    /// iTerm2 OSC 1337 inline file commands.
    Iterm2,
}

impl ImageProtocol {
    /// Whether this protocol can accept the container bytes without conversion.
    pub const fn supports_format(self, format: ImageFormat) -> bool {
        match self {
            // Kitty `f=100` is PNG. It is deliberately not used as a generic
            // image decoder for JPEG, GIF, or WebP input.
            Self::Kitty => matches!(format, ImageFormat::Png),
            // iTerm2's documented inline-image compatibility set is kept to
            // PNG/JPEG/GIF. WebP behavior varies with host image frameworks.
            Self::Iterm2 => matches!(
                format,
                ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif
            ),
        }
    }

    /// Whether this protocol has a targetable delete operation.
    pub const fn supports_delete(self) -> bool {
        matches!(self, Self::Kitty)
    }
}

/// Explicit deterministic image capability overrides.
///
/// `force` is intended for caller-managed negotiation and test harnesses. It
/// can bypass environment heuristics, but never enables image output for a
/// plain or noninteractive [`TerminalCapabilities`] profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageCapabilityOverrides {
    /// Force this protocol after an interactive caller has established support.
    pub force: Option<ImageProtocol>,
    /// Disable image output even if terminal heuristics claim support.
    pub disable: bool,
    /// Override a caller-provided cell-pixel measurement for deterministic tests.
    pub cell_pixel_size: Option<CellPixelSize>,
}

/// Image-specific terminal capability state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageCapabilities {
    protocol: Option<ImageProtocol>,
    cell_pixel_size: Option<CellPixelSize>,
    accepts_replies: bool,
    forced: bool,
}

impl ImageCapabilities {
    /// Detect image capability hints from the existing conservative terminal
    /// profile. This method sends no terminal query and performs no I/O.
    pub fn detect(terminal: &TerminalCapabilities, overrides: &ImageCapabilityOverrides) -> Self {
        let interactive = terminal.interactive && !terminal.plain;
        let forced = interactive && !overrides.disable && overrides.force.is_some();
        let protocol = if !interactive || overrides.disable {
            None
        } else if let Some(protocol) = overrides.force {
            Some(protocol)
        } else if terminal.kitty_graphics {
            // Keep a deterministic preference if a terminal reports both.
            Some(ImageProtocol::Kitty)
        } else if terminal.iterm2_images {
            Some(ImageProtocol::Iterm2)
        } else {
            None
        };
        Self {
            protocol,
            cell_pixel_size: if interactive {
                overrides.cell_pixel_size.or(terminal.cell_pixel_size)
            } else {
                None
            },
            accepts_replies: interactive && !overrides.disable,
            forced,
        }
    }

    /// Build deterministic capability state for a test harness or a caller
    /// that has already performed its own bounded negotiation.
    pub const fn forced(
        protocol: Option<ImageProtocol>,
        cell_pixel_size: Option<CellPixelSize>,
    ) -> Self {
        Self {
            protocol,
            cell_pixel_size,
            accepts_replies: protocol.is_some(),
            forced: protocol.is_some(),
        }
    }

    /// Selected protocol, if terminal images are usable.
    pub const fn protocol(self) -> Option<ImageProtocol> {
        self.protocol
    }

    /// Validated cell-pixel measurement, if a caller supplied or parsed one.
    pub const fn cell_pixel_size(self) -> Option<CellPixelSize> {
        self.cell_pixel_size
    }

    /// Apply one already-correlated, bounded terminal reply.
    ///
    /// A forced selection is never changed by a late reply. This method does
    /// not accept raw terminal text; use [`parse_terminal_image_reply`] first.
    pub fn apply_reply(&mut self, reply: TerminalImageReply) {
        match reply {
            TerminalImageReply::KittyGraphicsSupported { .. }
                if self.accepts_replies && !self.forced =>
            {
                self.protocol = Some(ImageProtocol::Kitty);
            }
            TerminalImageReply::CellPixels(size) | TerminalImageReply::Iterm2CellPixels(size)
                if self.accepts_replies =>
            {
                self.cell_pixel_size = Some(size);
            }
            _ => {}
        }
    }
}

/// A bounded, caller-owned terminal image capability query.
///
/// Query construction and reply parsing are deliberately separate from the
/// terminal lifecycle. This type never reads from a terminal or sleeps: callers
/// must perform at most one bounded poll using [`Self::timeout`] and then feed
/// exactly one reply to [`Self::parse_reply`].
#[derive(Clone)]
pub struct ImageCapabilityQuery {
    bytes: String,
    kind: ImageQueryKind,
    expected_kitty_query: Option<ImageId>,
    timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageQueryKind {
    KittyGraphics,
    CellPixels,
    Iterm2CellPixels,
}

impl fmt::Debug for ImageCapabilityQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageCapabilityQuery")
            .field("expected_kitty_query", &self.expected_kitty_query)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl ImageCapabilityQuery {
    /// Build a Kitty graphics support query correlated to a nonzero image ID.
    pub fn kitty_graphics(id: ImageId, limits: &ImageLimits) -> Self {
        Self {
            // Do not set Kitty's quiet mode here: this is the one command for
            // which the caller needs the correlated success reply.
            bytes: format!("\x1b_Ga=q,i={},s=1,v=1,f=24;\x1b\\", id.get()),
            kind: ImageQueryKind::KittyGraphics,
            expected_kitty_query: Some(id),
            timeout: limits.query_timeout,
        }
    }

    /// Build the standard xterm cell-pixel query (`CSI 16 t`).
    pub fn cell_pixels(limits: &ImageLimits) -> Self {
        Self {
            bytes: "\x1b[16t".to_owned(),
            kind: ImageQueryKind::CellPixels,
            expected_kitty_query: None,
            timeout: limits.query_timeout,
        }
    }

    /// Build iTerm2's cell-size query. The corresponding parser accepts both
    /// BEL and ST terminated replies, but no trailing data.
    pub fn iterm2_cell_pixels(limits: &ImageLimits) -> Self {
        Self {
            bytes: "\x1b]1337;ReportCellSize\x1b\\".to_owned(),
            kind: ImageQueryKind::Iterm2CellPixels,
            expected_kitty_query: None,
            timeout: limits.query_timeout,
        }
    }

    /// Maximum caller-owned wait for this one query attempt.
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Expected Kitty query ID, used to reject confused replies.
    pub const fn expected_kitty_query(&self) -> Option<ImageId> {
        self.expected_kitty_query
    }

    /// Parse exactly one reply for this specific query kind.
    ///
    /// Unlike [`parse_terminal_image_reply`], this method rejects a syntactically
    /// valid reply for another query type. It is the preferred correlation
    /// boundary after the caller-owned one-shot poll.
    pub fn parse_reply(&self, reply: &str, limits: &ImageLimits) -> Option<TerminalImageReply> {
        if reply.is_empty() || reply.len() > limits.max_terminal_reply_bytes {
            return None;
        }
        match self.kind {
            ImageQueryKind::KittyGraphics => {
                let expected = self.expected_kitty_query?;
                let id = parse_kitty_reply(reply)?;
                (id == expected)
                    .then_some(TerminalImageReply::KittyGraphicsSupported { query_id: id })
            }
            ImageQueryKind::CellPixels => {
                parse_standard_cell_reply(reply).map(TerminalImageReply::CellPixels)
            }
            ImageQueryKind::Iterm2CellPixels => {
                parse_iterm2_cell_reply(reply).map(TerminalImageReply::Iterm2CellPixels)
            }
        }
    }

    /// Write query bytes to a generic output without exposing them as semantic
    /// text. The query is fixed-size and contains no payload data.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(self.bytes.as_bytes())
    }

    /// Emit the bounded query through a terminal output sink.
    pub fn emit_to_terminal(&self, terminal: &mut dyn Terminal) {
        terminal.write(&self.bytes);
    }
}

/// A strictly parsed terminal image capability reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalImageReply {
    /// A Kitty graphics query completed successfully for this exact ID.
    KittyGraphicsSupported {
        /// The query ID supplied by the caller-owned query.
        query_id: ImageId,
    },
    /// A standard `CSI 6 ; height ; width t` cell-pixel report.
    CellPixels(CellPixelSize),
    /// An iTerm2 `ReportCellSize=height;width` cell-pixel report.
    Iterm2CellPixels(CellPixelSize),
}

/// Parse exactly one bounded terminal reply.
///
/// The parser accepts no prefixes, suffixes, concatenated frames, unknown
/// fields, or error text. Kitty success is accepted only when `expected_kitty`
/// matches the reply ID; this is the correlation boundary that prevents an old
/// or unrelated reply from enabling graphics support. Prefer
/// [`ImageCapabilityQuery::parse_reply`] when the originating query is known,
/// because it additionally rejects valid replies for another query type. This
/// function performs no I/O and never returns hostile text for logging.
pub fn parse_terminal_image_reply(
    reply: &str,
    expected_kitty: Option<ImageId>,
    limits: &ImageLimits,
) -> Option<TerminalImageReply> {
    if reply.is_empty() || reply.len() > limits.max_terminal_reply_bytes {
        return None;
    }

    if let Some(expected) = expected_kitty {
        if let Some(id) = parse_kitty_reply(reply) {
            return (id == expected)
                .then_some(TerminalImageReply::KittyGraphicsSupported { query_id: id });
        }
    }

    parse_standard_cell_reply(reply)
        .map(TerminalImageReply::CellPixels)
        .or_else(|| parse_iterm2_cell_reply(reply).map(TerminalImageReply::Iterm2CellPixels))
}

/// Terminal geometry used to derive a bounded image cell placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageViewport {
    columns: u16,
    rows: u16,
    cell_pixel_size: Option<CellPixelSize>,
}

impl ImageViewport {
    /// Build a viewport from terminal cell dimensions and an optional validated
    /// cell-pixel report.
    pub fn new(
        columns: u16,
        rows: u16,
        cell_pixel_size: Option<CellPixelSize>,
    ) -> Result<Self, ImageError> {
        if columns == 0 || rows == 0 {
            return Err(ImageError::InvalidLayout);
        }
        Ok(Self {
            columns,
            rows,
            cell_pixel_size,
        })
    }

    /// Build a viewport while carrying the cell measurement from terminal
    /// image capabilities.
    pub fn with_capabilities(
        columns: u16,
        rows: u16,
        capabilities: ImageCapabilities,
    ) -> Result<Self, ImageError> {
        Self::new(columns, rows, capabilities.cell_pixel_size())
    }

    /// Available character-cell columns.
    pub const fn columns(self) -> u16 {
        self.columns
    }

    /// Available character-cell rows.
    pub const fn rows(self) -> u16 {
        self.rows
    }

    /// Optional pixel size of one character cell.
    pub const fn cell_pixel_size(self) -> Option<CellPixelSize> {
        self.cell_pixel_size
    }
}

/// A bounded terminal image placement in character cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageLayout {
    columns: u16,
    rows: u16,
}

impl ImageLayout {
    /// Validate an explicit placement. Explicit layouts remain bounded so a
    /// semantic reservation never allocates an unbounded number of rows.
    pub fn new(columns: u16, rows: u16) -> Result<Self, ImageError> {
        if columns == 0
            || rows == 0
            || columns > MAX_IMAGE_CELL_COLUMNS
            || rows > MAX_RESERVED_IMAGE_ROWS
        {
            return Err(ImageError::InvalidLayout);
        }
        Ok(Self { columns, rows })
    }

    /// Fit dimensions into a viewport without upscaling.
    ///
    /// When a cell-pixel report exists, the calculation uses checked wide
    /// integer arithmetic, preserves aspect ratio, and caps both axes to the
    /// viewport and semantic reservation limits. Without that report, the
    /// only non-speculative safe placement is one cell by one cell.
    pub fn fit(dimensions: ImageDimensions, viewport: ImageViewport) -> Result<Self, ImageError> {
        let max_columns = viewport.columns.min(MAX_IMAGE_CELL_COLUMNS);
        let max_rows = viewport.rows.min(MAX_RESERVED_IMAGE_ROWS);
        if max_columns == 0 || max_rows == 0 {
            return Err(ImageError::InvalidLayout);
        }
        let Some(cell) = viewport.cell_pixel_size else {
            return Self::new(1, 1);
        };

        let image_width = u64::from(dimensions.width);
        let image_height = u64::from(dimensions.height);
        let cell_width = u64::from(cell.width());
        let cell_height = u64::from(cell.height());
        let max_width = u64::from(max_columns)
            .checked_mul(cell_width)
            .ok_or(ImageError::InvalidLayout)?;
        let max_height = u64::from(max_rows)
            .checked_mul(cell_height)
            .ok_or(ImageError::InvalidLayout)?;

        let (target_width, target_height) =
            if image_width <= max_width && image_height <= max_height {
                (image_width, image_height)
            } else if u128::from(max_width) * u128::from(image_height)
                <= u128::from(max_height) * u128::from(image_width)
            {
                let height = ceil_div_u128(
                    u128::from(image_height) * u128::from(max_width),
                    u128::from(image_width),
                );
                (
                    max_width,
                    u64::try_from(height).map_err(|_| ImageError::InvalidLayout)?,
                )
            } else {
                let width = ceil_div_u128(
                    u128::from(image_width) * u128::from(max_height),
                    u128::from(image_height),
                );
                (
                    u64::try_from(width).map_err(|_| ImageError::InvalidLayout)?,
                    max_height,
                )
            };

        let columns = ceil_div_u64(target_width, cell_width).min(u64::from(max_columns));
        let rows = ceil_div_u64(target_height, cell_height).min(u64::from(max_rows));
        Self::new(
            u16::try_from(columns).map_err(|_| ImageError::InvalidLayout)?,
            u16::try_from(rows).map_err(|_| ImageError::InvalidLayout)?,
        )
    }

    /// Placement width in cells.
    pub const fn columns(self) -> u16 {
        self.columns
    }

    /// Reserved semantic rows and placement height in cells.
    pub const fn rows(self) -> u16 {
        self.rows
    }
}

/// Compute a safe number of terminal rows for a pixel height.
///
/// If no cell-pixel report is available, this returns one rather than guessing
/// a terminal aspect ratio. A result too large for a terminal-cell reservation
/// is an error, never a wrapping arithmetic result.
pub fn cell_rows_for_pixels(
    image_height: u32,
    cell_pixel_size: Option<CellPixelSize>,
) -> Result<u16, ImageError> {
    if image_height == 0 {
        return Err(ImageError::InvalidDimensions);
    }
    let Some(cell) = cell_pixel_size else {
        return Ok(1);
    };
    let rows = ceil_div_u64(u64::from(image_height), u64::from(cell.height()));
    let rows = u16::try_from(rows).map_err(|_| ImageError::InvalidLayout)?;
    if rows == 0 || rows > MAX_RESERVED_IMAGE_ROWS {
        return Err(ImageError::InvalidLayout);
    }
    Ok(rows)
}

/// Why a plan contains semantic fallback text instead of protocol output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFallbackReason {
    /// No interactive terminal image protocol is available.
    UnsupportedTerminal,
    /// The selected terminal protocol does not accept this source format.
    UnsupportedFormat,
    /// The selected protocol cannot safely replace the target image.
    UnsupportedOperation,
}

/// A semantic-only image reservation.
///
/// The type stores only bounded blank rows or generated ASCII fallback text. It
/// never stores protocol sequences, base64, payload bytes, or caller filenames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageReservation {
    rows: u16,
    fallback: Option<String>,
}

impl ImageReservation {
    fn blank(layout: ImageLayout) -> Self {
        Self {
            rows: layout.rows,
            fallback: None,
        }
    }

    fn fallback(image: &TerminalImage, reason: ImageFallbackReason) -> Self {
        let reason = match reason {
            ImageFallbackReason::UnsupportedTerminal => "unsupported terminal",
            ImageFallbackReason::UnsupportedFormat => "unsupported format",
            ImageFallbackReason::UnsupportedOperation => "unsupported operation",
        };
        let dimensions = image.dimensions();
        Self {
            rows: 1,
            fallback: Some(format!(
                "[image: {} {}x{} ({reason})]",
                image.format().name(),
                dimensions.width(),
                dimensions.height(),
            )),
        }
    }

    /// Number of semantic rows reserved for this image or fallback.
    pub const fn rows(&self) -> u16 {
        self.rows
    }

    /// Generate semantic frame rows. These rows are safe to select, copy, log,
    /// and send through a text renderer; they cannot contain protocol bytes.
    pub fn semantic_rows(&self) -> Vec<String> {
        let mut rows = Vec::with_capacity(usize::from(self.rows));
        if let Some(fallback) = &self.fallback {
            rows.push(fallback.clone());
        } else {
            rows.resize(usize::from(self.rows), String::new());
        }
        rows
    }

    /// Generate the copy/log representation of the reservation.
    pub fn semantic_copy_text(&self) -> String {
        self.semantic_rows().join("\n")
    }

    /// Whether this reservation contains deterministic fallback text.
    pub const fn is_fallback(&self) -> bool {
        self.fallback.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandKind {
    Place,
    Replace,
    Delete,
}

impl CommandKind {
    const fn action(self, id: ImageId) -> ImageAction {
        match self {
            Self::Place => ImageAction::Place(id),
            Self::Replace => ImageAction::Replace(id),
            Self::Delete => ImageAction::Delete(id),
        }
    }
}

struct ImageTransmission<'a> {
    image: &'a TerminalImage,
    protocol: ImageProtocol,
    chunk_bytes: usize,
    chunks: usize,
    first_header: String,
    continuation_more: String,
    continuation_last: String,
}

/// An opaque, bounded terminal image command.
///
/// Use [`Self::write_to`] or [`Self::emit_to_terminal`] to send it. The command
/// has no method returning raw protocol text, which keeps callers from
/// accidentally placing escape sequences in semantic rows or diagnostics.
pub struct ImageTerminalCommand<'a> {
    protocol: ImageProtocol,
    kind: CommandKind,
    id: ImageId,
    transmission: Option<ImageTransmission<'a>>,
    delete_prefix: Option<String>,
    encoded_len: usize,
    payload_chunks: usize,
}

impl fmt::Debug for ImageTerminalCommand<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageTerminalCommand")
            .field("protocol", &self.protocol)
            .field("action", &self.kind.action(self.id))
            .field("encoded_len", &self.encoded_len)
            .field("payload_chunks", &self.payload_chunks)
            .finish()
    }
}

impl<'a> ImageTerminalCommand<'a> {
    /// Protocol selected for this command.
    pub const fn protocol(&self) -> ImageProtocol {
        self.protocol
    }

    /// Explicit lifecycle action represented by this command.
    pub const fn action(&self) -> ImageAction {
        self.kind.action(self.id)
    }

    /// Complete bounded output length, including headers and terminators.
    pub const fn encoded_len(&self) -> usize {
        self.encoded_len
    }

    /// Number of bounded base64 portions written. Kitty terminates each part;
    /// iTerm2 writes the same portions inside one OSC frame.
    pub const fn payload_chunks(&self) -> usize {
        self.payload_chunks
    }

    /// Stream the opaque command to any byte writer using fixed-size base64
    /// buffers. No output-sized allocation occurs here.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        self.visit_bytes(|bytes| writer.write_all(bytes))
    }

    /// Stream the opaque command through the terminal output channel. This is
    /// intentionally separate from [`ImageRenderPlan::semantic_rows`].
    pub fn emit_to_terminal(&self, terminal: &mut dyn Terminal) {
        let _: Result<(), std::convert::Infallible> = self.visit_bytes(|bytes| {
            // Headers, base64 chunks, and terminators are generated ASCII. If
            // an internal invariant is ever broken, suppress bytes rather than
            // forwarding an unexpected terminal control sequence.
            if let Ok(text) = std::str::from_utf8(bytes) {
                terminal.write(text);
            }
            Ok(())
        });
    }

    fn visit_bytes<E, F>(&self, mut write: F) -> Result<(), E>
    where
        F: FnMut(&[u8]) -> Result<(), E>,
    {
        if let Some(delete) = &self.delete_prefix {
            write(delete.as_bytes())?;
        }
        if let Some(transmission) = &self.transmission {
            emit_transmission(transmission, &mut write)?;
        }
        Ok(())
    }
}

/// A protocol encoder that keeps payload output opaque and bounded.
#[derive(Clone, Debug)]
pub struct ImageProtocolEncoder {
    protocol: ImageProtocol,
    limits: ImageLimits,
}

impl ImageProtocolEncoder {
    /// Construct an encoder for a directly supported protocol.
    pub fn new(protocol: ImageProtocol, limits: ImageLimits) -> Self {
        Self { protocol, limits }
    }

    /// Selected protocol.
    pub const fn protocol(&self) -> ImageProtocol {
        self.protocol
    }

    /// Encode a new image placement.
    pub fn encode_place<'a>(
        &self,
        id: ImageId,
        image: &'a TerminalImage,
        layout: ImageLayout,
    ) -> Result<ImageTerminalCommand<'a>, ImageError> {
        self.encode_transmission(CommandKind::Place, id, image, layout)
    }

    /// Encode a replacement. Kitty emits a targeted delete immediately before
    /// transmission; iTerm2 returns [`ImageError::UnsupportedOperation`] rather
    /// than pretending that an unaddressable OSC image was replaced.
    pub fn encode_replace<'a>(
        &self,
        id: ImageId,
        image: &'a TerminalImage,
        layout: ImageLayout,
    ) -> Result<ImageTerminalCommand<'a>, ImageError> {
        if !self.protocol.supports_delete() {
            return Err(ImageError::UnsupportedOperation);
        }
        self.encode_transmission(CommandKind::Replace, id, image, layout)
    }

    /// Encode a targeted delete. iTerm2 has no equivalent targetable command
    /// and therefore returns [`ImageError::UnsupportedOperation`].
    pub fn encode_delete(&self, id: ImageId) -> Result<ImageTerminalCommand<'static>, ImageError> {
        if !self.protocol.supports_delete() {
            return Err(ImageError::UnsupportedOperation);
        }
        let delete = kitty_delete_sequence(id);
        if delete.len() > self.limits.max_encoded_output_bytes {
            return Err(ImageError::EncodedOutputTooLarge);
        }
        Ok(ImageTerminalCommand {
            protocol: self.protocol,
            kind: CommandKind::Delete,
            id,
            transmission: None,
            encoded_len: delete.len(),
            delete_prefix: Some(delete),
            payload_chunks: 0,
        })
    }

    fn encode_transmission<'a>(
        &self,
        kind: CommandKind,
        id: ImageId,
        image: &'a TerminalImage,
        layout: ImageLayout,
    ) -> Result<ImageTerminalCommand<'a>, ImageError> {
        validate_existing_image(image, &self.limits)?;
        if !self.protocol.supports_format(image.format()) {
            return Err(ImageError::UnsupportedFormatForProtocol);
        }
        let (transmission, output_len, payload_chunks) =
            build_transmission(self.protocol, id, image, layout, &self.limits)?;
        let delete_prefix = (kind == CommandKind::Replace).then(|| kitty_delete_sequence(id));
        let encoded_len = delete_prefix.as_ref().map_or(Ok(output_len), |delete| {
            checked_add(delete.len(), output_len)
        })?;
        if encoded_len > self.limits.max_encoded_output_bytes {
            return Err(ImageError::EncodedOutputTooLarge);
        }
        Ok(ImageTerminalCommand {
            protocol: self.protocol,
            kind,
            id,
            transmission: Some(transmission),
            delete_prefix,
            encoded_len,
            payload_chunks,
        })
    }
}

/// A private zero-width marker that connects a semantic reservation to an
/// opaque terminal-image command at the terminal-adapter boundary.
///
/// The marker is deliberately a DCS control string rather than a graphics
/// protocol command. Retained renderers may keep it alongside blank semantic
/// rows, while an adapter that owns the corresponding image bytes replaces it
/// with a bounded protocol command. It contains no image payload, filename, or
/// source location.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageAnchor {
    protocol: ImageProtocol,
    id: ImageId,
    layout: ImageLayout,
}

const IMAGE_ANCHOR_PREFIX: &str = "\x1bP+ygg-image;";
const IMAGE_ANCHOR_SUFFIX: &str = "\x1b\\";

impl ImageAnchor {
    /// Build an anchor for one already-planned placement.
    pub const fn new(protocol: ImageProtocol, id: ImageId, layout: ImageLayout) -> Self {
        Self {
            protocol,
            id,
            layout,
        }
    }

    /// Protocol selected for this placement.
    pub const fn protocol(self) -> ImageProtocol {
        self.protocol
    }

    /// Stable logical image ID.
    pub const fn id(self) -> ImageId {
        self.id
    }

    /// Bounded terminal-cell placement.
    pub const fn layout(self) -> ImageLayout {
        self.layout
    }

    /// Render the zero-width adapter marker. This is not a terminal graphics
    /// command and must not be emitted without an adapter that resolves it.
    pub fn marker(self) -> String {
        let protocol = match self.protocol {
            ImageProtocol::Kitty => "kitty",
            ImageProtocol::Iterm2 => "iterm2",
        };
        format!(
            "{IMAGE_ANCHOR_PREFIX}v=1,p={protocol},i={},c={},r={}{}",
            self.id.get(),
            self.layout.columns(),
            self.layout.rows(),
            IMAGE_ANCHOR_SUFFIX,
        )
    }

    /// Strictly parse one complete anchor marker. Unknown versions, fields,
    /// protocols, invalid IDs, and invalid layouts are rejected.
    pub fn parse(marker: &str) -> Option<Self> {
        let body = marker
            .strip_prefix(IMAGE_ANCHOR_PREFIX)?
            .strip_suffix(IMAGE_ANCHOR_SUFFIX)?;
        let mut fields = body.split(',');
        if fields.next()? != "v=1" {
            return None;
        }
        let protocol = match fields.next()?.strip_prefix("p=")? {
            "kitty" => ImageProtocol::Kitty,
            "iterm2" => ImageProtocol::Iterm2,
            _ => return None,
        };
        let id = ImageId::new(fields.next()?.strip_prefix("i=")?.parse().ok()?).ok()?;
        let columns = fields.next()?.strip_prefix("c=")?.parse().ok()?;
        let rows = fields.next()?.strip_prefix("r=")?.parse().ok()?;
        if fields.next().is_some() {
            return None;
        }
        Some(Self::new(
            protocol,
            id,
            ImageLayout::new(columns, rows).ok()?,
        ))
    }

    /// Find every complete, bounded anchor in a rendered line. At most the
    /// global live-image limit is returned, so hostile text cannot create an
    /// unbounded parse result.
    pub fn parse_all(line: &str) -> Vec<Self> {
        let mut anchors = Vec::new();
        let mut search_from = 0;
        while anchors.len() < HARD_MAX_LIVE_IMAGES {
            let Some(relative_start) = line[search_from..].find(IMAGE_ANCHOR_PREFIX) else {
                break;
            };
            let start = search_from.saturating_add(relative_start);
            let body_start = start.saturating_add(IMAGE_ANCHOR_PREFIX.len());
            let Some(relative_end) = line[body_start..].find(IMAGE_ANCHOR_SUFFIX) else {
                break;
            };
            let end = body_start
                .saturating_add(relative_end)
                .saturating_add(IMAGE_ANCHOR_SUFFIX.len());
            if let Some(anchor) = Self::parse(&line[start..end]) {
                anchors.push(anchor);
            }
            search_from = end;
        }
        anchors
    }
}

/// A semantic reservation plus optional, out-of-band protocol command.
///
/// This is the intended handoff to a future renderer integration: use semantic
/// rows for its retained frame, then write the command at the placement point.
/// Serialize that emission with normal terminal writes. The foundation does not
/// alter TUI lifecycle or scrollback itself.
pub struct ImageRenderPlan<'a> {
    reservation: ImageReservation,
    command: Option<ImageTerminalCommand<'a>>,
    layout: Option<ImageLayout>,
    fallback_reason: Option<ImageFallbackReason>,
}

impl fmt::Debug for ImageRenderPlan<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageRenderPlan")
            .field("reservation", &self.reservation)
            .field("has_terminal_command", &self.command.is_some())
            .field("layout", &self.layout)
            .field("fallback_reason", &self.fallback_reason)
            .finish()
    }
}

impl<'a> ImageRenderPlan<'a> {
    /// Semantic-only reservation for frame, selection, copy, or logs.
    pub const fn reservation(&self) -> &ImageReservation {
        &self.reservation
    }

    /// Generate semantic rows without protocol or payload bytes.
    pub fn semantic_rows(&self) -> Vec<String> {
        self.reservation.semantic_rows()
    }

    /// Generate the semantic copy/log representation without protocol bytes.
    pub fn semantic_copy_text(&self) -> String {
        self.reservation.semantic_copy_text()
    }

    /// Optional opaque protocol output to write separately from semantic text.
    pub const fn terminal_command(&self) -> Option<&ImageTerminalCommand<'a>> {
        self.command.as_ref()
    }

    /// Placement geometry when a terminal command is available. This lets a
    /// retained renderer create a zero-width [`ImageAnchor`] without exposing
    /// protocol bytes in its semantic frame.
    pub const fn layout(&self) -> Option<ImageLayout> {
        self.layout
    }

    /// Why this plan has no terminal command.
    pub const fn fallback_reason(&self) -> Option<ImageFallbackReason> {
        self.fallback_reason
    }

    /// Write the optional protocol command to a generic byte writer.
    pub fn write_protocol_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        match &self.command {
            Some(command) => command.write_to(writer),
            None => Ok(()),
        }
    }

    /// Emit the optional protocol command through the terminal output channel.
    pub fn emit_protocol_to_terminal(&self, terminal: &mut dyn Terminal) {
        if let Some(command) = &self.command {
            command.emit_to_terminal(terminal);
        }
    }
}

/// Capability-aware protocol and semantic reservation planner.
#[derive(Clone, Debug)]
pub struct ImagePlanner {
    capabilities: ImageCapabilities,
    limits: ImageLimits,
}

impl ImagePlanner {
    /// Construct a planner from already detected or forced capabilities.
    pub fn new(capabilities: ImageCapabilities, limits: ImageLimits) -> Self {
        Self {
            capabilities,
            limits,
        }
    }

    /// Current image capability state.
    pub const fn capabilities(&self) -> ImageCapabilities {
        self.capabilities
    }

    /// Plan a new placement and its semantic reservation.
    pub fn plan_place<'a>(
        &self,
        id: ImageId,
        image: &'a TerminalImage,
        viewport: ImageViewport,
    ) -> Result<ImageRenderPlan<'a>, ImageError> {
        self.plan(CommandKind::Place, id, image, viewport)
    }

    /// Plan a replacement. Kitty yields delete-then-transmit; iTerm2 yields a
    /// semantic fallback because it cannot target an existing inline image.
    pub fn plan_replace<'a>(
        &self,
        id: ImageId,
        image: &'a TerminalImage,
        viewport: ImageViewport,
    ) -> Result<ImageRenderPlan<'a>, ImageError> {
        self.plan(CommandKind::Replace, id, image, viewport)
    }

    /// Plan a targetable delete. `Ok(None)` means no image terminal is active;
    /// iTerm2 returns an explicit unsupported-operation error rather than a
    /// false success.
    pub fn plan_delete(
        &self,
        action: ImageAction,
    ) -> Result<Option<ImageTerminalCommand<'static>>, ImageError> {
        let ImageAction::Delete(id) = action else {
            return Err(ImageError::InvalidAction);
        };
        let Some(protocol) = self.capabilities.protocol() else {
            return Ok(None);
        };
        ImageProtocolEncoder::new(protocol, self.limits.clone())
            .encode_delete(id)
            .map(Some)
    }

    fn plan<'a>(
        &self,
        kind: CommandKind,
        id: ImageId,
        image: &'a TerminalImage,
        viewport: ImageViewport,
    ) -> Result<ImageRenderPlan<'a>, ImageError> {
        let Some(protocol) = self.capabilities.protocol() else {
            return Ok(fallback_plan(
                image,
                ImageFallbackReason::UnsupportedTerminal,
            ));
        };
        if !protocol.supports_format(image.format()) {
            return Ok(fallback_plan(image, ImageFallbackReason::UnsupportedFormat));
        }
        let layout = ImageLayout::fit(image.dimensions(), viewport)?;
        let encoder = ImageProtocolEncoder::new(protocol, self.limits.clone());
        let command = match kind {
            CommandKind::Place => encoder.encode_place(id, image, layout),
            CommandKind::Replace => encoder.encode_replace(id, image, layout),
            CommandKind::Delete => return Err(ImageError::InvalidAction),
        };
        match command {
            Ok(command) => Ok(ImageRenderPlan {
                reservation: ImageReservation::blank(layout),
                command: Some(command),
                layout: Some(layout),
                fallback_reason: None,
            }),
            Err(ImageError::UnsupportedOperation) => Ok(fallback_plan(
                image,
                ImageFallbackReason::UnsupportedOperation,
            )),
            Err(error) => Err(error),
        }
    }
}

fn fallback_plan<'a>(image: &'a TerminalImage, reason: ImageFallbackReason) -> ImageRenderPlan<'a> {
    ImageRenderPlan {
        reservation: ImageReservation::fallback(image, reason),
        command: None,
        layout: None,
        fallback_reason: Some(reason),
    }
}

fn validate_existing_image(image: &TerminalImage, limits: &ImageLimits) -> Result<(), ImageError> {
    // A `TerminalImage` may have been accepted under different caller limits.
    // Re-inspection is bounded and ensures an encoder or planner cannot loosen
    // its own source, metadata, or container-record boundary by borrowing it.
    inspect_image(image.bytes(), image.metadata(), limits).map(|_| ())
}

fn inspect_image(
    bytes: &[u8],
    metadata: &ImageMetadata,
    limits: &ImageLimits,
) -> Result<(ImageFormat, ImageDimensions), ImageError> {
    if bytes.is_empty() {
        return Err(ImageError::InvalidImage);
    }
    if bytes.len() > limits.max_payload_bytes {
        return Err(ImageError::PayloadTooLarge);
    }
    if let Some(filename) = metadata.filename() {
        if filename.as_str().len() > limits.max_filename_bytes {
            return Err(ImageError::UnsafeFilename);
        }
    }
    let format = ImageFormat::detect(bytes).ok_or(ImageError::UnsupportedFormat)?;
    let dimensions = match format {
        ImageFormat::Png => parse_png(bytes, limits)?,
        ImageFormat::Jpeg => parse_jpeg(bytes, limits)?,
        ImageFormat::Gif => parse_gif(bytes, limits)?,
        ImageFormat::Webp => parse_webp(bytes, limits)?,
    };
    validate_dimensions(dimensions, limits)?;
    if metadata
        .expected_dimensions()
        .is_some_and(|expected| expected != dimensions)
    {
        return Err(ImageError::MetadataDimensionMismatch);
    }
    Ok((format, dimensions))
}

fn validate_dimensions(
    dimensions: ImageDimensions,
    limits: &ImageLimits,
) -> Result<(), ImageError> {
    if dimensions.width > limits.max_width || dimensions.height > limits.max_height {
        return Err(ImageError::DimensionsTooLarge);
    }
    let pixels = u64::from(dimensions.width)
        .checked_mul(u64::from(dimensions.height))
        .ok_or(ImageError::PixelCountTooLarge)?;
    if pixels > limits.max_pixels {
        return Err(ImageError::PixelCountTooLarge);
    }
    Ok(())
}

const fn crc32_table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                0xedb8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

const PNG_CRC_TABLE: [u32; 256] = crc32_table();

fn png_crc32(parts: &[&[u8]]) -> u32 {
    let mut value = 0xffff_ffff_u32;
    for part in parts {
        for byte in *part {
            let index = usize::from(((value ^ u32::from(*byte)) & 0xff) as u8);
            value = PNG_CRC_TABLE[index] ^ (value >> 8);
        }
    }
    !value
}

fn parse_png(bytes: &[u8], limits: &ImageLimits) -> Result<ImageDimensions, ImageError> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !bytes.starts_with(SIGNATURE) || bytes.len() < SIGNATURE.len() + 25 {
        return Err(ImageError::InvalidImage);
    }
    let mut offset = SIGNATURE.len();
    let mut items = 0;
    let mut dimensions = None;
    let mut saw_idat = false;
    let mut idat_ended = false;

    loop {
        take_container_item(&mut items, limits)?;
        if bytes.len().saturating_sub(offset) < 12 {
            return Err(ImageError::InvalidImage);
        }
        let length = usize::try_from(read_u32_be(bytes, offset).ok_or(ImageError::InvalidImage)?)
            .map_err(|_| ImageError::InvalidImage)?;
        let data_start = offset.checked_add(8).ok_or(ImageError::InvalidImage)?;
        let crc_start = data_start
            .checked_add(length)
            .ok_or(ImageError::InvalidImage)?;
        let end = crc_start.checked_add(4).ok_or(ImageError::InvalidImage)?;
        if end > bytes.len() {
            return Err(ImageError::InvalidImage);
        }
        let kind = &bytes[offset + 4..data_start];
        let data = &bytes[data_start..crc_start];
        let expected_crc = read_u32_be(bytes, crc_start).ok_or(ImageError::InvalidImage)?;
        if png_crc32(&[kind, data]) != expected_crc {
            return Err(ImageError::InvalidImage);
        }
        if kind == b"acTL" || kind == b"fcTL" || kind == b"fdAT" {
            return Err(ImageError::UnsupportedAnimation);
        }

        if items == 1 {
            if kind != b"IHDR" || length != 13 {
                return Err(ImageError::InvalidImage);
            }
            let parsed = ImageDimensions::new(
                read_u32_be(data, 0).ok_or(ImageError::InvalidImage)?,
                read_u32_be(data, 4).ok_or(ImageError::InvalidImage)?,
            )?;
            if !valid_png_header(data) {
                return Err(ImageError::InvalidImage);
            }
            dimensions = Some(parsed);
        } else if kind == b"IHDR" {
            return Err(ImageError::InvalidImage);
        }

        if kind == b"IDAT" {
            if dimensions.is_none() || idat_ended || length == 0 {
                return Err(ImageError::InvalidImage);
            }
            saw_idat = true;
        } else if saw_idat && kind != b"IEND" {
            // PNG requires all IDAT chunks to be consecutive.
            idat_ended = true;
        }
        if kind == b"IEND" {
            if length != 0 || !saw_idat || end != bytes.len() {
                return Err(ImageError::InvalidImage);
            }
            return dimensions.ok_or(ImageError::InvalidImage);
        }
        offset = end;
    }
}

fn valid_png_header(data: &[u8]) -> bool {
    if data.len() != 13 || data[10] != 0 || data[11] != 0 || data[12] > 1 {
        return false;
    }
    matches!(
        (data[8], data[9]),
        (1 | 2 | 4 | 8 | 16, 0) | (8 | 16, 2 | 4 | 6) | (1 | 2 | 4 | 8, 3)
    )
}

fn parse_jpeg(bytes: &[u8], limits: &ImageLimits) -> Result<ImageDimensions, ImageError> {
    if bytes.len() < 12 || !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return Err(ImageError::InvalidImage);
    }
    let mut offset = 2;
    let mut items = 0;
    let header_limit = bytes.len().min(limits.max_header_bytes);

    while offset < bytes.len().saturating_sub(2) {
        if offset >= header_limit || bytes.get(offset) != Some(&0xff) {
            return Err(ImageError::MetadataTooLarge);
        }
        while bytes.get(offset) == Some(&0xff) {
            offset = offset.checked_add(1).ok_or(ImageError::InvalidImage)?;
            if offset >= header_limit {
                return Err(ImageError::MetadataTooLarge);
            }
        }
        let marker = *bytes.get(offset).ok_or(ImageError::InvalidImage)?;
        offset = offset.checked_add(1).ok_or(ImageError::InvalidImage)?;
        if marker == 0 || marker == 0xff || marker == 0xd8 || marker == 0xd9 {
            return Err(ImageError::InvalidImage);
        }
        take_container_item(&mut items, limits)?;
        if matches!(marker, 0x01 | 0xd0..=0xd7) {
            continue;
        }
        let segment_length =
            usize::from(read_u16_be(bytes, offset).ok_or(ImageError::InvalidImage)?);
        if segment_length < 2 {
            return Err(ImageError::InvalidImage);
        }
        let data_start = offset.checked_add(2).ok_or(ImageError::InvalidImage)?;
        let end = offset
            .checked_add(segment_length)
            .ok_or(ImageError::InvalidImage)?;
        if end > bytes.len() {
            return Err(ImageError::InvalidImage);
        }
        if end > header_limit {
            return Err(ImageError::MetadataTooLarge);
        }
        let data = &bytes[data_start..end];
        if is_jpeg_sof(marker) {
            if data.len() < 6 || data[0] == 0 {
                return Err(ImageError::InvalidImage);
            }
            let components = usize::from(data[5]);
            let minimum = 6usize
                .checked_add(components.checked_mul(3).ok_or(ImageError::InvalidImage)?)
                .ok_or(ImageError::InvalidImage)?;
            if components == 0 || data.len() < minimum {
                return Err(ImageError::InvalidImage);
            }
            return ImageDimensions::new(
                u32::from(read_u16_be(data, 3).ok_or(ImageError::InvalidImage)?),
                u32::from(read_u16_be(data, 1).ok_or(ImageError::InvalidImage)?),
            )
            .and_then(|dimensions| {
                // A start-of-frame alone is insufficient. JPEG must carry a
                // bounded start-of-scan marker before the exact final EOI.
                jpeg_has_bounded_sos(bytes, end, header_limit, limits, &mut items)
                    .map(|()| dimensions)
            });
        }
        offset = end;
    }
    Err(ImageError::InvalidImage)
}

fn jpeg_has_bounded_sos(
    bytes: &[u8],
    mut offset: usize,
    header_limit: usize,
    limits: &ImageLimits,
    items: &mut usize,
) -> Result<(), ImageError> {
    while offset < bytes.len().saturating_sub(2) {
        if offset >= header_limit || bytes.get(offset) != Some(&0xff) {
            return Err(ImageError::MetadataTooLarge);
        }
        while bytes.get(offset) == Some(&0xff) {
            offset = offset.checked_add(1).ok_or(ImageError::InvalidImage)?;
            if offset >= header_limit {
                return Err(ImageError::MetadataTooLarge);
            }
        }
        let marker = *bytes.get(offset).ok_or(ImageError::InvalidImage)?;
        offset = offset.checked_add(1).ok_or(ImageError::InvalidImage)?;
        if marker == 0 || marker == 0xff || marker == 0xd8 || marker == 0xd9 {
            return Err(ImageError::InvalidImage);
        }
        take_container_item(items, limits)?;
        if matches!(marker, 0x01 | 0xd0..=0xd7) {
            continue;
        }
        let segment_length =
            usize::from(read_u16_be(bytes, offset).ok_or(ImageError::InvalidImage)?);
        if segment_length < 2 {
            return Err(ImageError::InvalidImage);
        }
        let data_start = offset.checked_add(2).ok_or(ImageError::InvalidImage)?;
        let end = offset
            .checked_add(segment_length)
            .ok_or(ImageError::InvalidImage)?;
        if end > bytes.len() || end > header_limit {
            return Err(ImageError::MetadataTooLarge);
        }
        if marker == 0xda {
            let data = &bytes[data_start..end];
            if data.is_empty() || end >= bytes.len().saturating_sub(2) {
                return Err(ImageError::InvalidImage);
            }
            let components = usize::from(data[0]);
            let minimum = 4usize
                .checked_add(components.checked_mul(2).ok_or(ImageError::InvalidImage)?)
                .ok_or(ImageError::InvalidImage)?;
            return (components > 0 && data.len() >= minimum)
                .then_some(())
                .ok_or(ImageError::InvalidImage);
        }
        offset = end;
    }
    Err(ImageError::InvalidImage)
}

fn is_jpeg_sof(marker: u8) -> bool {
    matches!(
        marker,
        0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf
    )
}

fn parse_gif(bytes: &[u8], limits: &ImageLimits) -> Result<ImageDimensions, ImageError> {
    if bytes.len() < 15 || !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Err(ImageError::InvalidImage);
    }
    let dimensions = ImageDimensions::new(
        u32::from(read_u16_le(bytes, 6).ok_or(ImageError::InvalidImage)?),
        u32::from(read_u16_le(bytes, 8).ok_or(ImageError::InvalidImage)?),
    )?;
    let mut offset = 13usize;
    let packed = bytes[10];
    if packed & 0x80 != 0 {
        let entries = 1usize << (usize::from(packed & 0x07) + 1);
        let table_bytes = entries.checked_mul(3).ok_or(ImageError::InvalidImage)?;
        offset = offset
            .checked_add(table_bytes)
            .ok_or(ImageError::InvalidImage)?;
        if offset > bytes.len() {
            return Err(ImageError::InvalidImage);
        }
    }

    let mut items = 0;
    let mut saw_image = false;
    loop {
        take_container_item(&mut items, limits)?;
        let marker = *bytes.get(offset).ok_or(ImageError::InvalidImage)?;
        offset = offset.checked_add(1).ok_or(ImageError::InvalidImage)?;
        match marker {
            0x3b if saw_image && offset == bytes.len() => return Ok(dimensions),
            0x3b => return Err(ImageError::InvalidImage),
            0x21 => {
                let label = *bytes.get(offset).ok_or(ImageError::InvalidImage)?;
                offset = offset.checked_add(1).ok_or(ImageError::InvalidImage)?;
                // A plain-text extension is a GIF graphic-rendering block, so
                // allowing it alongside an image descriptor would admit a
                // multi-frame container without a second `0x2c` marker.
                if label == 0x01 {
                    return Err(ImageError::UnsupportedAnimation);
                }
                if label != 0xfe {
                    let fixed_len =
                        usize::from(*bytes.get(offset).ok_or(ImageError::InvalidImage)?);
                    offset = offset.checked_add(1).ok_or(ImageError::InvalidImage)?;
                    if matches!(label, 0xf9) && fixed_len != 4
                        || matches!(label, 0xff) && fixed_len != 11
                        || matches!(label, 0x01) && fixed_len != 12
                    {
                        return Err(ImageError::InvalidImage);
                    }
                    offset = offset
                        .checked_add(fixed_len)
                        .ok_or(ImageError::InvalidImage)?;
                    if offset > bytes.len() {
                        return Err(ImageError::InvalidImage);
                    }
                }
                skip_gif_subblocks(bytes, &mut offset, &mut items, limits)?;
            }
            0x2c => {
                if saw_image {
                    return Err(ImageError::UnsupportedAnimation);
                }
                let descriptor_start = offset;
                let descriptor_end = offset.checked_add(9).ok_or(ImageError::InvalidImage)?;
                if descriptor_end > bytes.len() {
                    return Err(ImageError::InvalidImage);
                }
                let left = u32::from(
                    read_u16_le(bytes, descriptor_start).ok_or(ImageError::InvalidImage)?,
                );
                let top = u32::from(
                    read_u16_le(bytes, descriptor_start + 2).ok_or(ImageError::InvalidImage)?,
                );
                let width = u32::from(
                    read_u16_le(bytes, descriptor_start + 4).ok_or(ImageError::InvalidImage)?,
                );
                let height = u32::from(
                    read_u16_le(bytes, descriptor_start + 6).ok_or(ImageError::InvalidImage)?,
                );
                if width == 0
                    || height == 0
                    || left
                        .checked_add(width)
                        .is_none_or(|right| right > dimensions.width)
                    || top
                        .checked_add(height)
                        .is_none_or(|bottom| bottom > dimensions.height)
                {
                    return Err(ImageError::InvalidImage);
                }
                let image_packed = bytes[descriptor_start + 8];
                offset = descriptor_end;
                if image_packed & 0x80 != 0 {
                    let entries = 1usize << (usize::from(image_packed & 0x07) + 1);
                    let table_bytes = entries.checked_mul(3).ok_or(ImageError::InvalidImage)?;
                    offset = offset
                        .checked_add(table_bytes)
                        .ok_or(ImageError::InvalidImage)?;
                    if offset > bytes.len() {
                        return Err(ImageError::InvalidImage);
                    }
                }
                let lzw_minimum = *bytes.get(offset).ok_or(ImageError::InvalidImage)?;
                if !(2..=8).contains(&lzw_minimum) {
                    return Err(ImageError::InvalidImage);
                }
                offset = offset.checked_add(1).ok_or(ImageError::InvalidImage)?;
                skip_gif_subblocks(bytes, &mut offset, &mut items, limits)?;
                saw_image = true;
            }
            _ => return Err(ImageError::InvalidImage),
        }
    }
}

fn skip_gif_subblocks(
    bytes: &[u8],
    offset: &mut usize,
    items: &mut usize,
    limits: &ImageLimits,
) -> Result<(), ImageError> {
    loop {
        let length = usize::from(*bytes.get(*offset).ok_or(ImageError::InvalidImage)?);
        *offset = offset.checked_add(1).ok_or(ImageError::InvalidImage)?;
        if length == 0 {
            return Ok(());
        }
        take_container_item(items, limits)?;
        *offset = offset.checked_add(length).ok_or(ImageError::InvalidImage)?;
        if *offset > bytes.len() {
            return Err(ImageError::InvalidImage);
        }
    }
}

fn parse_webp(bytes: &[u8], limits: &ImageLimits) -> Result<ImageDimensions, ImageError> {
    if bytes.len() < 20 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(ImageError::InvalidImage);
    }
    let declared = usize::try_from(read_u32_le(bytes, 4).ok_or(ImageError::InvalidImage)?)
        .map_err(|_| ImageError::InvalidImage)?;
    if declared
        .checked_add(8)
        .is_none_or(|expected| expected != bytes.len())
    {
        return Err(ImageError::InvalidImage);
    }

    let mut offset = 12usize;
    let mut items = 0;
    let mut dimensions = None;
    let mut saw_extended_header = false;
    let mut saw_payload = false;
    while offset < bytes.len() {
        take_container_item(&mut items, limits)?;
        let header_end = offset.checked_add(8).ok_or(ImageError::InvalidImage)?;
        if header_end > bytes.len() {
            return Err(ImageError::InvalidImage);
        }
        let kind = &bytes[offset..offset + 4];
        let length =
            usize::try_from(read_u32_le(bytes, offset + 4).ok_or(ImageError::InvalidImage)?)
                .map_err(|_| ImageError::InvalidImage)?;
        let data_start = header_end;
        let data_end = data_start
            .checked_add(length)
            .ok_or(ImageError::InvalidImage)?;
        if data_end > bytes.len() {
            return Err(ImageError::InvalidImage);
        }
        let data = &bytes[data_start..data_end];
        if kind == b"VP8X" {
            if saw_extended_header || saw_payload || data.len() != 10 || data[1..4] != [0, 0, 0] {
                return Err(ImageError::InvalidImage);
            }
            saw_extended_header = true;
            if data[0] & 0x02 != 0 {
                return Err(ImageError::UnsupportedAnimation);
            }
            let width = u32::from(data[4]) | (u32::from(data[5]) << 8) | (u32::from(data[6]) << 16);
            let height =
                u32::from(data[7]) | (u32::from(data[8]) << 8) | (u32::from(data[9]) << 16);
            set_webp_dimensions(
                &mut dimensions,
                ImageDimensions::new(width.saturating_add(1), height.saturating_add(1))?,
            )?;
        } else if kind == b"VP8 " {
            if saw_payload || data.len() < 11 || data[3..6] != [0x9d, 0x01, 0x2a] {
                return Err(ImageError::InvalidImage);
            }
            let width = u32::from(read_u16_le(data, 6).ok_or(ImageError::InvalidImage)? & 0x3fff);
            let height = u32::from(read_u16_le(data, 8).ok_or(ImageError::InvalidImage)? & 0x3fff);
            set_webp_dimensions(&mut dimensions, ImageDimensions::new(width, height)?)?;
            saw_payload = true;
        } else if kind == b"VP8L" {
            if saw_payload || data.len() < 6 || data[0] != 0x2f {
                return Err(ImageError::InvalidImage);
            }
            let packed = read_u32_le(data, 1).ok_or(ImageError::InvalidImage)?;
            let width = 1 + (packed & 0x3fff);
            let height = 1 + ((packed >> 14) & 0x3fff);
            set_webp_dimensions(&mut dimensions, ImageDimensions::new(width, height)?)?;
            saw_payload = true;
        } else if kind == b"ANIM" || kind == b"ANMF" {
            return Err(ImageError::UnsupportedAnimation);
        }
        offset = data_end
            .checked_add(length & 1)
            .ok_or(ImageError::InvalidImage)?;
        if offset > bytes.len() {
            return Err(ImageError::InvalidImage);
        }
    }
    if offset != bytes.len() || !saw_payload {
        return Err(ImageError::InvalidImage);
    }
    dimensions.ok_or(ImageError::InvalidImage)
}

fn set_webp_dimensions(
    target: &mut Option<ImageDimensions>,
    dimensions: ImageDimensions,
) -> Result<(), ImageError> {
    match target {
        Some(current) if *current != dimensions => Err(ImageError::InvalidImage),
        Some(_) => Ok(()),
        None => {
            *target = Some(dimensions);
            Ok(())
        }
    }
}

fn build_transmission<'a>(
    protocol: ImageProtocol,
    id: ImageId,
    image: &'a TerminalImage,
    layout: ImageLayout,
    limits: &ImageLimits,
) -> Result<(ImageTransmission<'a>, usize, usize), ImageError> {
    let source_len = image.bytes().len();
    let chunk_bytes = limits.max_protocol_chunk_bytes;
    let raw_chunk_bytes = chunk_bytes
        .checked_div(4)
        .and_then(|groups| groups.checked_mul(3))
        .filter(|value| *value > 0)
        .ok_or(ImageError::InvalidLimit)?;
    let chunks = ceil_div_usize(source_len, raw_chunk_bytes);
    if chunks == 0 || chunks > limits.max_protocol_chunks {
        return Err(ImageError::TooManyChunks);
    }
    let base64_len = base64_len(source_len)?;
    let (first_header, continuation_more, continuation_last, output_len) = match protocol {
        ImageProtocol::Kitty => {
            let first = kitty_transmit_header(id, layout, chunks > 1);
            let more = "\x1b_Gm=1;".to_owned();
            let last = "\x1b_Gm=0;".to_owned();
            let mut total = checked_add(first.len(), base64_len)?;
            total = checked_add(total, KITTY_ST.len())?;
            if chunks > 1 {
                let middle_count = chunks.saturating_sub(2);
                let middle = checked_mul(middle_count, checked_add(more.len(), KITTY_ST.len())?)?;
                total = checked_add(total, middle)?;
                total = checked_add(total, checked_add(last.len(), KITTY_ST.len())?)?;
            }
            (first, more, last, total)
        }
        ImageProtocol::Iterm2 => {
            let first = iterm_transmit_header(image, layout)?;
            let total = checked_add(checked_add(first.len(), base64_len)?, ITERM_ST.len())?;
            (first, String::new(), String::new(), total)
        }
    };
    if output_len > limits.max_encoded_output_bytes {
        return Err(ImageError::EncodedOutputTooLarge);
    }
    Ok((
        ImageTransmission {
            image,
            protocol,
            chunk_bytes,
            chunks,
            first_header,
            continuation_more,
            continuation_last,
        },
        output_len,
        chunks,
    ))
}

fn emit_transmission<E, F>(transmission: &ImageTransmission<'_>, write: &mut F) -> Result<(), E>
where
    F: FnMut(&[u8]) -> Result<(), E>,
{
    let raw_chunk_bytes = transmission.chunk_bytes / 4 * 3;
    let mut buffer = [0_u8; HARD_MAX_PROTOCOL_CHUNK_BYTES];
    match transmission.protocol {
        ImageProtocol::Kitty => {
            for chunk in 0..transmission.chunks {
                if chunk == 0 {
                    write(transmission.first_header.as_bytes())?;
                } else if chunk + 1 == transmission.chunks {
                    write(transmission.continuation_last.as_bytes())?;
                } else {
                    write(transmission.continuation_more.as_bytes())?;
                }
                let start = chunk.saturating_mul(raw_chunk_bytes);
                let end = start
                    .checked_add(raw_chunk_bytes)
                    .unwrap_or(transmission.image.bytes().len())
                    .min(transmission.image.bytes().len());
                let encoded =
                    encode_base64_into(&transmission.image.bytes()[start..end], &mut buffer);
                write(&buffer[..encoded])?;
                write(KITTY_ST)?;
            }
        }
        ImageProtocol::Iterm2 => {
            write(transmission.first_header.as_bytes())?;
            for chunk in 0..transmission.chunks {
                let start = chunk.saturating_mul(raw_chunk_bytes);
                let end = start
                    .checked_add(raw_chunk_bytes)
                    .unwrap_or(transmission.image.bytes().len())
                    .min(transmission.image.bytes().len());
                let encoded =
                    encode_base64_into(&transmission.image.bytes()[start..end], &mut buffer);
                write(&buffer[..encoded])?;
            }
            write(ITERM_ST)?;
        }
    }
    Ok(())
}

fn kitty_transmit_header(id: ImageId, layout: ImageLayout, more: bool) -> String {
    format!(
        "\x1b_Ga=T,t=d,f=100,i={},q=2,c={},r={},m={};",
        id.get(),
        layout.columns(),
        layout.rows(),
        u8::from(more),
    )
}

fn kitty_delete_sequence(id: ImageId) -> String {
    format!("\x1b_Ga=d,d=I,i={},q=2\x1b\\", id.get())
}

fn iterm_transmit_header(image: &TerminalImage, layout: ImageLayout) -> Result<String, ImageError> {
    let mut header = String::from("\x1b]1337;File=");
    if let Some(filename) = image.metadata().filename() {
        header.push_str("name=");
        header.push_str(&base64_string(filename.as_str().as_bytes())?);
        header.push(';');
    }
    // The retained semantic reservation owns cursor advancement and scrollback;
    // keep this protocol side effect from moving the cursor independently.
    header.push_str(&format!(
        "size={};inline=1;doNotMoveCursor=1;width={};height={};preserveAspectRatio=1:",
        image.byte_len(),
        layout.columns(),
        layout.rows(),
    ));
    Ok(header)
}

fn base64_string(bytes: &[u8]) -> Result<String, ImageError> {
    let length = base64_len(bytes.len())?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(length)
        .map_err(|_| ImageError::AllocationFailed)?;
    encoded.resize(length, 0);
    let written = encode_base64_into(bytes, &mut encoded);
    debug_assert_eq!(written, length);
    String::from_utf8(encoded).map_err(|_| ImageError::InvalidImage)
}

fn encode_base64_into(input: &[u8], output: &mut [u8]) -> usize {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut source = 0usize;
    let mut target = 0usize;
    while source + 3 <= input.len() {
        let a = input[source];
        let b = input[source + 1];
        let c = input[source + 2];
        output[target] = TABLE[usize::from(a >> 2)];
        output[target + 1] = TABLE[usize::from(((a & 0x03) << 4) | (b >> 4))];
        output[target + 2] = TABLE[usize::from(((b & 0x0f) << 2) | (c >> 6))];
        output[target + 3] = TABLE[usize::from(c & 0x3f)];
        source += 3;
        target += 4;
    }
    let remainder = input.len().saturating_sub(source);
    if remainder == 1 {
        let a = input[source];
        output[target] = TABLE[usize::from(a >> 2)];
        output[target + 1] = TABLE[usize::from((a & 0x03) << 4)];
        output[target + 2] = b'=';
        output[target + 3] = b'=';
        target += 4;
    } else if remainder == 2 {
        let a = input[source];
        let b = input[source + 1];
        output[target] = TABLE[usize::from(a >> 2)];
        output[target + 1] = TABLE[usize::from(((a & 0x03) << 4) | (b >> 4))];
        output[target + 2] = TABLE[usize::from((b & 0x0f) << 2)];
        output[target + 3] = b'=';
        target += 4;
    }
    target
}

fn parse_kitty_reply(reply: &str) -> Option<ImageId> {
    let value = reply.strip_prefix("\x1b_Gi=")?.strip_suffix(";OK\x1b\\")?;
    ImageId::new(parse_bounded_decimal(value, 10)?).ok()
}

fn parse_standard_cell_reply(reply: &str) -> Option<CellPixelSize> {
    let value = reply.strip_prefix("\x1b[6;")?.strip_suffix('t')?;
    let (height, width) = parse_cell_pair(value)?;
    CellPixelSize::new(width, height)
}

fn parse_iterm2_cell_reply(reply: &str) -> Option<CellPixelSize> {
    let value = reply.strip_prefix("\x1b]1337;ReportCellSize=")?;
    let value = value
        .strip_suffix('\u{7}')
        .or_else(|| value.strip_suffix("\x1b\\"))?;
    let (height, width) = parse_cell_pair(value)?;
    CellPixelSize::new(width, height)
}

fn parse_cell_pair(value: &str) -> Option<(u16, u16)> {
    let (height, width) = value.split_once(';')?;
    if width.contains(';') {
        return None;
    }
    let height = u16::try_from(parse_bounded_decimal(height, 5)?).ok()?;
    let width = u16::try_from(parse_bounded_decimal(width, 5)?).ok()?;
    Some((height, width))
}

fn parse_bounded_decimal(value: &str, max_digits: usize) -> Option<u32> {
    if value.is_empty()
        || value.len() > max_digits
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    value.bytes().try_fold(0_u32, |number, byte| {
        number.checked_mul(10)?.checked_add(u32::from(byte - b'0'))
    })
}

fn take_container_item(items: &mut usize, limits: &ImageLimits) -> Result<(), ImageError> {
    *items = items.checked_add(1).ok_or(ImageError::MetadataTooLarge)?;
    (*items <= limits.max_container_items)
        .then_some(())
        .ok_or(ImageError::MetadataTooLarge)
}

fn read_u16_be(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset.checked_add(1)?)?,
    ]))
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset.checked_add(1)?)?,
    ]))
}

fn read_u32_be(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset.checked_add(1)?)?,
        *bytes.get(offset.checked_add(2)?)?,
        *bytes.get(offset.checked_add(3)?)?,
    ]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset.checked_add(1)?)?,
        *bytes.get(offset.checked_add(2)?)?,
        *bytes.get(offset.checked_add(3)?)?,
    ]))
}

fn base64_len(length: usize) -> Result<usize, ImageError> {
    let full = length / 3;
    let remainder = length % 3;
    let base = full
        .checked_mul(4)
        .ok_or(ImageError::EncodedOutputTooLarge)?;
    let tail = usize::from(remainder != 0)
        .checked_mul(4)
        .ok_or(ImageError::EncodedOutputTooLarge)?;
    checked_add(base, tail)
}

fn checked_add(left: usize, right: usize) -> Result<usize, ImageError> {
    left.checked_add(right)
        .ok_or(ImageError::EncodedOutputTooLarge)
}

fn checked_mul(left: usize, right: usize) -> Result<usize, ImageError> {
    left.checked_mul(right)
        .ok_or(ImageError::EncodedOutputTooLarge)
}

fn ceil_div_usize(value: usize, divisor: usize) -> usize {
    value / divisor + usize::from(value % divisor != 0)
}

fn ceil_div_u64(value: u64, divisor: u64) -> u64 {
    value / divisor + u64::from(value % divisor != 0)
}

fn ceil_div_u128(value: u128, divisor: u128) -> u128 {
    value / divisor + u128::from(value % divisor != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColorDepth;

    fn push_png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        output.extend_from_slice(&(u32::try_from(data.len()).unwrap()).to_be_bytes());
        output.extend_from_slice(kind);
        output.extend_from_slice(data);
        output.extend_from_slice(&png_crc32(&[kind, data]).to_be_bytes());
    }

    fn png(width: u32, height: u32, idat_len: usize) -> Vec<u8> {
        let mut output = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut header = Vec::new();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        push_png_chunk(&mut output, b"IHDR", &header);
        let idat = (0..idat_len.max(1))
            .map(|index| (index as u8).wrapping_mul(31))
            .collect::<Vec<_>>();
        push_png_chunk(&mut output, b"IDAT", &idat);
        push_png_chunk(&mut output, b"IEND", &[]);
        output
    }

    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut output = vec![0xff, 0xd8];
        output.extend_from_slice(&[
            0xff,
            0xc0,
            0x00,
            0x11,
            8,
            (height >> 8) as u8,
            height as u8,
            (width >> 8) as u8,
            width as u8,
            3,
            1,
            0x11,
            0,
            2,
            0x11,
            0,
            3,
            0x11,
            0,
        ]);
        output.extend_from_slice(&[0xff, 0xda, 0x00, 0x08, 1, 1, 0, 0, 0x3f, 0, 0, 0xff, 0xd9]);
        output
    }

    fn gif(width: u16, height: u16) -> Vec<u8> {
        let mut output = b"GIF89a".to_vec();
        output.extend_from_slice(&width.to_le_bytes());
        output.extend_from_slice(&height.to_le_bytes());
        output.extend_from_slice(&[0x80, 0, 0]);
        output.extend_from_slice(&[0, 0, 0, 0xff, 0xff, 0xff]);
        output.push(0x2c);
        output.extend_from_slice(&0_u16.to_le_bytes());
        output.extend_from_slice(&0_u16.to_le_bytes());
        output.extend_from_slice(&width.to_le_bytes());
        output.extend_from_slice(&height.to_le_bytes());
        output.push(0);
        output.extend_from_slice(&[2, 2, 0x4c, 1, 0, 0x3b]);
        output
    }

    fn webp(width: u32, height: u32) -> Vec<u8> {
        assert!((1..=16_384).contains(&width));
        assert!((1..=16_384).contains(&height));
        let packed = (width - 1) | ((height - 1) << 14);
        let mut body = b"WEBPVP8L".to_vec();
        body.extend_from_slice(&6_u32.to_le_bytes());
        body.push(0x2f);
        body.extend_from_slice(&packed.to_le_bytes());
        body.push(0);
        let mut output = b"RIFF".to_vec();
        output.extend_from_slice(&(u32::try_from(body.len()).unwrap()).to_le_bytes());
        output.extend_from_slice(&body);
        output
    }

    fn image_id(value: u32) -> ImageId {
        ImageId::new(value).unwrap()
    }

    fn image(bytes: Vec<u8>) -> TerminalImage {
        TerminalImage::from_bytes(bytes).unwrap()
    }

    #[test]
    fn validates_all_supported_container_headers_and_dimensions() {
        for (bytes, format, dimensions) in [
            (png(13, 7, 8), ImageFormat::Png, (13, 7)),
            (jpeg(13, 7), ImageFormat::Jpeg, (13, 7)),
            (gif(13, 7), ImageFormat::Gif, (13, 7)),
            (webp(13, 7), ImageFormat::Webp, (13, 7)),
        ] {
            let image = image(bytes);
            assert_eq!(image.format(), format);
            assert_eq!(image.dimensions().width(), dimensions.0);
            assert_eq!(image.dimensions().height(), dimensions.1);
        }
    }

    #[test]
    fn format_matrix_is_explicit_and_conservative() {
        assert!(ImageProtocol::Kitty.supports_format(ImageFormat::Png));
        assert!(!ImageProtocol::Kitty.supports_format(ImageFormat::Jpeg));
        assert!(!ImageProtocol::Kitty.supports_format(ImageFormat::Gif));
        assert!(!ImageProtocol::Kitty.supports_format(ImageFormat::Webp));
        assert!(ImageProtocol::Iterm2.supports_format(ImageFormat::Png));
        assert!(ImageProtocol::Iterm2.supports_format(ImageFormat::Jpeg));
        assert!(ImageProtocol::Iterm2.supports_format(ImageFormat::Gif));
        assert!(!ImageProtocol::Iterm2.supports_format(ImageFormat::Webp));
    }

    #[test]
    fn validates_metadata_dimensions_filenames_and_payload_bounds_before_copying() {
        let metadata = ImageMetadata::default()
            .with_expected_dimensions(ImageDimensions::new(2, 1).unwrap())
            .with_filename(ImageFilename::new("safe-image.png").unwrap());
        assert!(matches!(
            TerminalImage::from_slice_with_metadata(
                &png(1, 1, 4),
                metadata,
                &ImageLimits::default()
            ),
            Err(ImageError::MetadataDimensionMismatch)
        ));
        assert_eq!(
            ImageFilename::new("../escape.png"),
            Err(ImageError::UnsafeFilename)
        );
        assert_eq!(ImageFilename::new("."), Err(ImageError::UnsafeFilename));
        assert_eq!(ImageFilename::new(".."), Err(ImageError::UnsafeFilename));
        assert_eq!(
            ImageFilename::new("bad\u{1b}name"),
            Err(ImageError::UnsafeFilename)
        );

        let tiny = ImageLimits::default().with_max_payload_bytes(50).unwrap();
        assert!(matches!(
            TerminalImage::from_slice_with_metadata(
                &png(1, 1, 64),
                ImageMetadata::default(),
                &tiny
            ),
            Err(ImageError::PayloadTooLarge)
        ));

        let retained_limits = ImageLimits::default().with_max_payload_bytes(512).unwrap();
        let mut excess_capacity = Vec::with_capacity(1_024);
        excess_capacity.extend_from_slice(&png(1, 1, 4));
        let retained = TerminalImage::from_bytes_with_metadata(
            excess_capacity,
            ImageMetadata::default(),
            &retained_limits,
        )
        .unwrap();
        assert!(retained.bytes.capacity() <= retained_limits.max_payload_bytes());

        assert!(matches!(
            TerminalImage::from_bytes(png(9_000, 1, 1)),
            Err(ImageError::DimensionsTooLarge)
        ));
        assert!(matches!(
            TerminalImage::from_bytes(png(4_000, 2_000, 1)),
            Err(ImageError::PixelCountTooLarge)
        ));

        let already_validated = image(png(1, 1, 64));
        assert!(matches!(
            ImageProtocolEncoder::new(ImageProtocol::Kitty, tiny).encode_place(
                image_id(1),
                &already_validated,
                ImageLayout::new(1, 1).unwrap()
            ),
            Err(ImageError::PayloadTooLarge)
        ));
    }

    #[test]
    fn rejects_corrupt_truncated_and_polyglot_containers() {
        let fixtures = [png(2, 2, 4), jpeg(2, 2), gif(2, 2), webp(2, 2)];
        for fixture in fixtures {
            let mut truncated = fixture.clone();
            truncated.pop();
            assert!(TerminalImage::from_bytes(truncated).is_err());
            let mut polyglot = fixture;
            polyglot.extend_from_slice(b"not-an-image-tail");
            assert!(TerminalImage::from_bytes(polyglot).is_err());
        }
        let mut corrupt = png(2, 2, 4);
        corrupt[20] ^= 0x80;
        assert!(matches!(
            TerminalImage::from_bytes(corrupt),
            Err(ImageError::InvalidImage)
        ));
        assert!(matches!(
            TerminalImage::from_bytes(b"\x89PNG\r\n\x1a\n".to_vec()),
            Err(ImageError::InvalidImage)
        ));
        assert!(matches!(
            TerminalImage::from_bytes(Vec::new()),
            Err(ImageError::InvalidImage)
        ));

        let mut separated_idat = png(2, 2, 4);
        let mut split_chunks = Vec::new();
        push_png_chunk(&mut split_chunks, b"tEXt", b"safe");
        push_png_chunk(&mut split_chunks, b"IDAT", &[1]);
        let iend = separated_idat.len() - 12;
        separated_idat.splice(iend..iend, split_chunks);
        assert!(matches!(
            TerminalImage::from_bytes(separated_idat),
            Err(ImageError::InvalidImage)
        ));

        let mut duplicate_webp_payload = webp(2, 2);
        let second_payload = duplicate_webp_payload[12..].to_vec();
        duplicate_webp_payload.extend_from_slice(&second_payload);
        let declared = u32::try_from(duplicate_webp_payload.len() - 8).unwrap();
        duplicate_webp_payload[4..8].copy_from_slice(&declared.to_le_bytes());
        assert!(matches!(
            TerminalImage::from_bytes(duplicate_webp_payload),
            Err(ImageError::InvalidImage)
        ));
    }

    #[test]
    fn rejects_animated_containers_before_terminal_side_decoding() {
        let mut animated_png = png(2, 2, 4);
        let mut control = Vec::new();
        push_png_chunk(&mut control, b"acTL", &[0; 8]);
        let iend = animated_png.len() - 12;
        animated_png.splice(iend..iend, control);
        assert!(matches!(
            TerminalImage::from_bytes(animated_png),
            Err(ImageError::UnsupportedAnimation)
        ));

        let mut animated_gif = gif(2, 2);
        assert_eq!(animated_gif.pop(), Some(0x3b));
        animated_gif.extend_from_slice(&[
            0x2c, 0, 0, 0, 0, 2, 0, 2, 0, 0, // image descriptor
            2, 2, 0x4c, 1, 0, // LZW stream
            0x3b,
        ]);
        assert!(matches!(
            TerminalImage::from_bytes(animated_gif),
            Err(ImageError::UnsupportedAnimation)
        ));

        let mut plain_text_gif = gif(2, 2);
        let mut plain_text_extension = vec![0x21, 0x01, 12];
        plain_text_extension.extend_from_slice(&[0; 12]);
        plain_text_extension.push(0);
        // Header (6) + logical screen descriptor (7) + two-color table (6).
        plain_text_gif.splice(19..19, plain_text_extension);
        assert!(matches!(
            TerminalImage::from_bytes(plain_text_gif),
            Err(ImageError::UnsupportedAnimation)
        ));

        let mut body = b"WEBPVP8X".to_vec();
        body.extend_from_slice(&10_u32.to_le_bytes());
        body.extend_from_slice(&[0x02, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        body.extend_from_slice(b"ANMF");
        body.extend_from_slice(&0_u32.to_le_bytes());
        let mut animated_webp = b"RIFF".to_vec();
        animated_webp.extend_from_slice(&(u32::try_from(body.len()).unwrap()).to_le_bytes());
        animated_webp.extend_from_slice(&body);
        assert!(matches!(
            TerminalImage::from_bytes(animated_webp),
            Err(ImageError::UnsupportedAnimation)
        ));
    }

    #[test]
    fn layout_uses_cell_pixels_resizes_and_never_wraps() {
        let cell = CellPixelSize::new(8, 16).unwrap();
        assert_eq!(cell_rows_for_pixels(33, Some(cell)), Ok(3));
        assert_eq!(cell_rows_for_pixels(33, None), Ok(1));
        assert!(cell_rows_for_pixels(u32::MAX, Some(CellPixelSize::new(1, 1).unwrap())).is_err());

        let dimensions = ImageDimensions::new(1_600, 800).unwrap();
        let wide =
            ImageLayout::fit(dimensions, ImageViewport::new(20, 10, Some(cell)).unwrap()).unwrap();
        assert_eq!((wide.columns(), wide.rows()), (20, 5));
        let narrow =
            ImageLayout::fit(dimensions, ImageViewport::new(10, 10, Some(cell)).unwrap()).unwrap();
        assert_eq!((narrow.columns(), narrow.rows()), (10, 3));
        let unknown =
            ImageLayout::fit(dimensions, ImageViewport::new(10, 10, None).unwrap()).unwrap();
        assert_eq!((unknown.columns(), unknown.rows()), (1, 1));
        assert!(ImageLayout::new(1, MAX_RESERVED_IMAGE_ROWS + 1).is_err());
    }

    #[test]
    fn capability_forcing_and_reply_parsing_are_deterministic_and_bounded() {
        let mut terminal = TerminalCapabilities::interactive(ColorDepth::TrueColor, true);
        terminal.kitty_graphics = false;
        terminal.iterm2_images = false;
        let mut detected =
            ImageCapabilities::detect(&terminal, &ImageCapabilityOverrides::default());
        assert_eq!(detected.protocol(), None);

        let forced = ImageCapabilities::detect(
            &terminal,
            &ImageCapabilityOverrides {
                force: Some(ImageProtocol::Iterm2),
                cell_pixel_size: Some(CellPixelSize::new(9, 18).unwrap()),
                ..ImageCapabilityOverrides::default()
            },
        );
        assert_eq!(forced.protocol(), Some(ImageProtocol::Iterm2));
        assert_eq!(forced.cell_pixel_size().unwrap().width(), 9);
        let plain_forced = ImageCapabilities::detect(
            &TerminalCapabilities::plain(),
            &ImageCapabilityOverrides {
                force: Some(ImageProtocol::Kitty),
                ..ImageCapabilityOverrides::default()
            },
        );
        assert_eq!(plain_forced.protocol(), None);

        let id = image_id(77);
        let limits = ImageLimits::default();
        let reply = "\x1b_Gi=77;OK\x1b\\";
        assert_eq!(
            parse_terminal_image_reply(reply, Some(id), &limits),
            Some(TerminalImageReply::KittyGraphicsSupported { query_id: id })
        );
        assert_eq!(
            parse_terminal_image_reply(reply, Some(image_id(78)), &limits),
            None
        );
        assert_eq!(
            parse_terminal_image_reply("\x1b_Gi=77;OK\x1b\\\x1b[6;16;8t", Some(id), &limits),
            None
        );
        assert_eq!(
            parse_terminal_image_reply("\x1b[6;16;8t", None, &limits),
            Some(TerminalImageReply::CellPixels(
                CellPixelSize::new(8, 16).unwrap()
            ))
        );
        assert_eq!(
            parse_terminal_image_reply("\x1b]1337;ReportCellSize=16;8\u{7}", None, &limits),
            Some(TerminalImageReply::Iterm2CellPixels(
                CellPixelSize::new(8, 16).unwrap()
            ))
        );
        assert!(parse_terminal_image_reply(&"x".repeat(513), None, &limits).is_none());

        detected.apply_reply(TerminalImageReply::KittyGraphicsSupported { query_id: id });
        assert_eq!(detected.protocol(), Some(ImageProtocol::Kitty));
        detected.apply_reply(TerminalImageReply::CellPixels(
            CellPixelSize::new(8, 16).unwrap(),
        ));
        assert_eq!(detected.cell_pixel_size().unwrap().height(), 16);

        let query = ImageCapabilityQuery::kitty_graphics(id, &limits);
        let mut wire = Vec::new();
        query.write_to(&mut wire).unwrap();
        assert_eq!(wire, b"\x1b_Ga=q,i=77,s=1,v=1,f=24;\x1b\\");
        assert_eq!(query.timeout(), DEFAULT_QUERY_TIMEOUT);
        assert!(matches!(
            query.parse_reply(reply, &limits),
            Some(TerminalImageReply::KittyGraphicsSupported { query_id }) if query_id == id
        ));
        assert!(query.parse_reply("\x1b[6;16;8t", &limits).is_none());
        let cell_query = ImageCapabilityQuery::cell_pixels(&limits);
        assert!(matches!(
            cell_query.parse_reply("\x1b[6;16;8t", &limits),
            Some(TerminalImageReply::CellPixels(_))
        ));
        assert!(cell_query
            .parse_reply("\x1b]1337;ReportCellSize=16;8\u{7}", &limits)
            .is_none());
    }

    #[test]
    fn kitty_chunks_are_bounded_and_reassemble_to_one_base64_stream() {
        let image = image(png(3, 2, 97));
        let limits = ImageLimits::default()
            .with_max_protocol_chunk_bytes(16)
            .unwrap()
            .with_max_protocol_chunks(256)
            .unwrap();
        let command = ImageProtocolEncoder::new(ImageProtocol::Kitty, limits)
            .encode_place(image_id(9), &image, ImageLayout::new(2, 3).unwrap())
            .unwrap();
        let mut wire = Vec::new();
        command.write_to(&mut wire).unwrap();
        assert_eq!(command.encoded_len(), wire.len());
        assert!(command.payload_chunks() > 1);
        let wire = String::from_utf8(wire).unwrap();
        assert!(wire.starts_with("\x1b_Ga=T,t=d,f=100,i=9,q=2,c=2,r=3,m=1;"));
        assert!(wire.ends_with("\x1b\\"));
        let sequences = wire
            .split("\x1b\\")
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(sequences.len(), command.payload_chunks());
        let joined = sequences
            .iter()
            .map(|sequence| sequence.split_once(';').unwrap().1)
            .collect::<String>();
        assert_eq!(joined, base64_string(image.bytes()).unwrap());
        assert!(sequences.iter().all(|sequence| {
            sequence.split_once(';').is_some_and(|(_, body)| {
                body.len() <= 16
                    && body.bytes().all(|byte| {
                        byte.is_ascii_alphabetic()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'+' | b'/' | b'=')
                    })
            })
        }));
    }

    #[test]
    fn iterm2_encodes_supported_formats_as_one_bounded_osc_frame() {
        let limits = ImageLimits::default();
        for source in [png(2, 1, 5), jpeg(2, 1), gif(2, 1)] {
            let image = image(source);
            let command = ImageProtocolEncoder::new(ImageProtocol::Iterm2, limits.clone())
                .encode_place(image_id(11), &image, ImageLayout::new(2, 1).unwrap())
                .unwrap();
            let mut wire = Vec::new();
            command.write_to(&mut wire).unwrap();
            let wire = String::from_utf8(wire).unwrap();
            assert!(wire.starts_with("\x1b]1337;File=size="), "{wire:?}");
            assert!(wire
                .contains(";inline=1;doNotMoveCursor=1;width=2;height=1;preserveAspectRatio=1:"));
            assert!(wire.ends_with("\x1b\\"));
            assert_eq!(wire.matches("\x1b]1337;File=").count(), 1);
            assert_eq!(wire.matches("\x1b\\").count(), 1);
        }
        let webp = image(webp(2, 1));
        assert!(matches!(
            ImageProtocolEncoder::new(ImageProtocol::Iterm2, limits).encode_place(
                image_id(11),
                &webp,
                ImageLayout::new(2, 1).unwrap()
            ),
            Err(ImageError::UnsupportedFormatForProtocol)
        ));
    }

    #[test]
    fn registry_replace_delete_and_iterm2_limits_are_explicit() {
        let image = image(png(2, 2, 8));
        let layout = ImageLayout::new(2, 2).unwrap();
        let mut registry = ImageRegistry::default();
        let placed = registry.place().unwrap();
        let id = placed.id();
        assert_eq!(id.get(), 1);
        assert!(registry.is_live(id));
        let replace = registry.replace(id).unwrap();
        let kitty = ImageProtocolEncoder::new(ImageProtocol::Kitty, ImageLimits::default());
        let command = kitty.encode_replace(replace.id(), &image, layout).unwrap();
        let mut wire = Vec::new();
        command.write_to(&mut wire).unwrap();
        let wire = String::from_utf8(wire).unwrap();
        assert!(wire.starts_with("\x1b_Ga=d,d=I,i=1,q=2\x1b\\\x1b_Ga=T,"));

        let deleted = registry.delete(id).unwrap();
        let delete = kitty.encode_delete(deleted.id()).unwrap();
        let mut delete_wire = Vec::new();
        delete.write_to(&mut delete_wire).unwrap();
        assert_eq!(delete_wire, b"\x1b_Ga=d,d=I,i=1,q=2\x1b\\");
        let tiny_output = ImageProtocolEncoder::new(
            ImageProtocol::Kitty,
            ImageLimits::default()
                .with_max_encoded_output_bytes(1)
                .unwrap(),
        );
        assert!(matches!(
            tiny_output.encode_delete(id),
            Err(ImageError::EncodedOutputTooLarge)
        ));
        assert_eq!(registry.delete(id), Err(ImageError::StaleImageId));
        assert_eq!(registry.replace(id), Err(ImageError::StaleImageId));
        assert_eq!(registry.place().unwrap().id().get(), 2);

        let iterm = ImageProtocolEncoder::new(ImageProtocol::Iterm2, ImageLimits::default());
        assert!(matches!(
            iterm.encode_replace(id, &image, layout),
            Err(ImageError::UnsupportedOperation)
        ));
        assert!(matches!(
            iterm.encode_delete(id),
            Err(ImageError::UnsupportedOperation)
        ));

        let mut final_id = ImageRegistry {
            next: u32::MAX,
            live: BTreeSet::new(),
        };
        assert_eq!(final_id.place().unwrap().id().get(), u32::MAX);
        assert_eq!(final_id.place(), Err(ImageError::ImageIdExhausted));
    }

    #[test]
    fn registry_bounds_concurrent_live_bookkeeping() {
        let mut registry = ImageRegistry::new();
        for _ in 0..HARD_MAX_LIVE_IMAGES {
            registry.place().unwrap();
        }
        assert_eq!(registry.place(), Err(ImageError::TooManyLiveImages));
        registry.delete(image_id(1)).unwrap();
        assert_eq!(registry.place().unwrap().id().get(), 4_097);
    }

    #[test]
    fn image_anchor_round_trips_without_graphics_payload() {
        let anchor = ImageAnchor::new(
            ImageProtocol::Kitty,
            image_id(41),
            ImageLayout::new(7, 3).unwrap(),
        );
        let marker = anchor.marker();
        assert_eq!(ImageAnchor::parse(&marker), Some(anchor));
        assert_eq!(
            ImageAnchor::parse_all(&format!("before{marker}after")),
            vec![anchor]
        );
        assert!(!marker.contains("IDAT"));
        assert!(!marker.contains("_G"));
        assert!(ImageAnchor::parse("\x1bP+ygg-image;v=2,p=kitty,i=41,c=7,r=3\x1b\\").is_none());
    }

    #[test]
    fn planner_reserves_rows_without_protocol_or_payload_in_semantic_text() {
        let png_image = image(png(16, 33, 16));
        let planner = ImagePlanner::new(
            ImageCapabilities::forced(
                Some(ImageProtocol::Kitty),
                Some(CellPixelSize::new(8, 16).unwrap()),
            ),
            ImageLimits::default(),
        );
        let plan = planner
            .plan_place(
                image_id(41),
                &png_image,
                ImageViewport::new(40, 20, Some(CellPixelSize::new(8, 16).unwrap())).unwrap(),
            )
            .unwrap();
        assert_eq!(plan.reservation().rows(), 3);
        let copy = plan.semantic_copy_text();
        assert!(!copy.contains('\u{1b}'));
        assert!(!copy.contains("_G"));
        assert!(!copy.contains("1337"));
        assert!(!copy.contains("IDAT"));
        assert!(plan.terminal_command().is_some());
        let mut protocol = Vec::new();
        plan.write_protocol_to(&mut protocol).unwrap();
        assert!(protocol.starts_with(b"\x1b_G"));

        let plain = ImagePlanner::new(
            ImageCapabilities::forced(None, None),
            ImageLimits::default(),
        )
        .plan_place(
            image_id(42),
            &png_image,
            ImageViewport::new(40, 20, None).unwrap(),
        )
        .unwrap();
        assert_eq!(
            plain.fallback_reason(),
            Some(ImageFallbackReason::UnsupportedTerminal)
        );
        assert!(plain.terminal_command().is_none());
        assert!(plain.semantic_copy_text().starts_with("[image: PNG"));
        assert!(!plain.semantic_copy_text().contains('\u{1b}'));

        let jpeg = image(jpeg(2, 1));
        let kitty_fallback = planner
            .plan_place(
                image_id(43),
                &jpeg,
                ImageViewport::new(40, 20, None).unwrap(),
            )
            .unwrap();
        assert_eq!(
            kitty_fallback.fallback_reason(),
            Some(ImageFallbackReason::UnsupportedFormat)
        );
        let webp = image(webp(2, 1));
        let iterm_fallback = ImagePlanner::new(
            ImageCapabilities::forced(Some(ImageProtocol::Iterm2), None),
            ImageLimits::default(),
        )
        .plan_place(
            image_id(44),
            &webp,
            ImageViewport::new(40, 20, None).unwrap(),
        )
        .unwrap();
        assert_eq!(
            iterm_fallback.fallback_reason(),
            Some(ImageFallbackReason::UnsupportedFormat)
        );
    }

    #[test]
    fn deterministic_property_style_headers_remain_bounded() {
        let cell = CellPixelSize::new(7, 13).unwrap();
        let mut seed = 0x5eed_f00d_u32;
        for _ in 0..128 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let width = 1 + seed % 400;
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let height = 1 + seed % 400;
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let payload = usize::try_from(1 + seed % 64).unwrap();
            let image = image(png(width, height, payload));
            let viewport = ImageViewport::new(80, 30, Some(cell)).unwrap();
            let layout = ImageLayout::fit(image.dimensions(), viewport).unwrap();
            assert!((1..=MAX_IMAGE_CELL_COLUMNS).contains(&layout.columns()));
            assert!((1..=MAX_RESERVED_IMAGE_ROWS).contains(&layout.rows()));
            let command = ImageProtocolEncoder::new(ImageProtocol::Kitty, ImageLimits::default())
                .encode_place(image_id(100), &image, layout)
                .unwrap();
            let mut wire = Vec::new();
            command.write_to(&mut wire).unwrap();
            assert_eq!(wire.len(), command.encoded_len());
            assert!(wire.len() <= ImageLimits::default().max_encoded_output_bytes());
        }
    }

    #[test]
    fn debug_and_errors_never_echo_raw_payload_or_hostile_metadata() {
        let image = image(png(2, 2, 32));
        let debug = format!("{image:?}");
        assert!(!debug.contains("IDAT"));
        assert!(!debug.contains("\u{1b}"));
        let error = ImageFilename::new("bad\u{1b}title").unwrap_err();
        assert!(!error.to_string().contains("title"));
        let command = ImageProtocolEncoder::new(ImageProtocol::Kitty, ImageLimits::default())
            .encode_place(image_id(5), &image, ImageLayout::new(1, 1).unwrap())
            .unwrap();
        assert!(!format!("{command:?}").contains("IDAT"));
    }
}
