use crate::capabilities::TerminalCapabilities;
use crate::glyphs::GlyphSet;
use crate::sanitize::sanitize_line;
use crate::tui::Component;
use crate::utils::{truncate_to_width, visible_width};

const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
const PRIMARY_COLUMN_GAP: usize = 2;
const MIN_DESCRIPTION_WIDTH: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

pub struct SelectListTheme {
    pub selected_prefix: Box<dyn Fn(&str) -> String>,
    pub selected_text: Box<dyn Fn(&str) -> String>,
    pub description: Box<dyn Fn(&str) -> String>,
    pub scroll_info: Box<dyn Fn(&str) -> String>,
    pub no_match: Box<dyn Fn(&str) -> String>,
}

pub type TruncatePrimary = Box<dyn Fn(&str, usize, usize, &SelectItem, bool) -> String>;
pub type SelectItemHandler = Box<dyn FnMut(&SelectItem)>;

#[derive(Default)]
pub struct SelectListLayoutOptions {
    pub min_primary_column_width: Option<usize>,
    pub max_primary_column_width: Option<usize>,
    pub truncate_primary: Option<TruncatePrimary>,
}

pub struct SelectList {
    items: Vec<SelectItem>,
    filtered: Vec<usize>,
    selected: usize,
    max_visible: usize,
    theme: SelectListTheme,
    layout: SelectListLayoutOptions,
    capabilities: TerminalCapabilities,
    pub on_select: Option<SelectItemHandler>,
    pub on_cancel: Option<Box<dyn FnMut()>>,
    pub on_selection_change: Option<SelectItemHandler>,
}

impl SelectList {
    pub fn new(items: Vec<SelectItem>, max_visible: usize, theme: SelectListTheme) -> Self {
        Self::with_layout(
            items,
            max_visible,
            theme,
            SelectListLayoutOptions::default(),
        )
    }

    pub fn with_layout(
        items: Vec<SelectItem>,
        max_visible: usize,
        theme: SelectListTheme,
        layout: SelectListLayoutOptions,
    ) -> Self {
        Self::with_layout_and_capabilities(
            items,
            max_visible,
            theme,
            layout,
            crate::terminal_image::get_capabilities(),
        )
    }

    pub fn with_capabilities(
        items: Vec<SelectItem>,
        max_visible: usize,
        theme: SelectListTheme,
        capabilities: TerminalCapabilities,
    ) -> Self {
        Self::with_layout_and_capabilities(
            items,
            max_visible,
            theme,
            SelectListLayoutOptions::default(),
            capabilities,
        )
    }

