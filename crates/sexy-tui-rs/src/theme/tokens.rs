//! Stable semantic token vocabulary and restrained defaults.

use std::collections::HashMap;

/// Apply built-in defaults. Neutral prose and surfaces follow the terminal's
/// configured foreground/background; fixed backgrounds are never required.
pub fn apply_defaults(values: &mut HashMap<String, String>) {
    let colors = [
        ("surface", "default"),
        ("overlay", "default"),
        ("raised", "default"),
        ("foreground", "default"),
        ("muted", "default"),
        ("dim", "default"),
        // Balanced for acceptable contrast on pure black and pure white.
        ("accent", "#16876d"),
        ("success", "#2e7d4f"),
        ("error", "#c74747"),
        ("warning", "#9a6700"),
        ("info", "#287fb8"),
        ("border", "default"),
        ("border_focused", "accent"),
        ("border_idle", "border"),
        ("user_msg_text", "foreground"),
        ("user_msg_bg", "default"),
        ("assistant_msg_text", "foreground"),
        ("assistant_msg_bg", "default"),
        ("tool_title", "foreground"),
        ("tool_output", "foreground"),
        ("tool_pending_bg", "default"),
        ("tool_success_bg", "default"),
        ("tool_error_bg", "default"),
        ("diff_added", "#00b847"),
        ("diff_removed", "#c74747"),
        // Surfaces are opt-in. Foreground-only diffs remain readable on both
        // light and dark terminal profiles without knowing their background.
        ("diff_added_bg", "default"),
        ("diff_removed_bg", "default"),
        ("diff_context", "foreground"),
        ("diff_hunk", "#7656a6"),
        ("diff_header", "foreground"),
        ("md_heading", "#7eb8da"),
        ("md_emphasis", "foreground"),
        ("md_strong", "foreground"),
        ("md_link", "#5ea3d9"),
        ("md_code", "#a66a3f"),
        ("md_code_block", "foreground"),
        // Fixed surfaces are unreadable on one of light/dark terminals and
        // can look like selection. Themes that know their background may opt
        // into either surface explicitly.
        ("md_code_bg", "default"),
        ("md_code_inline_bg", "default"),
        ("md_code_border", "border"),
        ("md_quote", "foreground"),
        ("md_quote_bg", "default"),
        ("md_quote_border", "#7eb8da"),
        ("md_hr", "border"),
        ("md_list_bullet", "foreground"),
        ("syntax_comment", "muted"),
        ("syntax_keyword", "#7656a6"),
        ("syntax_function", "#287fb8"),
        ("syntax_variable", "foreground"),
        ("syntax_string", "#00b847"),
        ("syntax_number", "#9a6700"),
        ("syntax_type", "#8a6200"),
        ("syntax_operator", "#a34473"),
        ("syntax_punctuation", "muted"),
    ];
    for (key, value) in colors {
        values.entry(key.into()).or_insert_with(|| value.into());
    }

    for (key, value) in [
        ("border_style", "thin"),
        ("spacing_none", "0"),
        ("spacing_xs", "1"),
        ("spacing_sm", "2"),
        ("spacing_md", "4"),
        ("spacing_lg", "8"),
        ("spacing_xl", "12"),
        ("editor_cursor_style", "bar"),
        ("editor_padding", "sm"),
        ("editor_border", "thin"),
        ("select_list_prefix", "> "),
        ("select_list_scroll_indicator", "true"),
        ("select_list_max_visible", "10"),
        ("loader_spinner_frames", "-,\\,|,/"),
        ("loader_interval_ms", "80"),
    ] {
        values.entry(key.into()).or_insert_with(|| value.into());
    }

    // Legacy icon tokens remain configurable, but defaults never require a
    // private-use glyph or an emoji.
    for (key, value) in [
        ("icon_branch", "git:"),
        ("icon_success", "+"),
        ("icon_error", "x"),
        ("icon_warning", "!"),
        ("icon_folder", "./"),
        ("icon_search", "?"),
        ("icon_prompt", ">"),
        ("icon_rust", "rs"),
        ("icon_python", "py"),
        ("icon_go", "go"),
        ("icon_js", "js"),
    ] {
        values.entry(key.into()).or_insert_with(|| value.into());
    }
}

pub fn ascii_fallback(token: &str) -> &str {
    match token {
        "icon_branch" => "git:",
        "icon_success" => "+",
        "icon_error" => "x",
        "icon_warning" => "!",
        "icon_folder" => "./",
        "icon_search" => "?",
        "icon_prompt" => ">",
        "icon_rust" => "rs",
        "icon_python" => "py",
        "icon_go" => "go",
        "icon_js" => "js",
        _ => "*",
    }
}
