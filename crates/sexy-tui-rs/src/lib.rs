//! sexy-tui-rs — Rust port of @earendil-works/pi-tui
//!
//! A minimal terminal UI framework with differential rendering,
//! synchronized output, and an enhanced declarative theming system.
//!
//! Forked and ported to Rust by @achuthanmukundan00.

pub mod autocomplete;
pub mod capabilities;
pub mod editor_component;
pub mod fuzzy;
pub mod glyphs;
pub mod keybindings;
pub mod keys;
pub mod kill_ring;
pub mod live;
pub mod native_modifiers;
pub mod rich_text;
pub mod sanitize;
pub mod stdin_buffer;
pub mod style;
pub mod terminal;
pub mod terminal_colors;
pub mod terminal_image;
pub mod theme;
pub mod tui;
pub mod undo_stack;
pub mod utils;
pub mod widgets;
pub mod width;
pub mod word_navigation;

// Re-exports matching the TS src/index.ts public API
pub use autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CombinedAutocompleteProvider,
    SlashCommand,
};
pub use capabilities::{
    CapabilityOverrides, CapabilityProbe, ColorDepth, SupportLevel, TerminalCapabilities,
    TerminalSize,
};
pub use editor_component::EditorComponent;
pub use fuzzy::{fuzzy_filter, fuzzy_match, FuzzyMatch};
pub use glyphs::GlyphSet;
pub use keybindings::{
    get_keybindings, set_keybindings, Keybinding, KeybindingConflict, KeybindingDefinition,
    KeybindingDefinitions, Keybindings, KeybindingsConfig, KeybindingsManager, TUI_KEYBINDINGS,
};
pub use keys::{
    decode_kitty_printable, decode_kitty_text, is_key_release, is_key_repeat,
    is_kitty_protocol_active, matches_key, parse_key, set_kitty_protocol_active, Key, KeyEventType,
};
pub use live::{
    LiveContent, LiveRegion, NodeHandle, NodeId, NodeState, PlainEvent, PlainEventKind,
    RenderUpdate,
};
pub use rich_text::diff::{DiffLine, DiffLineKind, DiffRenderOptions, UnifiedDiff};
pub use rich_text::markdown::parse as parse_markdown;
pub use rich_text::render::{
    CodeOverflow, RenderOptions, RenderedDocument, RenderedLine, RichRenderer, SyntaxCacheStats,
};
pub use rich_text::stream::{
    StreamingMarkdown, StreamingRenderCache, StreamingStats, MAX_UNSTABLE_PARSE_BYTES,
};
pub use rich_text::{
    Block, CodeBlock, DetailBlock, Document, Inline, List, ListItem, ListKind, StatusKind,
    StyledSpan, Table, TableAlignment, TableCell,
};
pub use sanitize::{
    safe_hyperlink, sanitize_line, sanitize_text, ControlPictures, SafeUrl, SanitizeOptions,
};
pub use stdin_buffer::{StdinBuffer, StdinBufferOptions};
pub use style::{BlockRole, BlockStyle, Color, TextAttributes, TextRole, TextStyle};
pub use terminal::{key_text, ProcessTerminal, Terminal, TerminalInput};
pub use terminal_colors::{parse_osc11_background_color, RgbColor};
pub use terminal_image::{
    allocate_image_id, calculate_image_rows, delete_all_kitty_images, delete_kitty_image,
    detect_capabilities, encode_iterm2, encode_kitty, get_capabilities, get_cell_dimensions,
    get_gif_dimensions, get_image_dimensions, get_jpeg_dimensions, get_png_dimensions,
    get_webp_dimensions, hyperlink, image_fallback, is_image_line, render_image,
    reset_capabilities_cache, set_capabilities, set_cell_dimensions, CellDimensions,
    ImageDimensions, ImageProtocol, ImageRenderOptions,
};
pub use theme::capability::CapabilityTier;
pub use theme::Theme;
pub use tui::{
    Component, Container, Focusable, FrameUpdate, OverlayAnchor, OverlayHandle, OverlayMargin,
    OverlayOptions, OverlayUnfocusOptions, CURSOR_MARKER, TUI,
};
pub use utils::{
    strip_terminal_sequences, terminal_tokens, truncate_to_width, visible_width,
    wrap_text_with_ansi, TerminalToken,
};
pub use widgets::{
    CancellableLoader, Editor, EditorOptions, EditorTheme, Image, ImageOptions, ImageTheme, Input,
    Loader, LoaderIndicatorOptions, Markdown, MarkdownOptions, MarkdownTheme, Panel, SelectItem,
    SelectList, SelectListTheme, SettingItem, SettingsList, SettingsListTheme, Spacer, Text,
    TruncatedText,
};
pub use width::{display_width, AmbiguousWidth, WidthPolicy};

/// Deprecated alias — use `Panel` instead.
#[deprecated(
    since = "0.1.1",
    note = "Renamed to Panel to avoid shadowing std::boxed::Box"
)]
pub type Box = Panel;
