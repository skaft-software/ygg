# Ygg themes

Ygg themes are declarative TOML files. They target semantic surfaces rather
than terminal escape sequences, so the same theme can degrade from truecolor
to 256/16 colors, Unicode to ASCII, and styled output to plain text.

Theme files are discovered in these locations:

1. `~/.ygg/themes/*.toml`
2. `.ygg/themes/*.toml` in a trusted workspace
3. Explicit theme directories, in command-line order

Later locations win when names collide. The resource inspector reports each
override and ignores project themes in untrusted workspaces. The ten themes
shipped in the binary are the lowest-precedence fallback, so any of them can be
replaced locally by a file with the same name.

Use `/theme` to pick a theme, `/theme list` to inspect the catalog, and
`/theme reload` after editing the active file. The general `/reload` command
also refreshes resource discovery. At startup, use `--theme <name>` and add
repeatable explicit roots with `--theme-dir <directory>`.

## Bundled theme pack

The pack intentionally demonstrates different design philosophies without
painting over the user's terminal background:

- `bone-machine` — dense brutalist slabs, heavy tabs, and mechanical state cuts
- `circuit-garden` — airy rounded seed/canopy cards with vine-like rails
- `field-notes` — ASCII notebook margins explicitly marked `Q`, `OBS`, and `CMD`
- `oxide-console` — industrial instrument panels, buses, alarms, and registers
- `paper-ledger` — measured 84-column rules, label columns, and margin notes
- `signal-noir` — sparse prose interrupted by red signal and trace bands
- `synthwave-relay` — centered `TX`/`RX` broadcast cards and auxiliary channels
- `tidepool` — alternating left/right islands joined by current and tidemark rails
- `violet-hour` — chapters, pull quotes, marginalia, footnotes, and a final `FINIS`
- `zen-mono` — strict unframed 68-column text floating in negative space

Each bundled theme opts into adaptive color balancing. Ygg keeps its design
hue while moving RGB foregrounds toward a contrast-safe luminance for the
detected light, dark, or unknown terminal profile. Detection first honors
`YGG_COLOR_SCHEME` and `COLORFGBG`; in the interactive TUI, when those are
absent, Ygg sends a short OSC 11 background query after entering raw mode and
uses the response when the terminal provides one.

Every pack theme also retains the terminal's own default canvas: none sets a
global background fill. Cards and bands may use a bounded, low-luminance
semantic tint when Ygg knows the terminal is light or dark. If the profile
remains unknown after the startup checks, those tints resolve to `default`, so
the recipe never guesses a canvas color. The built-in default and bundled themes
share a standard technical palette for code and diffs: syntax roles own
foreground colors, diff rows own quiet add/delete backgrounds, and `+`/`-`
markers carry the strongest hue. RGB accents are quantized through the active
ANSI 256 or ANSI 16 palette, and plain mode removes styling and selects an
explicit `[glyphs_ascii]` set without changing the visible text.

User prompts are the deliberate exception to a completely unpainted canvas:
each persisted, model-bound prompt paints its complete inner semantic row with
the model colour, including theme padding and trailing cells. The theme's
transcript inset and structural border remain outside that rectangle. Ygg stores
the exact `#RRGGBB` value with the session entry, so changing the model or theme
cannot recolour old prompts. Only output capability changes the wire encoding:
truecolor uses the stored RGB exactly, ANSI 256/16 quantize it, and plain/no-color
terminals retain the same row geometry without escapes.

## Runtime reload and native scrollback

Selecting or reloading a theme clears and repaints the complete visible
viewport before the next frame. Current rows and rows rendered afterward
therefore use the new theme, including byte-identical separator rows that a
normal differential update would otherwise skip.

The default primary-screen renderer deliberately commits older transcript rows
to the terminal's native scrollback. Terminal protocols do not provide a safe,
portable way to rewrite those historical cells, so Ygg does not claim to
recolour them and does not clear the user's scrollback on a theme change. Rows
already in native history retain the colours with which they were committed;
the visible viewport and future rows use the new theme. `--mouse app` instead
uses an application-owned semantic viewport, so retained transcript rows are
rendered with the current theme whenever they become visible.

## Schema

