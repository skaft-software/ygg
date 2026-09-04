//! Deterministic semantic fixtures for the ordinary command/picker contract.
//!
//! These deliberately assert surface rows, not terminal bytes. PTY and complete
//! frame replay belong to their own renderer regressions.

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use sexy_tui_rs::{
    strip_terminal_sequences, terminal_tokens, visible_width, TerminalToken, CURSOR_MARKER,
};

use super::*;
use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
use crate::tui::theme::test_theme_with;

#[derive(Clone, Copy)]
struct ContractFixture {
    name: &'static str,
    width: u16,
    height: u16,
    unicode: bool,
    color: ColorDepth,
    animation: bool,
}

impl ContractFixture {
    fn capabilities(self) -> TerminalCapabilities {
        let mut capabilities = TerminalCapabilities::test(true, self.unicode, self.color);
        capabilities.animation = self.animation;
        capabilities
    }
}

const NARROW: ContractFixture = ContractFixture {
    name: "narrow-46x8",
    width: 46,
    height: 8,
    unicode: true,
    color: ColorDepth::TrueColor,
    animation: true,
};
const NARROW_ASCII: ContractFixture = ContractFixture {
    name: "narrow-ascii-40x8",
    width: 40,
    height: 8,
    unicode: false,
    color: ColorDepth::TrueColor,
    animation: true,
};
const REGULAR: ContractFixture = ContractFixture {
    name: "regular-80x24",
    width: 80,
    height: 24,
    unicode: true,
    color: ColorDepth::TrueColor,
    animation: true,
};
const LARGE: ContractFixture = ContractFixture {
    name: "large-120x40",
    width: 120,
    height: 40,
    unicode: true,
    color: ColorDepth::TrueColor,
    animation: true,
};
const WIDE: ContractFixture = ContractFixture {
    name: "wide-144x48",
    width: 144,
    height: 48,
    unicode: true,
    color: ColorDepth::TrueColor,
    animation: true,
};
const ASCII: ContractFixture = ContractFixture {
    name: "ascii-80x24",
    width: 80,
    height: 24,
    unicode: false,
    color: ColorDepth::TrueColor,
    animation: true,
};
const NO_COLOR: ContractFixture = ContractFixture {
    name: "no-color-80x24",
    width: 80,
    height: 24,
    unicode: true,
    color: ColorDepth::None,
    animation: true,
};
const REDUCED_MOTION: ContractFixture = ContractFixture {
    name: "reduced-motion-80x24",
    width: 80,
    height: 24,
    unicode: true,
    color: ColorDepth::TrueColor,
    animation: false,
};

struct RenderedFixture {
    picker_raw: Vec<String>,
    picker: Vec<String>,
    commands_raw: Vec<String>,
    commands: Vec<String>,
}

fn shell_for(fixture: ContractFixture) -> InteractiveShell {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(fixture.width, fixture.height);
    shell.set_theme(test_theme_with(fixture.capabilities()));
    shell
}

fn plain_rows(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| strip_terminal_sequences(line).replace(CURSOR_MARKER, ""))
        .collect()
}

fn open_contract_picker(shell: &mut InteractiveShell) {
    shell.open_panel(Panel::SelectList {
        surface: OrdinarySurfaceMetadata::new("Select model"),
        items: vec!["Atlas".into(), "Borealis".into()],
        descriptions: vec![
            Some("128K context; vision".into()),
            Some("1M context; audio".into()),
        ],
        selected: 1,
        filter: String::new(),
        action: PanelAction::SelectModel(Vec::new()),
    });
}

fn render_fixture(fixture: ContractFixture) -> RenderedFixture {
    let mut shell = shell_for(fixture);
    open_contract_picker(&mut shell);
    let picker_raw = super::panel_render::render_panel(&shell.state.borrow(), fixture.width);
    shell.close_panel();

    shell.state.borrow_mut().editor = "/m".into();
    let commands_raw = {
        let state = shell.state.borrow();
        super::input_overlays::render_slash_suggestions(&state, fixture.width, 4)
    };

    RenderedFixture {
        picker: plain_rows(&picker_raw),
        commands: plain_rows(&commands_raw),
        picker_raw,
        commands_raw,
    }
}

fn rendered_panel(shell: &InteractiveShell, width: u16) -> String {
    plain_rows(&super::panel_render::render_panel(
        &shell.state.borrow(),
        width,
    ))
    .join("\n")
}

