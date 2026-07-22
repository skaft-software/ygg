use crate::tui::Component;

pub type PanelBackground = Box<dyn Fn(&str) -> String>;

/// Panel widget — container with padding and background color.
pub struct Panel {
    children: Vec<Box<dyn Component>>,
    padding_x: u16,
    padding_y: u16,
    bg_fn: Option<PanelBackground>,
}

impl Panel {
    pub fn new(padding_x: u16, padding_y: u16, bg_fn: Option<PanelBackground>) -> Self {
        Panel {
            children: Vec::new(),
            padding_x,
            padding_y,
            bg_fn,
        }
    }

    pub fn add_child(&mut self, child: Box<dyn Component>) {
        self.children.push(child);
    }

    pub fn set_bg_fn(&mut self, bg_fn: PanelBackground) {
        self.bg_fn = Some(bg_fn);
    }
}

impl Component for Panel {
    fn render(&self, width: u16) -> Vec<String> {
        let padding_x = self.padding_x.min(width / 2);
        let inner_width = width.saturating_sub(padding_x.saturating_mul(2));
        let spacer = " ".repeat(usize::from(padding_x));
        let top_bottom = vec!["".to_string(); self.padding_y as usize];
        let mut lines = top_bottom.clone();
        for child in &self.children {
            for line in child.render(inner_width) {
                let line =
                    crate::utils::truncate_to_width(&line, usize::from(inner_width), Some(""));
                let padded = format!("{spacer}{line}{spacer}");
                lines.push(if let Some(ref bg) = self.bg_fn {
                    bg(&padded)
                } else {
                    padded
                });
            }
        }
        lines.extend(top_bottom);
        lines
    }

    fn handle_input(&mut self, data: &str) {
        for child in &mut self.children {
            child.handle_input(data);
        }
    }

    fn handle_paste(&mut self, data: &str) {
        for child in &mut self.children {
            child.handle_paste(data);
        }
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}
