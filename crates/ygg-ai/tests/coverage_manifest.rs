#![allow(missing_docs)]
//! Task 16.1 coverage manifest: proves the design §19 required fixture cases
//! exist on disk, enforces the fixture prohibitions (no invented
//! `choices[].delta.audio` / `response.audio.*` streaming audio), and confirms
//! the embedded catalog loads via `builtin()`.

use std::fs;
use std::path::{Path, PathBuf};

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Every required fixture case (design §19), keyed by protocol directory.
const REQUIRED: &[(&str, &[&str])] = &[
    (
        "openai_chat",
        &[
            "plain_text.sse",
            "reasoning.sse",
            "one_tool_call.sse",
            "parallel_tool_calls.sse",
            "malformed_tool_json.sse",
            "length_stop.sse",
            "premature_eof.sse",
            "audio_output.json", // completed (non-streaming) audio decode
        ],
    ),
    (
        "anthropic",
        &[
            "text.sse",
            "thinking.sse",
            "redacted_thinking.sse",
            "tool_call.sse",
            "parallel_tool_calls.sse",
            "malformed_tool_json.sse",
            "max_tokens.sse",
            "stop_sequence.sse",
            "pause_turn.sse",
            "error_event.sse",
            "thinking_usage.sse",
            "premature_eof.sse",
        ],
    ),
    (
        "openai_responses",
        &[
            "plain_text.sse",
            "reasoning_encrypted.sse",
            "tool_call.sse",
            "parallel_tool_calls.sse",
            "malformed_tool_json.sse",
            "incomplete_max_tokens.sse",
            "ignored_event.sse",
            "response_failed.sse",
            "stream_error.sse",
            "reasoning_opaque_no_text.sse",
            "completed_no_usage.sse",
            "premature_eof.sse",
        ],
    ),
];

#[test]
fn all_required_fixtures_exist() {
    let root = fixtures_root();
    for (proto, files) in REQUIRED {
        for f in *files {
            let p = root.join(proto).join(f);
            assert!(p.exists(), "missing required fixture: {}", p.display());
        }
    }
}

#[test]
fn no_streaming_fixture_invents_audio_events() {
    // The design §19 fixture prohibitions: streaming (`.sse`) fixtures must not
    // invent `choices[].delta.audio` or any `response.audio.*` event, neither of
    // which has documented streaming support. Chat audio output is only ever
    // tested through the non-streaming `message.audio` JSON body.
    let root = fixtures_root();
    for (proto, _) in REQUIRED {
        let dir = root.join(proto);
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("sse") {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            assert!(
                !text.contains("response.audio"),
                "forbidden response.audio.* event in {}",
                path.display()
            );
            assert!(
                !text.contains("\"audio\""),
                "forbidden streamed audio field in {}",
                path.display()
            );
        }
    }
}

#[test]
fn builtin_catalog_loads_offline() {
    let catalog =
        ygg_ai::ModelCatalog::builtin().expect("embedded catalog must parse and validate");
    assert!(
        catalog.models().next().is_some(),
        "builtin catalog must contain at least one model"
    );
}