const HOSTILE_CSI: &str = "\x1b[2J";
const HOSTILE_OSC: &str = "\x1b]0;forged-title\x07";
const HOSTILE_HINT: &str = "[x]\nrow\x07\x1b[2J\x1b]0;forged-title\x07long-argument-hint";
const HOSTILE_METADATA: &str =
    "\x07metadata\nforged-row\x1b[2J\x1b]0;forged-title\x07that-keeps-going";

fn set_editor(shell: &mut InteractiveShell, editor: &str) {
    let mut state = shell.state.borrow_mut();
    state.editor = editor.to_owned();
    state.editor_cursor = state.editor.len();
    state.slash_popup_dismissed = false;
}

fn hostile_metadata_shell(fixture: ContractFixture) -> InteractiveShell {
    let mut shell = shell_for(fixture);
    shell.set_prompt_templates(Arc::from(vec![crate::prompts::PromptTemplateDescriptor {
        name: "hostile-prompt".into(),
        description: HOSTILE_METADATA.into(),
        argument_hint: Some(HOSTILE_HINT.into()),
        path: PathBuf::from("/tmp/hostile-prompt.md"),
        trust: crate::prompts::PromptTrust::UserInstalled,
        content_hash: "hostile".into(),
    }]));
    shell.set_skill_commands(Arc::from(vec![(
        "hostile-skill".into(),
        HOSTILE_METADATA.into(),
    )]));
    shell.set_extension_commands(Arc::from(vec![(
        "hostile-extension".into(),
        HOSTILE_METADATA.into(),
    )]));
    set_editor(&mut shell, "/hostile-");
    shell
}

fn render_hostile_suggestions(shell: &InteractiveShell, width: u16) -> Vec<String> {
    super::input_overlays::render_slash_suggestions(&shell.state.borrow(), width, 4)
}

#[test]
fn ordinary_surface_contract_fixture_matrix_preserves_grid_and_capabilities() {
    for fixture in [
        NARROW,
        NARROW_ASCII,
        REGULAR,
        LARGE,
        WIDE,
        ASCII,
        NO_COLOR,
        REDUCED_MOTION,
    ] {
        let rendered = render_fixture(fixture);
        let picker = rendered.picker.join("\n");
        let commands = rendered.commands.join("\n");

        assert!(
            rendered
                .picker
                .iter()
                .chain(&rendered.commands)
                .all(|line| visible_width(line) <= usize::from(fixture.width)),
            "{} overflowed {} columns:\npicker={picker:?}\ncommands={commands:?}",
            fixture.name,
            fixture.width,
        );
        assert!(
            picker.contains("Select model"),
            "{} lost title: {picker}",
            fixture.name
        );
        assert!(
            picker.contains("2/2"),
            "{} lost count: {picker}",
            fixture.name
        );
        assert!(
            picker.contains("Borealis"),
            "{} lost focused selection: {picker}",
            fixture.name
        );
        assert!(
            commands.contains("navigate") && commands.contains("select"),
            "{} lost primary action-footer grammar: {commands}",
            fixture.name
        );
        let expected_footer = match (fixture.width, fixture.unicode) {
            (46, true) => "↑↓ navigate · ↵ select · esc close",
            (40, false) => "up/down navigate - enter select",
            (_, true) => "commands · ↑↓ navigate · ↵ select · esc close",
            (_, false) => "commands - up/down navigate - enter select - esc close",
        };
        assert_eq!(
            rendered.commands.last().map(|line| line.trim_start()),
            Some(expected_footer),
            "{} changed the deterministic action footer",
            fixture.name
        );

        match fixture.width {
            80 if fixture.unicode => {
                let label = rendered
                    .picker
                    .iter()
                    .position(|line| line.contains("Atlas"))
                    .expect("regular fixture label");
                let metadata = rendered
                    .picker
                    .iter()
                    .position(|line| line.contains("128K context; vision"))
                    .expect("regular fixture metadata");
                assert!(
                    label < metadata,
                    "regular fixture did not stack metadata: {picker}"
                );
            }
            width if width >= 120 => assert!(
                rendered
                    .picker
                    .iter()
                    .any(|line| line.contains("Atlas") && line.contains("128K context; vision")),
                "wide fixture did not keep label/metadata on one shared row: {picker}"
            ),
            _ => {}
        }

        if fixture.color == ColorDepth::None {
            assert!(
                rendered
                    .picker_raw
                    .iter()
                    .chain(&rendered.commands_raw)
                    // The renderer's cursor marker is a control sentinel,
                    // not colour/style output; remove it before testing the
                    // no-colour presentation projection.
                    .all(|line| !line.replace(CURSOR_MARKER, "").contains('\x1b')),
                "no-color fixture emitted ANSI styling: picker={:?}, commands={:?}",
                rendered.picker_raw,
                rendered.commands_raw
            );
        }
    }

    let ascii = render_fixture(ASCII);
    let ascii_picker = ascii.picker.join("\n");
    let ascii_commands = ascii.commands.join("\n");
    assert!(
        ascii_picker.contains("> Borealis"),
        "ASCII focus marker: {ascii_picker}"
    );
    assert!(
        ascii_commands.contains("up/down navigate") && ascii_commands.contains("enter select"),
        "ASCII action footer: {ascii_commands}"
    );
    assert!(
        !ascii_picker.contains('›')
            && !ascii_commands.contains('↑')
            && !ascii_commands.contains('↓')
            && !ascii_commands.contains('↵'),
        "ASCII fixture leaked Unicode interaction glyphs"
    );

    let ordinary = render_fixture(REGULAR);
    let reduced = render_fixture(REDUCED_MOTION);
    assert_eq!(
        ordinary.picker, reduced.picker,
        "motion changed picker geometry"
    );
    assert_eq!(
        ordinary.commands, reduced.commands,
        "motion changed command discovery geometry"
    );
}

