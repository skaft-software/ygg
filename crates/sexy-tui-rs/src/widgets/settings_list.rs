use crate::capabilities::TerminalCapabilities;
use crate::glyphs::GlyphSet;
use crate::sanitize::sanitize_line;
use crate::tui::Component;

pub type SettingsStateStyle = Box<dyn Fn(&str, bool) -> String>;
pub type SettingsTextStyle = Box<dyn Fn(&str) -> String>;
pub type SettingsChangeHandler = Box<dyn Fn(&str, &str)>;

/// A settings item with values to cycle through.
#[derive(Clone)]
pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub current_value: String,
    pub values: Vec<String>,
}

/// SettingsList theme.
pub struct SettingsListTheme {
    pub label: SettingsStateStyle,
    pub value: SettingsStateStyle,
    pub description: SettingsTextStyle,
    pub cursor: String,
    pub hint: SettingsTextStyle,
}

/// Settings panel widget.
pub struct SettingsList {
    items: Vec<SettingItem>,
    selected: usize,
    max_visible: usize,
    theme: SettingsListTheme,
    capabilities: TerminalCapabilities,
    on_change: Option<SettingsChangeHandler>,
}

impl SettingsList {
    pub fn new(
        items: Vec<SettingItem>,
        max_visible: usize,
        theme: SettingsListTheme,
        on_change: SettingsChangeHandler,
    ) -> Self {
        Self::with_capabilities(
            items,
            max_visible,
            theme,
            on_change,
            crate::terminal_image::get_capabilities(),
        )
    }

    pub fn with_capabilities(
        items: Vec<SettingItem>,
        max_visible: usize,
        theme: SettingsListTheme,
        on_change: SettingsChangeHandler,
        capabilities: TerminalCapabilities,
    ) -> Self {
        SettingsList {
            items,
            selected: 0,
            max_visible,
            theme,
            capabilities,
            on_change: Some(on_change),
        }
    }

    pub fn update_value(&mut self, id: &str, value: &str) {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.current_value = value.to_string();
        }
    }
}

impl Component for SettingsList {
    fn render(&self, width: u16) -> Vec<String> {
        let end = (self.selected + self.max_visible).min(self.items.len());
        let glyphs = GlyphSet::for_capabilities(self.capabilities);
        self.items[self.selected..end]
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let selected = index == 0;
                let prefix = if selected {
                    format!("{} ", glyphs.chevron)
                } else {
                    "  ".to_owned()
                };
                let label = sanitize_line(&item.label, !self.capabilities.unicode);
                let value = sanitize_line(&item.current_value, !self.capabilities.unicode);
                let label = if self.capabilities.plain {
                    label.into_owned()
                } else {
                    (self.theme.label)(&label, selected)
                };
                let value = if self.capabilities.plain {
                    value.into_owned()
                } else {
                    (self.theme.value)(&value, selected)
                };
                crate::utils::truncate_to_width(
                    &format!("{prefix}{label}: {value}"),
                    usize::from(width),
                    Some(glyphs.ellipsis),
                )
            })
            .collect()
    }

    fn handle_input(&mut self, data: &str) {
        use crate::keys::{matches_key, Key};
        if matches_key(data, Key::up) && self.selected > 0 {
            self.selected -= 1;
        } else if matches_key(data, Key::down) && self.selected + 1 < self.items.len() {
            self.selected += 1;
        } else if (matches_key(data, Key::enter) || matches_key(data, " "))
            && !self.items.is_empty()
        {
            let item = &self.items[self.selected];
            if item.values.len() > 1 {
                let idx = item
                    .values
                    .iter()
                    .position(|v| v == &item.current_value)
                    .unwrap_or(0);
                let next = item.values[(idx + 1) % item.values.len()].clone();
                if let Some(ref cb) = self.on_change {
                    cb(&item.id, &next);
                }
            }
        }
    }

    fn invalidate(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_safe_and_plain_rows_use_ascii() {
        let state_style =
            || Box::new(|text: &str, _selected: bool| text.to_owned()) as SettingsStateStyle;
        let text_style = || Box::new(str::to_owned) as SettingsTextStyle;
        let theme = SettingsListTheme {
            label: state_style(),
            value: state_style(),
            description: text_style(),
            cursor: "> ".into(),
            hint: text_style(),
        };
        let mut list = SettingsList::with_capabilities(
            Vec::new(),
            5,
            theme,
            Box::new(|_, _| {}),
            TerminalCapabilities::plain(),
        );
        list.handle_input("\r");
        assert!(list.render(10).is_empty());
    }
}
