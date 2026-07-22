//! Platform-specific modifier key detection.
//!
//! In the TypeScript version, this loads a native .node addon on macOS
//! to detect modifier key state. In Rust, crossterm's KeyEvent already
//! includes modifier state, so this module is a compatibility shim.
//!
//! Returns false on non-macOS platforms (platform detection not supported).

/// Modifier key identifiers.
pub type ModifierKey = &'static str;

pub const MODIFIER_SHIFT: ModifierKey = "shift";
pub const MODIFIER_COMMAND: ModifierKey = "command";
pub const MODIFIER_CONTROL: ModifierKey = "control";
pub const MODIFIER_OPTION: ModifierKey = "option";

/// Check if a native modifier key is currently pressed.
///
/// On macOS, this would query the system for the current modifier state.
/// Currently returns false — crossterm's KeyEvent.modifiers should be
/// used instead for key event modifier detection.
pub fn is_native_modifier_pressed(_key: ModifierKey) -> bool {
    // TODO: Implement macOS modifier detection via CGEventSource if needed.
    // For now, crossterm's KeyEvent provides modifier state per-event,
    // which covers the primary use case (shift+enter detection, etc.).
    false
}