#[test]
fn ordinary_surface_contract_footer_drops_complete_lower_priority_segments() {
    let mut shell = shell_for(NARROW_ASCII);
    set_editor(&mut shell, "/m");

    for (width, expected) in [
        (40, "up/down navigate - enter select"),
        (30, "up/down navigate"),
    ] {
        let rows = plain_rows(&super::input_overlays::render_slash_suggestions(
            &shell.state.borrow(),
            width,
            4,
        ));
        let footer = rows.last().expect("slash footer");
        assert_eq!(
            footer.trim_start(),
            expected,
            "{width}-column ASCII footer kept a partial lower-priority segment: {footer:?}"
        );
    }

    let rows = plain_rows(&super::input_overlays::render_slash_suggestions(
        &shell.state.borrow(),
        12,
        4,
    ));
    let footer = rows.last().expect("tiny slash footer");
    assert_eq!(
        footer.trim(),
        "up/down na",
        "only the highest-priority navigation segment may truncate: {footer:?}"
    );
    assert!(
        !footer.contains("select") && !footer.contains("esc") && !footer.contains(" - "),
        "tiny footer retained a partial optional segment: {footer:?}"
    );
}

#[test]
fn ordinary_surface_contract_sanitizes_hostile_dynamic_slash_metadata() {
    let plain_narrow = ContractFixture {
        name: "narrow-no-color-46x8",
        width: NARROW.width,
        height: NARROW.height,
        unicode: true,
        color: ColorDepth::None,
        animation: true,
    };

    for fixture in [NARROW, plain_narrow, NARROW_ASCII] {
        let mut shell = hostile_metadata_shell(fixture);
        {
            let state = shell.state.borrow();
            let suggestions = super::input_overlays::input_slash_suggestions(&state);
            let prompt = suggestions
                .iter()
                .find(|suggestion| suggestion.name == "hostile-prompt")
                .expect("hostile prompt suggestion");
            assert_eq!(prompt.argument_hint.as_deref(), Some(HOSTILE_HINT));
            assert_eq!(
                prompt.provenance,
                super::input_overlays::SlashSuggestionProvenance::Prompt,
                "prompt provenance must remain typed until rendering"
            );
            for (name, provenance) in [
                (
                    "hostile-skill",
                    super::input_overlays::SlashSuggestionProvenance::Skill,
                ),
                (
                    "hostile-extension",
                    super::input_overlays::SlashSuggestionProvenance::Extension,
                ),
            ] {
                assert_eq!(
                    suggestions
                        .iter()
                        .find(|suggestion| suggestion.name == name)
                        .map(|suggestion| suggestion.provenance),
                    Some(provenance),
                    "{name} provenance must remain typed until rendering"
                );
            }
            assert!(
                prompt.description.contains(HOSTILE_CSI)
                    && prompt.description.contains(HOSTILE_OSC),
                "metadata must remain raw until the display boundary: {prompt:?}"
            );
        }

        let raw = render_hostile_suggestions(&shell, fixture.width);
        let plain = plain_rows(&raw);
        let rendered = plain.join("\n");
        assert_eq!(
            raw.len(),
            4,
            "{} let hostile metadata inject a physical suggestion row: {raw:?}",
            fixture.name
        );
        assert!(
            raw.iter().all(|line| {
                !line.contains('\n')
                    && !line.contains('\r')
                    && !line.contains('\x07')
                    && !line.contains(HOSTILE_CSI)
                    && !line.contains(HOSTILE_OSC)
                    && !line.contains("\x1b]")
            }),
            "{} emitted active hostile controls: {raw:?}",
            fixture.name
        );
        assert!(
            raw.iter()
                .flat_map(|line| terminal_tokens(line))
                .all(|token| match token {
                    TerminalToken::Text(text) => !text.chars().any(char::is_control),
                    TerminalToken::Escape(code) => code.ends_with('m'),
                }),
            "{} emitted non-style terminal escapes: {raw:?}",
            fixture.name
        );
        assert!(
            plain.iter().all(|line| !line.chars().any(char::is_control)),
            "{} retained controls after removing trusted theme styling: {plain:?}",
            fixture.name
        );
        assert!(
            plain
                .iter()
                .all(|line| visible_width(line) <= usize::from(fixture.width)),
            "{} clipped hostile metadata outside its display cell: {plain:?}",
            fixture.name
        );
        let prompt_fragment = if fixture.unicode {
            "/hostile-prompt [x] row␇long-…"
        } else {
            "/hostile-prompt [x] row[BEL..."
        };
        assert!(
            rendered.contains(prompt_fragment)
                && rendered.contains("hostile-skill")
                && rendered.contains("hostile-extension"),
            "{} did not sanitize and clip hostile metadata deterministically: {rendered:?}",
            fixture.name
        );
        if !fixture.unicode {
            assert!(
                plain.iter().all(|line| line.is_ascii()),
                "ASCII hostile metadata leaked Unicode: {plain:?}"
            );
        }

        if fixture.color == ColorDepth::None {
            assert!(
                raw.iter().all(|line| !line.contains('\x1b')),
                "no-color hostile metadata emitted ANSI: {raw:?}"
            );
        } else {
            assert!(
                raw.iter().any(|line| line.contains('\x1b')),
                "rich hostile metadata fixture did not exercise styled rows: {raw:?}"
            );
        }

        for name in ["hostile-prompt", "hostile-skill", "hostile-extension"] {
            set_editor(&mut shell, &format!("/{name}"));
            shell.complete_slash_command();
            assert_eq!(
                shell.pending(),
                format!("/{name} "),
                "display sanitization changed {name}'s completion identity"
            );
        }
    }
}

