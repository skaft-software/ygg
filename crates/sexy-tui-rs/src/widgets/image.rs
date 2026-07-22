use crate::tui::Component;

pub struct ImageTheme {
    pub fallback_color: Box<dyn Fn(&str) -> String>,
}

pub struct ImageOptions {
    pub max_width_cells: Option<u32>,
    pub max_height_cells: Option<u32>,
    pub filename: Option<String>,
}

/// Image widget for Kitty/iTerm2 inline images.
pub struct Image {
    base64_data: String,
    mime_type: String,
    opts: ImageOptions,
}

impl Image {
    pub fn new(base64_data: &str, mime_type: &str, _theme: ImageTheme, opts: ImageOptions) -> Self {
        Image {
            base64_data: base64_data.to_string(),
            mime_type: mime_type.to_string(),
            opts,
        }
    }
}

impl Component for Image {
    fn render(&self, width: u16) -> Vec<String> {
        if width == 0 {
            return vec![String::new()];
        }
        let width = u32::from(width);
        let render_opts = crate::terminal_image::ImageRenderOptions {
            max_width_cells: Some(self.opts.max_width_cells.unwrap_or(width).min(width)),
            max_height_cells: self.opts.max_height_cells,
            filename: self.opts.filename.clone(),
        };
        let output =
            crate::terminal_image::render_image(&self.base64_data, &self.mime_type, &render_opts);
        if crate::terminal_image::is_image_line(&output) || output.starts_with("\x1b]1337;") {
            vec![output]
        } else {
            let capabilities = crate::terminal_image::get_capabilities();
            let glyphs = crate::GlyphSet::for_capabilities(capabilities);
            vec![crate::utils::truncate_to_width(
                &output,
                width as usize,
                Some(glyphs.ellipsis),
            )]
        }
    }

    fn invalidate(&mut self) {}
}