```toml
[metadata]
name = "My Workshop"
description = "A project-local agent bench"
terminal = "light-dark"
adaptive = true

[colors]
accent = "#168f91"
muted = "#5b7376"
tool_title = "accent"
diff_added_bg = "#388064"
diff_removed_bg = "#cf5b55"
prompt_card_bg = "#164a4d"

[model]
use_lab_color = false

[roles.tool_title]
foreground = "tool_title"
bold = true

[roles.heading]
foreground = "accent"
bold = true

# Namespaced roles are open contribution points for extensions.
[roles."extension.git.branch"]
foreground = "accent"
italic = true

# Footer contributions use this conventional role unless they request one.
[roles."extension.status"]
foreground = "accent"
bold = true

# Header contributions have a separate conventional role.
[roles."extension.header"]
foreground = "accent"
underline = true

[glyphs]
top_left = "╭"
top_right = "╮"
bottom_left = "╰"
bottom_right = "╯"
horizontal = "─"
vertical = "│"
prompt = "❯"
success = "✓"
warning = "△"
error = "×"
collapsed = "▸"
expanded = "▾"
separator = " · "
wordmark = "my-ygg"

[glyphs_ascii]
top_left = "+"
top_right = "+"
bottom_left = "+"
bottom_right = "+"
horizontal = "-"
vertical = "|"
rail = "|"
prompt = ">"
shell = "$"
success = "+"
warning = "!"
error = "x"
collapsed = "[+]"
expanded = "[-]"
separator = " - "
wordmark = "my-ygg"

# Typed transcript recipes are intentionally bounded. They can arrange and
# style semantic content but cannot inject widgets, callbacks, or terminal
# sequences.
[surfaces.user]
chrome = "card"       # plain, rail, card, band, or rule
heading = "tab"       # none, inline, tab, or overline
label = "REQUEST"
padding = 2            # 0..4
width = "content"     # content or full
align = "right"       # left, center, or right
max_width = 84         # 12..240
narrow_chrome = "rail"
narrow_heading = "inline"
narrow_label = "YOU"
narrow_padding = 0

[roles."surface.user"]
foreground = "foreground"
background = "prompt_card_bg"

[roles."surface.user.border"]
foreground = "accent"

[roles."surface.user.label"]
foreground = "accent"
bold = true

[layout]
density = "comfortable" # compact, comfortable, or airy
show_header = true
show_footer = true
show_status_line = true
show_tool_duration = true
show_reasoning = true
show_panel_borders = true
transcript_inset = 2
composer_padding = 1
narrow_breakpoint = 72
narrow_show_header = false
narrow_show_footer = true
narrow_show_status_line = true
narrow_show_tool_duration = false
narrow_show_reasoning = true
narrow_show_panel_borders = false
```

Role foregrounds and backgrounds accept `default`, `#RRGGBB`, ANSI names,
`ansi:N`, `index:N`, or another semantic token. Style attributes are typed
booleans: `bold`, `dim`, `italic`, `underline`, `strikethrough`, and `inverse`.
Theme strings containing terminal controls are rejected. Structural border
glyphs must occupy exactly one terminal column.

`adaptive = true` is optional. It balances RGB foreground and `*_bg` surface
tokens for the detected terminal profile. A theme that needs exact values can
leave it off, or override adaptation on one role with `adaptive = false`.

## Light and dark variants

Themes can layer terminal-specific values without duplicating the whole file:

```toml
[colors]
accent = "#777777" # conservative unknown-background value

[variants.dark.colors]
accent = "#70d8bd"

[variants.light.colors]
accent = "#176b5b"
```

`variants.universal` is applied for every terminal before a detected
`dark`/`light` variant. `variants.unknown` can override the conservative case.
All variants remain subject to capability quantization and plain-mode
fallbacks.

## Extension roles

Extensions should request a semantic name such as `extension.git.branch` and
render through Ygg's semantic-style API. A theme may style any such name under
`[roles]`. Unknown role names remain terminal-neutral, which makes extension
output readable even when the active theme predates the extension.

`extension.header` and `extension.status` are the conventional roles for
persistent extension chrome. Tool-renderer segments can request any other
namespaced role; all ten bundled themes demonstrate both conventional chrome
roles and a distinct extension-specific role. Extension chrome is explicit:
an enabled contribution remains visible even when a theme hides the matching
built-in identity or telemetry surface.

## Current semantic coverage

Theme glyphs currently drive composer borders, prompt markers, wordmarks,
semantic separators, transcript rails/branches, reasoning markers, run
outcomes, and narrow ASCII fallbacks. Markdown's internal list/detail/status
glyphs still come from sexy-tui-rs's capability-aware glyph set; the remaining
configured glyph names are validated vocabulary for completing that bridge.

Role styles are live for rich Markdown/diff roles, extension tool-renderer
segments, persistent extension header/status/footer contributions, and all
three layers of each transcript surface: `surface.<kind>`,
`surface.<kind>.border`, and `surface.<kind>.label`. The eight typed surface
kinds are `user`, `assistant`, `reasoning`, `tool`, `shell`, `notice`, `outcome`,
and `compaction`. Every layout field alters the current renderer:

- `density` selects zero, one, or two rows between semantic transcript blocks.
- `transcript_inset` moves and reflows transcript content without breaking
  mouse-selection coordinates.
- `composer_padding` controls boxed and compact prompt padding and aligns the
  footer/status sentence.
- `show_header` moves model identity into the pinned header band. The compiled
  default leaves it off to preserve Ygg's sparse default geometry.
- `show_footer` retains identity in the footer when the header is hidden, while
  `show_status_line` independently controls telemetry and active-run status.
- `show_panel_borders`, `show_reasoning`, and `show_tool_duration` control their
  respective semantic surfaces.

Each `narrow_show_*` value is resolved once from the physical terminal width,
before transcript insets are applied. This gives themes deterministic narrow
fallbacks without accidentally switching layouts early.

Theme parsing is bounded to 256 KiB. Runtime reload builds a complete new theme
before the shell swaps it in, so malformed edits leave the current theme
intact. Reload also reuses the no-follow regular-file boundary, so a discovered
theme cannot be replaced by a symlink or special file before reloading.