    pub fn with_layout_and_capabilities(
        items: Vec<SelectItem>,
        max_visible: usize,
        theme: SelectListTheme,
        layout: SelectListLayoutOptions,
        capabilities: TerminalCapabilities,
    ) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            items,
            filtered,
            selected: 0,
            max_visible,
            theme,
            layout,
            capabilities,
            on_select: None,
            on_cancel: None,
            on_selection_change: None,
        }
    }

    pub fn set_filter(&mut self, filter: &str) {
        let filter = filter.to_lowercase();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                item.value
                    .to_lowercase()
                    .starts_with(&filter)
                    .then_some(index)
            })
            .collect();
        self.selected = 0;
    }

    pub fn set_selected_index(&mut self, index: usize) {
        self.selected = index.min(self.filtered.len().saturating_sub(1));
    }

    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.filtered
            .get(self.selected)
            .map(|&index| &self.items[index])
    }

    fn primary_bounds(&self) -> (usize, usize) {
        let raw_min = self
            .layout
            .min_primary_column_width
            .or(self.layout.max_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let raw_max = self
            .layout
            .max_primary_column_width
            .or(self.layout.min_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        (raw_min.min(raw_max).max(1), raw_min.max(raw_max).max(1))
    }

    fn primary_width(&self) -> usize {
        let widest = self.filtered.iter().fold(0, |widest, &index| {
            widest.max(visible_width(self.display_value(&self.items[index])) + PRIMARY_COLUMN_GAP)
        });
        let (min, max) = self.primary_bounds();
        widest.clamp(min, max)
    }

    fn display_value<'b>(&self, item: &'b SelectItem) -> &'b str {
        if item.label.is_empty() {
            &item.value
        } else {
            &item.label
        }
    }

    fn truncate_primary(
        &self,
        item: &SelectItem,
        selected: bool,
        max_width: usize,
        column_width: usize,
    ) -> String {
        let display = self.display_value(item);
        let result = self.layout.truncate_primary.as_ref().map_or_else(
            || truncate_to_width(display, max_width, Some("")),
            |truncate| truncate(display, max_width, column_width, item, selected),
        );
        truncate_to_width(&result, max_width, Some(""))
    }

    fn render_item(
        &self,
        item: &SelectItem,
        selected: bool,
        width: usize,
        description: Option<&str>,
        primary_width: usize,
    ) -> String {
        let glyphs = GlyphSet::for_capabilities(self.capabilities);
        let raw_prefix = if selected {
            format!("{} ", glyphs.chevron)
        } else {
            "  ".into()
        };
        let prefix_width = visible_width(&raw_prefix);
        let prefix = if selected && !self.capabilities.plain {
            (self.theme.selected_prefix)(&raw_prefix)
        } else {
            raw_prefix
        };

        if let Some(description) = description.filter(|_| width > 40) {
            let column_width = primary_width
                .min(width.saturating_sub(prefix_width + 4))
                .max(1);
            let max_primary = column_width.saturating_sub(PRIMARY_COLUMN_GAP).max(1);
            let primary = self.truncate_primary(item, selected, max_primary, column_width);
            let spacing = " ".repeat(column_width.saturating_sub(visible_width(&primary)).max(1));
            let description_start = prefix_width + visible_width(&primary) + spacing.len();
            let remaining = width.saturating_sub(description_start + 2);
            if remaining > MIN_DESCRIPTION_WIDTH {
                let description = truncate_to_width(description, remaining, Some(""));
                if selected && !self.capabilities.plain {
                    return (self.theme.selected_text)(&format!(
                        "{prefix}{primary}{spacing}{description}"
                    ));
                }
                let styled_description = if self.capabilities.plain {
                    format!("{spacing}{description}")
                } else {
                    (self.theme.description)(&format!("{spacing}{description}"))
                };
                return format!("{prefix}{primary}{styled_description}");
            }
        }

        let max_width = width.saturating_sub(prefix_width + 2).max(1);
        let primary = self.truncate_primary(item, selected, max_width, max_width);
        if selected && !self.capabilities.plain {
            (self.theme.selected_text)(&format!("{prefix}{primary}"))
        } else {
            format!("{prefix}{primary}")
        }
    }
}

