/// Widget components — implemented widgets for the TUI framework.
pub mod cancellable_loader;
pub mod editor;
pub mod image;
pub mod input;
pub mod loader;
pub mod markdown;
pub mod panel;
pub mod select_list;
pub mod settings_list;
pub mod spacer;
pub mod text;
pub mod truncated_text;

// Re-exports
pub use cancellable_loader::CancellableLoader;
pub use editor::{Editor, EditorOptions, EditorTheme};
pub use image::{Image, ImageOptions, ImageTheme};
pub use input::Input;
pub use loader::{Loader, LoaderIndicatorOptions};
pub use markdown::{Markdown, MarkdownOptions, MarkdownTheme, StreamingMarkdownWidget};
pub use panel::Panel;
pub use select_list::{
    SelectItem, SelectItemHandler, SelectList, SelectListLayoutOptions, SelectListTheme,
    TruncatePrimary,
};
pub use settings_list::{SettingItem, SettingsList, SettingsListTheme};
pub use spacer::Spacer;
pub use text::{RichText, Text};
pub use truncated_text::TruncatedText;