#[test]
fn ordinary_surface_contract_statuses_actions_and_sanitization_are_explicit() {
    let mut loading = shell_for(NO_COLOR);
    let mut picker = PickerState::new(Vec::new(), None);
    picker.scope = PickerScope::All;
    picker.surface.lifecycle = OrdinarySurfaceLifecycle::loading("all workspaces");
    loading.open_panel(Panel::SessionPicker { picker });
    let loading_text = rendered_panel(&loading, NO_COLOR.width);
    assert!(
        loading_text.to_ascii_lowercase().contains("loading")
            && loading_text.contains("all workspaces"),
        "loading status lost its semantic word/detail: {loading_text}"
    );

    let mut narrow_loading = shell_for(NARROW);
    let mut picker = PickerState::new(Vec::new(), None);
    picker.surface.lifecycle = OrdinarySurfaceLifecycle::loading("all workspaces");
    narrow_loading.open_panel(Panel::SessionPicker { picker });
    let narrow_loading_text = rendered_panel(&narrow_loading, NARROW.width);
    assert!(
        narrow_loading_text.to_ascii_lowercase().contains("loading"),
        "narrow session picker hid its explicit lifecycle status: {narrow_loading_text}"
    );

    let mut completed = shell_for(NO_COLOR);
    let mut picker = PickerState::new(Vec::new(), None);
    picker.surface.lifecycle = OrdinarySurfaceLifecycle::success(
        "session renamed",
        Instant::now() + Duration::from_secs(60),
    );
    completed.open_panel(Panel::SessionPicker { picker });
    let completed_text = rendered_panel(&completed, NO_COLOR.width);
    assert!(
        completed_text.to_ascii_lowercase().contains("completed")
            && completed_text.contains("session renamed"),
        "success status lost its semantic word/detail: {completed_text}"
    );

    let mut failed = shell_for(NO_COLOR);
    let mut picker = PickerState::new(Vec::new(), None);
    picker.surface.lifecycle = OrdinarySurfaceLifecycle::recoverable_error(
        "to rename session: permission denied",
        Instant::now() + Duration::from_secs(60),
    );
    failed.open_panel(Panel::SessionPicker { picker });
    let failed_text = rendered_panel(&failed, NO_COLOR.width);
    assert!(
        failed_text.to_ascii_lowercase().contains("failed")
            && failed_text.contains("rename session")
            && failed_text.contains("permission denied"),
        "recoverable error lost its semantic word/detail: {failed_text}"
    );

    let mut cancelled = shell_for(NO_COLOR);
    let mut picker = PickerState::new(Vec::new(), None);
    picker.surface.lifecycle =
        OrdinarySurfaceLifecycle::cancelled("rename", Instant::now() + Duration::from_secs(60));
    cancelled.open_panel(Panel::SessionPicker { picker });
    let cancelled_text = rendered_panel(&cancelled, NO_COLOR.width);
    assert!(
        cancelled_text.to_ascii_lowercase().contains("cancelled")
            && cancelled_text.contains("rename"),
        "cancellation status lost its semantic word/detail: {cancelled_text}"
    );

    let mut empty = shell_for(NO_COLOR);
    open_contract_picker(&mut empty);
    {
        let mut state = empty.state.borrow_mut();
        let Some(Panel::SelectList {
            surface, filter, ..
        }) = state.panel.as_mut()
        else {
            panic!("contract picker should be open");
        };
        *filter = "missing".into();
        surface.lifecycle = OrdinarySurfaceLifecycle::empty("filtered models");
    }
    let empty_text = rendered_panel(&empty, NO_COLOR.width);
    assert!(
        empty_text.to_ascii_lowercase().contains("no matches")
            && empty_text.contains("filtered models")
            && empty_text.matches("no matches").count() == 1,
        "empty state lost or duplicated its typed status: {empty_text}"
    );

    let mut purpose = shell_for(NO_COLOR);
    purpose.open_panel(Panel::MessagePicker {
        picker: MessagePicker::new(vec![ForkMessage {
            entry_id: "entry-contract".into(),
            text: "Use this message as the fork boundary".into(),
            whole_conversation: false,
        }]),
    });
    let purpose_text = rendered_panel(&purpose, NO_COLOR.width);
    assert!(
        purpose_text.contains("Fork from Message")
            && purpose_text.contains("Select a message to copy its path into a new session"),
        "picker title/purpose hierarchy: {purpose_text}"
    );

    let mut message_empty = shell_for(NO_COLOR);
    let mut picker = MessagePicker::new(Vec::new());
    picker.surface.lifecycle = OrdinarySurfaceLifecycle::empty("fork points");
    message_empty.open_panel(Panel::MessagePicker { picker });
    let message_empty_text = rendered_panel(&message_empty, NO_COLOR.width);
    assert!(
        message_empty_text.matches("no matches").count() == 1
            && message_empty_text.contains("fork points"),
        "message picker duplicated or lost its typed empty status: {message_empty_text}"
    );

    let mut cancellation = shell_for(NO_COLOR);
    open_contract_picker(&mut cancellation);
    assert!(matches!(
        cancellation.panel_input(&Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))),
        Some((PanelResult::Cancel, PanelAction::SelectModel(_)))
    ));
    assert!(
        !cancellation.has_panel(),
        "escape must cancel rather than select"
    );

    let mut structural_blank = shell_for(NO_COLOR);
    structural_blank.open_panel(Panel::SelectList {
        surface: OrdinarySurfaceMetadata::new("Select model"),
        items: vec!["Atlas".into(), "Borealis".into()],
        descriptions: vec![None, Some("1M context; audio".into())],
        selected: 0,
        filter: String::new(),
        action: PanelAction::SelectModel(Vec::new()),
    });
    let blank_rows = plain_rows(&super::panel_render::render_panel(
        &structural_blank.state.borrow(),
        NO_COLOR.width,
    ));
    assert!(
        blank_rows.iter().any(String::is_empty),
        "model metadata rhythm must retain an intentional blank row: {blank_rows:?}"
    );

    let mut untrusted = shell_for(NO_COLOR);
    untrusted.open_panel(Panel::SelectList {
        surface: OrdinarySurfaceMetadata::new("\x1b[31mSelect\nmodel"),
        items: vec!["untrusted\x1b[2J\nlabel".into()],
        descriptions: vec![Some("metadata\x1b]8;;https://example.invalid\x1b\\".into())],
        selected: 0,
        filter: String::new(),
        action: PanelAction::SelectModel(Vec::new()),
    });
    let untrusted_raw =
        super::panel_render::render_panel(&untrusted.state.borrow(), NO_COLOR.width);
    let untrusted_text = plain_rows(&untrusted_raw).join("\n");
    assert!(
        untrusted_raw
            .iter()
            .all(|line| !line.replace(CURSOR_MARKER, "").contains('\x1b'))
            && untrusted_text.contains("Select model")
            && untrusted_text.contains("untrusted label"),
        "ordinary surface exposed unsanitized display text: raw={untrusted_raw:?}, plain={untrusted_text}"
    );
}
