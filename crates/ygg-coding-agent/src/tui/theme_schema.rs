#![allow(missing_docs)]

use std::collections::BTreeMap;

use anyhow::{bail, Context};
use sexy_tui_rs::{display_width, Color};
use toml::Value;

use super::theme::TerminalBackground;

pub const MAX_THEME_BYTES: u64 = 256 * 1024;
const MAX_THEME_ROLES: usize = 256;
const MAX_THEME_GLYPHS: usize = 128;
const MAX_THEME_SURFACES: usize = 16;
const MAX_TOKEN_VALUE_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeMetadata {
    pub name: String,
    pub description: String,
    pub author: String,
    pub version: String,
    pub terminal: String,
    /// Rebalance configured RGB foregrounds/surfaces for the detected terminal
    /// profile. This is opt-in so third-party themes retain exact color values.
    pub adaptive: bool,
}

impl Default for ThemeMetadata {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            author: String::new(),
            version: "1".to_owned(),
            terminal: "light-dark".to_owned(),
            adaptive: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeDensity {
    Compact,
    #[default]
    Comfortable,
    Airy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeLayout {
    pub density: ThemeDensity,
    pub show_header: bool,
    pub show_footer: bool,
    pub show_status_line: bool,
    pub show_tool_duration: bool,
    pub show_reasoning: bool,
    pub show_panel_borders: bool,
    pub transcript_inset: u16,
    pub composer_padding: u16,
    pub narrow_breakpoint: u16,
    pub narrow_show_header: bool,
    pub narrow_show_footer: bool,
    pub narrow_show_status_line: bool,
    pub narrow_show_tool_duration: bool,
    pub narrow_show_reasoning: bool,
    pub narrow_show_panel_borders: bool,
}

impl Default for ThemeLayout {
    fn default() -> Self {
        Self {
            density: ThemeDensity::Comfortable,
            // Preserve the compiled theme's deliberately sparse shell. Named
            // themes opt into the identity header explicitly, while an
            // extension header contribution can still create the surface.
            show_header: false,
            show_footer: true,
            show_status_line: true,
            show_tool_duration: true,
            show_reasoning: true,
            show_panel_borders: true,
            transcript_inset: 2,
            composer_padding: 1,
            narrow_breakpoint: 64,
            narrow_show_header: false,
            narrow_show_footer: true,
            narrow_show_status_line: true,
            narrow_show_tool_duration: true,
            narrow_show_reasoning: true,
            narrow_show_panel_borders: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedThemeLayout {
    pub density: ThemeDensity,
    pub narrow: bool,
    pub show_header: bool,
    pub show_footer: bool,
    pub show_status_line: bool,
    pub show_tool_duration: bool,
    pub show_reasoning: bool,
    pub show_panel_borders: bool,
    pub transcript_inset: u16,
    pub composer_padding: u16,
}

impl ThemeLayout {
    pub fn resolve(&self, width: u16) -> ResolvedThemeLayout {
        let narrow = width < self.narrow_breakpoint;
        ResolvedThemeLayout {
            density: self.density,
            narrow,
            show_header: if narrow {
                self.narrow_show_header
            } else {
                self.show_header
            },
            show_footer: if narrow {
                self.narrow_show_footer
            } else {
                self.show_footer
            },
            show_status_line: if narrow {
                self.narrow_show_status_line
            } else {
                self.show_status_line
            },
            show_tool_duration: if narrow {
                self.narrow_show_tool_duration
            } else {
                self.show_tool_duration
            },
            show_reasoning: if narrow {
                self.narrow_show_reasoning
            } else {
                self.show_reasoning
            },
            show_panel_borders: if narrow {
                self.narrow_show_panel_borders
            } else {
                self.show_panel_borders
            },
            transcript_inset: self.transcript_inset,
            composer_padding: self.composer_padding,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeSurfaceChrome {
    #[default]
    Plain,
    Rail,
    Card,
    Band,
    Rule,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeSurfaceHeading {
    #[default]
    None,
    Inline,
    Tab,
    Overline,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeSurfaceWidth {
    #[default]
    Full,
    Content,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeSurfaceAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// A deliberately bounded layout recipe for one semantic transcript surface.
/// Theme files choose among typed contribution points; they never inject
/// terminal bytes or application callbacks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThemeSurface {
    pub chrome: ThemeSurfaceChrome,
    pub heading: ThemeSurfaceHeading,
    pub label: Option<String>,
    pub padding: u16,
    pub width: ThemeSurfaceWidth,
    pub align: ThemeSurfaceAlign,
    pub max_width: Option<u16>,
    pub narrow_chrome: Option<ThemeSurfaceChrome>,
    pub narrow_heading: Option<ThemeSurfaceHeading>,
    pub narrow_label: Option<String>,
    pub narrow_padding: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedThemeSurface<'a> {
    pub chrome: ThemeSurfaceChrome,
    pub heading: ThemeSurfaceHeading,
    pub label: Option<&'a str>,
    pub padding: u16,
    pub width: ThemeSurfaceWidth,
    pub align: ThemeSurfaceAlign,
    pub max_width: Option<u16>,
}

impl ThemeSurface {
    pub fn resolve(&self, narrow: bool) -> ResolvedThemeSurface<'_> {
        if !narrow {
            return ResolvedThemeSurface {
                chrome: self.chrome,
                heading: self.heading,
                label: self.label.as_deref(),
                padding: self.padding,
                width: self.width,
                align: self.align,
                max_width: self.max_width,
            };
        }

        // Safe structural fallback when a theme does not spell out its narrow
        // form. Cards keep a rail, while full-width bands and rules return the
        // user's terminal canvas. Explicit narrow fields always win.
        let chrome = self.narrow_chrome.unwrap_or(match self.chrome {
            ThemeSurfaceChrome::Card => ThemeSurfaceChrome::Rail,
            ThemeSurfaceChrome::Band | ThemeSurfaceChrome::Rule => ThemeSurfaceChrome::Plain,
            other => other,
        });
        let heading = self.narrow_heading.unwrap_or(match self.heading {
            ThemeSurfaceHeading::Tab | ThemeSurfaceHeading::Overline => ThemeSurfaceHeading::Inline,
            other => other,
        });
        ResolvedThemeSurface {
            chrome,
            heading,
            label: self.narrow_label.as_deref().or(self.label.as_deref()),
            padding: self.narrow_padding.unwrap_or(self.padding.min(1)),
            width: self.width,
            align: self.align,
            max_width: self.max_width,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct RoleStyleSpec {
    pub foreground: Option<String>,
    pub background: Option<String>,
    pub bold: Option<bool>,
    pub dim: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strikethrough: Option<bool>,
    pub inverse: Option<bool>,
    pub adaptive: Option<bool>,
}

#[derive(Clone, Debug)]
pub(super) struct ParsedTheme {
    pub metadata: ThemeMetadata,
    pub tokens: BTreeMap<String, String>,
    pub roles: BTreeMap<String, RoleStyleSpec>,
    pub glyphs: BTreeMap<String, String>,
    pub ascii_glyphs: BTreeMap<String, String>,
    pub surfaces: BTreeMap<String, ThemeSurface>,
    pub layout: ThemeLayout,
}

pub(super) fn parse_theme(
    source: &str,
    source_name: &str,
    background: TerminalBackground,
) -> anyhow::Result<ParsedTheme> {
    if source.len() as u64 > MAX_THEME_BYTES {
        bail!(
            "theme {source_name} is {} bytes; the limit is {MAX_THEME_BYTES}",
            source.len()
        );
    }
    let root: Value =
        toml::from_str(source).with_context(|| format!("invalid theme {source_name}"))?;
    let Value::Table(mut effective) = root else {
        bail!("theme {source_name} must contain a TOML table");
    };

    let variants = effective.remove("variants");
    if let Some(Value::Table(variants)) = variants {
        if let Some(Value::Table(universal)) = variants.get("universal") {
            merge_tables(&mut effective, universal);
        }
        let selected = match background {
            TerminalBackground::Dark => "dark",
            TerminalBackground::Light => "light",
            TerminalBackground::Unknown => "unknown",
        };
        if let Some(Value::Table(variant)) = variants.get(selected) {
            merge_tables(&mut effective, variant);
        }
    }

    reject_control_strings(&Value::Table(effective.clone()), source_name)?;
    let metadata = parse_metadata(effective.get("metadata"), source_name)?;
    let tokens = parse_tokens(&effective, source_name)?;
    let roles = parse_roles(effective.get("roles"), source_name)?;
    let glyphs = parse_glyphs(effective.get("glyphs"), source_name, "glyphs", false)?;
    let ascii_glyphs = parse_glyphs(
        effective.get("glyphs_ascii"),
        source_name,
        "glyphs_ascii",
        true,
    )?;
    let surfaces = parse_surfaces(effective.get("surfaces"), source_name)?;
    let layout = parse_layout(effective.get("layout"), source_name)?;

    Ok(ParsedTheme {
        metadata,
        tokens,
        roles,
        glyphs,
        ascii_glyphs,
        surfaces,
        layout,
    })
}

fn merge_tables(base: &mut toml::map::Map<String, Value>, overlay: &toml::map::Map<String, Value>) {
    for (key, value) in overlay {
        if let (Some(Value::Table(base_table)), Value::Table(overlay_table)) =
            (base.get_mut(key), value)
        {
            merge_tables(base_table, overlay_table);
        } else {
            base.insert(key.clone(), value.clone());
        }
    }
}

fn reject_control_strings(value: &Value, source_name: &str) -> anyhow::Result<()> {
    match value {
        Value::String(text) if text.chars().any(char::is_control) => {
            bail!("theme {source_name} contains a control character")
        }
        Value::Array(values) => {
            for value in values {
                reject_control_strings(value, source_name)?;
            }
        }
        Value::Table(table) => {
            for value in table.values() {
                reject_control_strings(value, source_name)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_metadata(value: Option<&Value>, source_name: &str) -> anyhow::Result<ThemeMetadata> {
    let mut metadata = ThemeMetadata::default();
    let Some(value) = value else {
        return Ok(metadata);
    };
    let table = value
        .as_table()
        .with_context(|| format!("theme {source_name} metadata must be a table"))?;
    for (key, value) in table {
        match key.as_str() {
            "name" => metadata.name = string(value, source_name, "metadata.name")?,
            "description" => {
                metadata.description = string(value, source_name, "metadata.description")?
            }
            "author" => metadata.author = string(value, source_name, "metadata.author")?,
            "version" => metadata.version = scalar(value, source_name, "metadata.version")?,
            "terminal" => metadata.terminal = string(value, source_name, "metadata.terminal")?,
            "adaptive" => metadata.adaptive = boolean(value, source_name, "metadata.adaptive")?,
            other => bail!("theme {source_name} has unknown metadata field {other:?}"),
        }
    }
    if metadata.name.len() > 80 || metadata.description.len() > 512 || metadata.author.len() > 120 {
        bail!("theme {source_name} metadata is too long");
    }
    if !matches!(
        metadata.terminal.as_str(),
        "light-dark" | "dark" | "light" | "any"
    ) {
        bail!("theme {source_name} metadata.terminal must be light-dark, dark, light, or any");
    }
    Ok(metadata)
}

fn parse_tokens(
    root: &toml::map::Map<String, Value>,
    source_name: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut tokens = BTreeMap::new();
    for (key, value) in root {
        match key.as_str() {
            "metadata" | "roles" | "glyphs" | "glyphs_ascii" | "surfaces" | "layout" => continue,
            "colors" | "tokens" => {
                let table = value
                    .as_table()
                    .with_context(|| format!("theme {source_name} {key} must be a table"))?;
                flatten_tokens(table, "", &mut tokens, source_name)?;
            }
            "spacing" | "icons" => {
                let table = value
                    .as_table()
                    .with_context(|| format!("theme {source_name} {key} must be a table"))?;
                for (child, value) in table {
                    let token = format!("{}_{child}", key.trim_end_matches('s'));
                    insert_token(&mut tokens, token, value, source_name)?;
                }
            }
            "model" => {
                let table = value
                    .as_table()
                    .with_context(|| format!("theme {source_name} model must be a table"))?;
                flatten_tokens(table, "model", &mut tokens, source_name)?;
            }
            _ if !value.is_table() && !value.is_array() => {
                insert_token(&mut tokens, key.clone(), value, source_name)?;
            }
            _ => {}
        }
    }
    Ok(tokens)
}

fn flatten_tokens(
    table: &toml::map::Map<String, Value>,
    prefix: &str,
    tokens: &mut BTreeMap<String, String>,
    source_name: &str,
) -> anyhow::Result<()> {
    for (key, value) in table {
        let token = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if let Value::Table(child) = value {
            flatten_tokens(child, &token, tokens, source_name)?;
        } else {
            insert_token(tokens, token, value, source_name)?;
        }
    }
    Ok(())
}

fn insert_token(
    tokens: &mut BTreeMap<String, String>,
    key: String,
    value: &Value,
    source_name: &str,
) -> anyhow::Result<()> {
    valid_key(&key)
        .then_some(())
        .with_context(|| format!("theme {source_name} has invalid token name {key:?}"))?;
    let value = scalar(value, source_name, &key)?;
    if value.len() > MAX_TOKEN_VALUE_BYTES {
        bail!("theme {source_name} token {key:?} is too long");
    }
    tokens.insert(key, value);
    Ok(())
}

fn parse_roles(
    value: Option<&Value>,
    source_name: &str,
) -> anyhow::Result<BTreeMap<String, RoleStyleSpec>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let table = value
        .as_table()
        .with_context(|| format!("theme {source_name} roles must be a table"))?;
    if table.len() > MAX_THEME_ROLES {
        bail!("theme {source_name} has too many semantic roles");
    }
    let mut roles = BTreeMap::new();
    for (name, value) in table {
        if !valid_role_name(name) {
            bail!("theme {source_name} has invalid semantic role {name:?}");
        }
        let fields = value
            .as_table()
            .with_context(|| format!("theme {source_name} role {name:?} must be a table"))?;
        let mut style = RoleStyleSpec::default();
        for (field, value) in fields {
            match field.as_str() {
                "foreground" => {
                    let color = string(value, source_name, &format!("roles.{name}.foreground"))?;
                    validate_color_reference(&color, source_name, name, field)?;
                    style.foreground = Some(color);
                }
                "background" => {
                    let color = string(value, source_name, &format!("roles.{name}.background"))?;
                    validate_color_reference(&color, source_name, name, field)?;
                    style.background = Some(color);
                }
                "bold" => style.bold = Some(boolean(value, source_name, field)?),
                "dim" => style.dim = Some(boolean(value, source_name, field)?),
                "italic" => style.italic = Some(boolean(value, source_name, field)?),
                "underline" => style.underline = Some(boolean(value, source_name, field)?),
                "strikethrough" => style.strikethrough = Some(boolean(value, source_name, field)?),
                "inverse" => style.inverse = Some(boolean(value, source_name, field)?),
                "adaptive" => style.adaptive = Some(boolean(value, source_name, field)?),
                other => {
                    bail!("theme {source_name} role {name:?} has unknown style field {other:?}")
                }
            }
        }
        roles.insert(name.clone(), style);
    }
    Ok(roles)
}

fn validate_color_reference(
    value: &str,
    source_name: &str,
    role: &str,
    field: &str,
) -> anyhow::Result<()> {
    if Color::parse(value).is_none() && !valid_key(value) {
        bail!("theme {source_name} role {role:?} has invalid {field} color or token {value:?}");
    }
    Ok(())
}

fn parse_glyphs(
    value: Option<&Value>,
    source_name: &str,
    section: &str,
    ascii_only: bool,
) -> anyhow::Result<BTreeMap<String, String>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let table = value
        .as_table()
        .with_context(|| format!("theme {source_name} {section} must be a table"))?;
    if table.len() > MAX_THEME_GLYPHS {
        bail!("theme {source_name} has too many glyphs");
    }
    let mut glyphs = BTreeMap::new();
    for (name, value) in table {
        if !valid_key(name) {
            bail!("theme {source_name} has invalid glyph name {name:?}");
        }
        let glyph = string(value, source_name, &format!("{section}.{name}"))?;
        if glyph.is_empty() {
            bail!("theme {source_name} glyph {name:?} cannot be empty");
        }
        if ascii_only && !glyph.is_ascii() {
            bail!("theme {source_name} ASCII glyph {name:?} must contain only ASCII characters");
        }
        let width = display_width(&glyph);
        let maximum = match name.as_str() {
            "wordmark" => 24,
            "separator" => 8,
            "branch" | "last_branch" | "ellipsis" | "collapsed" | "expanded" => 4,
            _ => 3,
        };
        if width == 0 || width > maximum {
            bail!("theme {source_name} glyph {name:?} is {width} columns; the limit is {maximum}");
        }
        if matches!(
            name.as_str(),
            "top_left" | "top_right" | "bottom_left" | "bottom_right" | "horizontal" | "vertical"
        ) && width != 1
        {
            bail!("theme {source_name} structural glyph {name:?} must be one column");
        }
        glyphs.insert(name.clone(), glyph);
    }
    Ok(glyphs)
}

fn parse_surfaces(
    value: Option<&Value>,
    source_name: &str,
) -> anyhow::Result<BTreeMap<String, ThemeSurface>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let table = value
        .as_table()
        .with_context(|| format!("theme {source_name} surfaces must be a table"))?;
    if table.len() > MAX_THEME_SURFACES {
        bail!("theme {source_name} has too many transcript surfaces");
    }

    let mut surfaces = BTreeMap::new();
    for (kind, value) in table {
        if !matches!(
            kind.as_str(),
            "user"
                | "assistant"
                | "reasoning"
                | "tool"
                | "notice"
                | "outcome"
                | "shell"
                | "compaction"
        ) {
            bail!("theme {source_name} has unknown transcript surface {kind:?}");
        }
        let fields = value
            .as_table()
            .with_context(|| format!("theme {source_name} surface {kind:?} must be a table"))?;
        let mut surface = ThemeSurface::default();
        for (field, value) in fields {
            let path = format!("surfaces.{kind}.{field}");
            match field.as_str() {
                "chrome" => {
                    surface.chrome = parse_surface_chrome(value, source_name, &path)?;
                }
                "heading" => {
                    surface.heading = parse_surface_heading(value, source_name, &path)?;
                }
                "label" => surface.label = Some(surface_label(value, source_name, &path)?),
                "padding" => {
                    surface.padding = bounded_u16(value, source_name, &path, 0, 4)?;
                }
                "width" => {
                    surface.width = match string(value, source_name, &path)?.as_str() {
                        "full" => ThemeSurfaceWidth::Full,
                        "content" => ThemeSurfaceWidth::Content,
                        other => bail!(
                            "theme {source_name} {path} must be full or content, got {other:?}"
                        ),
                    };
                }
                "align" => {
                    surface.align = match string(value, source_name, &path)?.as_str() {
                        "left" => ThemeSurfaceAlign::Left,
                        "center" => ThemeSurfaceAlign::Center,
                        "right" => ThemeSurfaceAlign::Right,
                        other => bail!(
                            "theme {source_name} {path} must be left, center, or right, got {other:?}"
                        ),
                    };
                }
                "max_width" => {
                    surface.max_width = Some(bounded_u16(value, source_name, &path, 12, 240)?);
                }
                "narrow_chrome" => {
                    surface.narrow_chrome = Some(parse_surface_chrome(value, source_name, &path)?);
                }
                "narrow_heading" => {
                    surface.narrow_heading =
                        Some(parse_surface_heading(value, source_name, &path)?);
                }
                "narrow_label" => {
                    surface.narrow_label = Some(surface_label(value, source_name, &path)?);
                }
                "narrow_padding" => {
                    surface.narrow_padding = Some(bounded_u16(value, source_name, &path, 0, 2)?);
                }
                other => bail!("theme {source_name} surface {kind:?} has unknown field {other:?}"),
            }
        }
        if surface.heading != ThemeSurfaceHeading::None && surface.label.is_none() {
            bail!("theme {source_name} surface {kind:?} needs label when heading is enabled");
        }
        if surface
            .narrow_heading
            .is_some_and(|heading| heading != ThemeSurfaceHeading::None)
            && surface.narrow_label.is_none()
            && surface.label.is_none()
        {
            bail!(
                "theme {source_name} surface {kind:?} needs label when narrow heading is enabled"
            );
        }
        surfaces.insert(kind.clone(), surface);
    }
    Ok(surfaces)
}

fn parse_surface_chrome(
    value: &Value,
    source_name: &str,
    path: &str,
) -> anyhow::Result<ThemeSurfaceChrome> {
    Ok(match string(value, source_name, path)?.as_str() {
        "plain" => ThemeSurfaceChrome::Plain,
        "rail" => ThemeSurfaceChrome::Rail,
        "card" => ThemeSurfaceChrome::Card,
        "band" => ThemeSurfaceChrome::Band,
        "rule" => ThemeSurfaceChrome::Rule,
        other => bail!(
            "theme {source_name} {path} must be plain, rail, card, band, or rule, got {other:?}"
        ),
    })
}

fn parse_surface_heading(
    value: &Value,
    source_name: &str,
    path: &str,
) -> anyhow::Result<ThemeSurfaceHeading> {
    Ok(match string(value, source_name, path)?.as_str() {
        "none" => ThemeSurfaceHeading::None,
        "inline" => ThemeSurfaceHeading::Inline,
        "tab" => ThemeSurfaceHeading::Tab,
        "overline" => ThemeSurfaceHeading::Overline,
        other => bail!(
            "theme {source_name} {path} must be none, inline, tab, or overline, got {other:?}"
        ),
    })
}

fn surface_label(value: &Value, source_name: &str, path: &str) -> anyhow::Result<String> {
    let label = string(value, source_name, path)?;
    let width = display_width(&label);
    if label.is_empty() || label.len() > 64 || width > 24 {
        bail!("theme {source_name} {path} must be 1 to 24 columns and at most 64 bytes");
    }
    Ok(label)
}

fn parse_layout(value: Option<&Value>, source_name: &str) -> anyhow::Result<ThemeLayout> {
    let Some(value) = value else {
        return Ok(ThemeLayout::default());
    };
    let table = value
        .as_table()
        .with_context(|| format!("theme {source_name} layout must be a table"))?;
    let mut layout = ThemeLayout::default();
    for (key, value) in table {
        match key.as_str() {
            "density" => {
                layout.density = match string(value, source_name, "layout.density")?.as_str() {
                    "compact" => ThemeDensity::Compact,
                    "comfortable" => ThemeDensity::Comfortable,
                    "airy" => ThemeDensity::Airy,
                    other => bail!("theme {source_name} has invalid layout density {other:?}"),
                }
            }
            "show_header" => layout.show_header = boolean(value, source_name, key)?,
            "show_footer" => layout.show_footer = boolean(value, source_name, key)?,
            "show_status_line" => layout.show_status_line = boolean(value, source_name, key)?,
            "show_tool_duration" => layout.show_tool_duration = boolean(value, source_name, key)?,
            "show_reasoning" => layout.show_reasoning = boolean(value, source_name, key)?,
            "show_panel_borders" => layout.show_panel_borders = boolean(value, source_name, key)?,
            "transcript_inset" => {
                layout.transcript_inset = bounded_u16(value, source_name, key, 0, 8)?
            }
            "composer_padding" => {
                layout.composer_padding = bounded_u16(value, source_name, key, 0, 4)?
            }
            "narrow_breakpoint" => {
                layout.narrow_breakpoint = bounded_u16(value, source_name, key, 20, 240)?
            }
            "narrow_show_header" => layout.narrow_show_header = boolean(value, source_name, key)?,
            "narrow_show_footer" => layout.narrow_show_footer = boolean(value, source_name, key)?,
            "narrow_show_status_line" => {
                layout.narrow_show_status_line = boolean(value, source_name, key)?
            }
            "narrow_show_tool_duration" => {
                layout.narrow_show_tool_duration = boolean(value, source_name, key)?
            }
            "narrow_show_reasoning" => {
                layout.narrow_show_reasoning = boolean(value, source_name, key)?
            }
            "narrow_show_panel_borders" => {
                layout.narrow_show_panel_borders = boolean(value, source_name, key)?
            }
            other => bail!("theme {source_name} has unknown layout field {other:?}"),
        }
    }
    Ok(layout)
}

fn bounded_u16(
    value: &Value,
    source_name: &str,
    field: &str,
    minimum: u16,
    maximum: u16,
) -> anyhow::Result<u16> {
    let value = value
        .as_integer()
        .with_context(|| format!("theme {source_name} {field} must be an integer"))?;
    let value = u16::try_from(value)
        .with_context(|| format!("theme {source_name} {field} is out of range"))?;
    if !(minimum..=maximum).contains(&value) {
        bail!("theme {source_name} {field} must be between {minimum} and {maximum}");
    }
    Ok(value)
}

fn string(value: &Value, source_name: &str, field: &str) -> anyhow::Result<String> {
    value
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("theme {source_name} {field} must be a string"))
}

fn boolean(value: &Value, source_name: &str, field: &str) -> anyhow::Result<bool> {
    value
        .as_bool()
        .with_context(|| format!("theme {source_name} {field} must be true or false"))
}

fn scalar(value: &Value, source_name: &str, field: &str) -> anyhow::Result<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Integer(value) => Ok(value.to_string()),
        Value::Float(value) => Ok(value.to_string()),
        Value::Boolean(value) => Ok(value.to_string()),
        _ => bail!("theme {source_name} {field} must be a scalar value"),
    }
}

fn valid_role_name(name: &str) -> bool {
    valid_key(name) && name.len() <= 96 && !name.starts_with('.') && !name.ends_with('.')
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_variant_overlays_tokens_roles_glyphs_and_layout() {
        let source = r##"
            [metadata]
            name = "Variant"

            [colors]
            accent = "#777777"

            [roles.tool_title]
            foreground = "accent"
            bold = true

            [glyphs]
            prompt = ">"

            [layout]
            density = "comfortable"
            narrow_breakpoint = 70

            [variants.dark.colors]
            accent = "#abcdef"

            [variants.dark.glyphs]
            prompt = "❯"

            [variants.dark.layout]
            density = "compact"
        "##;
        let parsed = parse_theme(source, "variant", TerminalBackground::Dark).unwrap();
        assert_eq!(
            parsed.tokens.get("accent").map(String::as_str),
            Some("#abcdef")
        );
        assert_eq!(parsed.glyphs.get("prompt").map(String::as_str), Some("❯"));
        assert_eq!(parsed.layout.density, ThemeDensity::Compact);
        assert_eq!(parsed.roles["tool_title"].bold, Some(true));
    }

    #[test]
    fn semantic_extension_roles_are_open_but_typed() {
        let parsed = parse_theme(
            r##"
                [roles."extension.git.branch"]
                foreground = "#123456"
                italic = true
            "##,
            "extension-role",
            TerminalBackground::Unknown,
        )
        .unwrap();
        let role = &parsed.roles["extension.git.branch"];
        assert_eq!(role.foreground.as_deref(), Some("#123456"));
        assert_eq!(role.italic, Some(true));
    }

    #[test]
    fn terminal_controls_and_wide_structural_glyphs_are_rejected() {
        let control = parse_theme(
            "[glyphs]\nprompt = \"\\u001b[31m\"\n",
            "control",
            TerminalBackground::Unknown,
        )
        .unwrap_err()
        .to_string();
        assert!(control.contains("control character"));

        let wide = parse_theme(
            "[glyphs]\nhorizontal = \"--\"\n",
            "wide",
            TerminalBackground::Unknown,
        )
        .unwrap_err()
        .to_string();
        assert!(wide.contains("must be one column"));
    }

    #[test]
    fn narrow_layout_is_deterministic() {
        let layout = ThemeLayout {
            show_tool_duration: true,
            narrow_show_tool_duration: false,
            narrow_breakpoint: 72,
            ..ThemeLayout::default()
        };
        assert!(layout.resolve(120).show_tool_duration);
        let narrow = layout.resolve(60);
        assert!(narrow.narrow);
        assert!(!narrow.show_tool_duration);
    }

    #[test]
    fn transcript_surfaces_and_ascii_glyphs_are_bounded_and_resolve_narrowly() {
        let parsed = parse_theme(
            r#"
                [glyphs_ascii]
                rail = "|"
                wordmark = "YGG-CARD"

                [surfaces.user]
                chrome = "card"
                heading = "tab"
                label = "REQUEST"
                padding = 2
                width = "content"
                align = "right"
                max_width = 88
                narrow_chrome = "rail"
                narrow_heading = "inline"
                narrow_label = "IN"
                narrow_padding = 0
            "#,
            "surface",
            TerminalBackground::Unknown,
        )
        .unwrap();
        assert_eq!(parsed.ascii_glyphs["wordmark"], "YGG-CARD");
        let surface = &parsed.surfaces["user"];
        assert_eq!(surface.chrome, ThemeSurfaceChrome::Card);
        assert_eq!(surface.width, ThemeSurfaceWidth::Content);
        assert_eq!(surface.align, ThemeSurfaceAlign::Right);
        let narrow = surface.resolve(true);
        assert_eq!(narrow.chrome, ThemeSurfaceChrome::Rail);
        assert_eq!(narrow.heading, ThemeSurfaceHeading::Inline);
        assert_eq!(narrow.label, Some("IN"));
        assert_eq!(narrow.padding, 0);
    }

    #[test]
    fn transcript_surface_schema_rejects_open_ended_layout_and_unicode_ascii_fallbacks() {
        let unknown = parse_theme(
            "[surfaces.widget]\nchrome = \"card\"\n",
            "unknown-surface",
            TerminalBackground::Unknown,
        )
        .unwrap_err()
        .to_string();
        assert!(unknown.contains("unknown transcript surface"), "{unknown}");

        let field = parse_theme(
            "[surfaces.user]\nchrome = \"plain\"\nformat = \"{private_state}\"\n",
            "open-ended-surface",
            TerminalBackground::Unknown,
        )
        .unwrap_err()
        .to_string();
        assert!(field.contains("unknown field"), "{field}");

        let unicode = parse_theme(
            "[glyphs_ascii]\nprompt = \"❯\"\n",
            "unicode-ascii",
            TerminalBackground::Unknown,
        )
        .unwrap_err()
        .to_string();
        assert!(unicode.contains("only ASCII"), "{unicode}");
    }
}
