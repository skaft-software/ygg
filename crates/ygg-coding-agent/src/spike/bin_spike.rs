#![allow(missing_docs)]

use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use sexy_tui_rs::widgets::{Markdown, MarkdownOptions};
use sexy_tui_rs::{RenderOptions, RichRenderer, TerminalCapabilities, Theme};

#[allow(dead_code)]
#[path = "../tui/terminal.rs"]
mod terminal;
use terminal::{force_restore, install_panic_hook, YggTerminal};

fn markdown(text: &str) -> Box<Markdown> {
    let capabilities = TerminalCapabilities::detect();
    Box::new(Markdown::with_renderer(
        text,
        MarkdownOptions {
            padding_x: 1,
            padding_y: 1,
        },
        RichRenderer::new(
            Theme::with_capabilities(capabilities),
            capabilities,
            RenderOptions::default(),
        ),
    ))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    install_panic_hook();
    let (terminal, _size) = YggTerminal::enter()?;
    let mut tui = sexy_tui_rs::TUI::new(Box::new(terminal));
    tui.add_child(markdown(
        "# spike\ntype; `q` or Ctrl+C quits; resize the window.",
    ));
    tui.start();

    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(80));
    let mut buffer = String::from("# spike\n");
    // Do not write diagnostics while sexy-tui owns the cursor: doing so would
    // invalidate its differential-rendering cursor bookkeeping. Print them
    // after the TUI has restored the terminal.
    let mut key_log = Vec::new();

    loop {
        tokio::select! {
            maybe = input.next() => match maybe {
                Some(Ok(Event::Key(key))) => {
                    key_log.push(format!("key event: {key:?}"));
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                        break;
                    }
                    if key.code == KeyCode::Char('q') && key.modifiers.is_empty() {
                        break;
                    }
                    if key.code == KeyCode::Char('p') && key.modifiers.is_empty() {
                        panic!("spike panic restoration check");
                    }
                    if let KeyCode::Char(c) = key.code {
                        if key.modifiers.is_empty() {
                            buffer.push(c);
                            tui.remove_child(0);
                            tui.add_child(markdown(&buffer));
                            tui.request_render();
                        }
                    }
                }
                Some(Ok(Event::Paste(text))) => {
                    key_log.push(format!("paste event: {text:?}"));
                }
                Some(Ok(Event::Resize(columns, rows))) => {
                    key_log.push(format!("resize event: {columns}x{rows}"));
                    tui.request_render();
                }
                Some(Err(error)) => {
                    key_log.push(format!("input error: {error}"));
                    break;
                }
                None => break,
                _ => {}
            },
            _ = ticker.tick() => tui.request_render(),
        }
    }

    tui.stop();
    force_restore();
    for line in key_log {
        println!("{line}");
    }
    Ok(())
}