impl Component for SelectList {
    fn render(&self, width: u16) -> Vec<String> {
        if self.filtered.is_empty() {
            let text = if self.capabilities.plain {
                "  No matching commands".into()
            } else {
                (self.theme.no_match)("  No matching commands")
            };
            return vec![truncate_to_width(&text, usize::from(width), Some(""))];
        }

        let start = self
            .selected
            .saturating_sub(self.max_visible / 2)
            .min(self.filtered.len().saturating_sub(self.max_visible));
        let end = (start + self.max_visible).min(self.filtered.len());
        let primary_width = self.primary_width();
        let mut lines = Vec::new();
        for index in start..end {
            let item = &self.items[self.filtered[index]];
            let safe_label = sanitize_line(&item.label, !self.capabilities.unicode).into_owned();
            let safe_description = item.description.as_ref().map(|description| {
                let normalized = description.replace(['\r', '\n'], " ");
                sanitize_line(normalized.trim(), !self.capabilities.unicode).into_owned()
            });
            let safe = SelectItem {
                value: item.value.clone(),
                label: safe_label,
                description: safe_description.clone(),
            };
            lines.push(self.render_item(
                &safe,
                index == self.selected,
                usize::from(width),
                safe_description.as_deref(),
                primary_width,
            ));
        }
        if start > 0 || end < self.filtered.len() {
            let info = truncate_to_width(
                &format!("  ({}/{})", self.selected + 1, self.filtered.len()),
                usize::from(width).saturating_sub(2),
                Some(""),
            );
            lines.push(if self.capabilities.plain {
                info
            } else {
                (self.theme.scroll_info)(&info)
            });
        }
        if self.capabilities.plain {
            for line in &mut lines {
                *line = crate::utils::strip_terminal_sequences(line);
            }
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let bindings = crate::keybindings::get_keybindings();
        if bindings.matches(data, "tui.select.up") && !self.filtered.is_empty() {
            self.selected = if self.selected == 0 {
                self.filtered.len() - 1
            } else {
                self.selected - 1
            };
            let item = self.selected_item().cloned();
            if let (Some(callback), Some(item)) = (&mut self.on_selection_change, item) {
                callback(&item);
            }
        } else if bindings.matches(data, "tui.select.down") && !self.filtered.is_empty() {
            self.selected = if self.selected + 1 == self.filtered.len() {
                0
            } else {
                self.selected + 1
            };
            let item = self.selected_item().cloned();
            if let (Some(callback), Some(item)) = (&mut self.on_selection_change, item) {
                callback(&item);
            }
        } else if bindings.matches(data, "tui.select.confirm") {
            let item = self.selected_item().cloned();
            if let (Some(callback), Some(item)) = (&mut self.on_select, item) {
                callback(&item);
            }
        } else if bindings.matches(data, "tui.select.cancel") {
            if let Some(callback) = &mut self.on_cancel {
                callback();
            }
        }
    }

    fn invalidate(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Box<dyn Fn(&str) -> String> {
        Box::new(str::to_owned)
    }
    fn theme() -> SelectListTheme {
        SelectListTheme {
            selected_prefix: identity(),
            selected_text: identity(),
            description: identity(),
            scroll_info: identity(),
            no_match: identity(),
        }
    }

    fn item(label: &str, description: &str) -> SelectItem {
        SelectItem {
            value: label.into(),
            label: label.into(),
            description: Some(description.into()),
        }
    }

    #[test]
    fn pi_normalizes_multiline_descriptions_to_one_line() {
        let list = SelectList::new(
            vec![item("test", "Line one\nLine two\nLine three")],
            5,
            theme(),
        );
        let line = list.render(100).remove(0);
        assert!(!line.contains('\n'));
        assert!(line.contains("Line one Line two Line three"));
    }

    #[test]
    fn pi_aligns_descriptions_after_primary_truncation() {
        let list = SelectList::new(
            vec![
                item("short", "short description"),
                item(
                    "very-long-command-name-that-needs-truncation",
                    "long description",
                ),
            ],
            5,
            theme(),
        );
        let lines = list.render(80);
        assert_eq!(
            lines[0].find("short description"),
            lines[1].find("long description")
        );
    }

    #[test]
    fn pi_honors_primary_column_bounds_and_custom_truncation() {
        let layout = SelectListLayoutOptions {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(12),
            truncate_primary: Some(Box::new(|text, max, _, _, _| {
                if visible_width(text) <= max {
                    text.into()
                } else {
                    format!("{}…", &text[..max - 1])
                }
            })),
        };
        let list = SelectList::with_layout(
            vec![item("very-long-command", "first"), item("short", "second")],
            5,
            theme(),
            layout,
        );
        let lines = list.render(80);
        assert!(lines[0].contains('…'));
        let first = lines[0].find("first").unwrap();
        let second = lines[1].find("second").unwrap();
        assert_eq!(
            visible_width(&lines[0][..first]),
            visible_width(&lines[1][..second])
        );
    }

    #[test]
    fn plain_selection_is_ascii_safe_and_bounded() {
        let list = SelectList::with_capabilities(
            vec![SelectItem {
                value: "one".into(),
                label: "label\x1b]52;c;bad\x07".into(),
                description: Some("description".into()),
            }],
            5,
            theme(),
            TerminalCapabilities::plain(),
        );
        let lines = list.render(12);
        assert!(lines
            .iter()
            .all(|line| line.is_ascii() && !line.contains('\x1b') && visible_width(line) <= 12));
        assert!(lines[0].starts_with("> "));
    }
}
