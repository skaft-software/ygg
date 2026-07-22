/// Editor component interface. Port of src/editor-component.ts (74 lines).
use crate::tui::Component;

/// Components that replace the main editor must implement this trait.
/// Extends Component with editor-specific behavior.
pub trait EditorComponent: Component {
    /// Get the current text content.
    fn get_text(&self) -> String;

    /// Set the text content.
    fn set_text(&mut self, text: &str);

    /// Called when the user submits (Enter).
    fn on_submit(&mut self, text: &str);

    /// Called when content changes.
    fn on_change(&mut self, text: &str);
}
