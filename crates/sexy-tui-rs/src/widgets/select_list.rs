use crate::capabilities::TerminalCapabilities;
use crate::fuzzy::fuzzy_filter;
use crate::glyphs::GlyphSet;
use crate::sanitize::sanitize_line;
use crate::tui::Component;

/// A selectable item.
#[derive(Clone)]
pub struct SelectItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// Theme for SelectList.
pub struct SelectListTheme {
    pub selected_prefix: Box<dyn Fn(&str) -> String>,
    pub selected_text: Box<dyn Fn(&str) -> String>,
    pub description: Box<dyn Fn(&str) -> String>,
    pub scroll_info: Box<dyn Fn(&str) -> String>,
    pub no_match: Box<dyn Fn(&str) -> String>,
}

/// Interactive selection list widget.
pub struct SelectList {
    items: Vec<SelectItem>,
    filtered: Vec<usize>,
    selected: usize,
    filter: String,
    max_visible: usize,
    scroll_offset: usize,
    theme: SelectListTheme,
    capabilities: TerminalCapabilities,
}

impl SelectList {
    pub fn new(items: Vec<SelectItem>, max_visible: usize, theme: SelectListTheme) -> Self {
        Self::with_capabilities(
            items,
            max_visible,
            theme,
            crate::terminal_image::get_capabilities(),
        )
    }

    pub fn with_capabilities(
        items: Vec<SelectItem>,
        max_visible: usize,
        theme: SelectListTheme,
        capabilities: TerminalCapabilities,
    ) -> Self {
        let filtered: Vec<usize> = (0..items.len()).collect();
        SelectList {
            items,
            filtered,
            selected: 0,
            filter: String::new(),
            max_visible,
            scroll_offset: 0,
            theme,
            capabilities,
        }
    }

    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        let filter_str = self.filter.clone();
        // Collect labels first to avoid borrowing self in closure
        let labels: Vec<String> = self.items.iter().map(|item| item.label.clone()).collect();
        let indices: Vec<usize> = (0..labels.len()).collect();
        self.filtered = fuzzy_filter(&indices, &filter_str, |i| labels[*i].clone());
        self.selected = 0;
        self.scroll_offset = 0;
    }

    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.filtered.get(self.selected).map(|&i| &self.items[i])
    }
}

impl Component for SelectList {
    fn render(&self, width: u16) -> Vec<String> {
        let end = (self.scroll_offset + self.max_visible).min(self.filtered.len());
        let visible = &self.filtered[self.scroll_offset..end];
        let glyphs = GlyphSet::for_capabilities(self.capabilities);

        if visible.is_empty() {
            let text = if self.capabilities.plain {
                "No matches".to_owned()
            } else {
                (self.theme.no_match)("No matches")
            };
            let text =
                crate::utils::truncate_to_width(&text, usize::from(width), Some(glyphs.ellipsis));
            return vec![if self.capabilities.plain {
                crate::utils::strip_terminal_sequences(&text)
            } else {
                text
            }];
        }

        let mut lines: Vec<String> = visible
            .iter()
            .enumerate()
            .map(|(i, &idx)| {
                let item = &self.items[idx];
                let is_selected = self.scroll_offset + i == self.selected;
                let raw_prefix = if is_selected {
                    format!("{} ", glyphs.chevron)
                } else {
                    "  ".to_owned()
                };
                let safe_label = sanitize_line(&item.label, !self.capabilities.unicode);
                let prefix = if is_selected && !self.capabilities.plain {
                    (self.theme.selected_prefix)(&raw_prefix)
                } else {
                    raw_prefix
                };
                let label = if is_selected && !self.capabilities.plain {
                    (self.theme.selected_text)(&safe_label)
                } else {
                    safe_label.into_owned()
                };
                let line = if let Some(description) = &item.description {
                    let description = sanitize_line(description, !self.capabilities.unicode);
                    let description = if self.capabilities.plain {
                        description.into_owned()
                    } else {
                        (self.theme.description)(&description)
                    };
                    let separator = if self.capabilities.unicode {
                        " — "
                    } else {
                        " - "
                    };
                    format!("{prefix}{label}{separator}{description}")
                } else {
                    format!("{prefix}{label}")
                };
                crate::utils::truncate_to_width(&line, usize::from(width), Some(glyphs.ellipsis))
            })
            .collect();

        if self.filtered.len() > self.max_visible {
            let info = format!("[{}/{}]", self.selected + 1, self.filtered.len());
            let info = if self.capabilities.plain {
                info
            } else {
                (self.theme.scroll_info)(&info)
            };
            lines.push(crate::utils::truncate_to_width(
                &info,
                usize::from(width),
                Some(glyphs.ellipsis),
            ));
        }
        if self.capabilities.plain {
            for line in &mut lines {
                *line = crate::utils::strip_terminal_sequences(line);
            }
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        use crate::keys::{matches_key, Key};
        if matches_key(data, Key::up) && self.selected > 0 {
            self.selected -= 1;
            if self.selected < self.scroll_offset {
                self.scroll_offset = self.selected;
            }
        } else if matches_key(data, Key::down) && self.selected + 1 < self.filtered.len() {
            self.selected += 1;
            if self.selected >= self.scroll_offset + self.max_visible {
                self.scroll_offset = self.selected + 1 - self.max_visible;
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

    #[test]
    fn plain_selection_is_ascii_safe_and_bounded() {
        let theme = SelectListTheme {
            selected_prefix: identity(),
            selected_text: identity(),
            description: identity(),
            scroll_info: identity(),
            no_match: identity(),
        };
        let list = SelectList::with_capabilities(
            vec![SelectItem {
                value: "one".into(),
                label: "label\x1b]52;c;bad\x07".into(),
                description: Some("description".into()),
            }],
            5,
            theme,
            TerminalCapabilities::plain(),
        );
        let lines = list.render(12);
        assert!(lines.iter().all(|line| {
            line.is_ascii() && !line.contains('\x1b') && crate::utils::visible_width(line) <= 12
        }));
        assert!(lines[0].starts_with("> "));
    }
}
