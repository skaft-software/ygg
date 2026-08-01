//! Session persistence smoke benchmark. Run with:
//! `cargo run --release --example session_bench --features benchmarks -p ygg-agent`

use std::hint::black_box;
use std::time::Instant;

use tempfile::tempdir;
use ygg_agent::{EntryValue, Session};
use ygg_ai::{AssistantMessage, AssistantPart, Message, ModelId, Protocol, UserMessage, UserPart};

fn user(text: String) -> EntryValue {
    EntryValue::Message(Message::User(UserMessage {
        content: vec![UserPart::Text(text)],
    }))
}

fn assistant(text: String) -> EntryValue {
    EntryValue::Message(Message::Assistant(AssistantMessage {
        content: vec![AssistantPart::Text(text)],
        model: ModelId("benchmark-model".to_owned()),
        protocol: Protocol::AnthropicMessages,
    }))
}

fn main() {
    const TURNS: usize = 250;
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("session.jsonl");
    let mut session = Session::create(&path).expect("create session");

    let append_start = Instant::now();
    for turn in 0..TURNS {
        session
            .append(user(format!(
                "Implement benchmark turn {turn}: preserve correctness."
            )))
            .expect("append user message");
        session
            .append(assistant(format!(
                "Completed turn {turn}; inspected files and ran tests."
            )))
            .expect("append assistant message");
    }
    let append_elapsed = append_start.elapsed();

    let cold_context_start = Instant::now();
    let cold_context_len = black_box(session.context_ref().expect("context").len());
    let cold_context_elapsed = cold_context_start.elapsed();

    let warm_context_start = Instant::now();
    let warm_context_len = black_box(session.context_ref().expect("cached context").len());
    let warm_context_elapsed = warm_context_start.elapsed();

    let file_bytes = std::fs::metadata(&path).expect("session metadata").len();
    drop(session);

    let open_start = Instant::now();
    let reopened = Session::open(&path).expect("reopen session");
    let open_elapsed = open_start.elapsed();
    let reopened_context_len = black_box(reopened.context_ref().expect("reopened context").len());

    assert_eq!(cold_context_len, warm_context_len);
    assert_eq!(cold_context_len, reopened_context_len);
    println!(
        "{TURNS} turns, {} entries, {file_bytes} bytes",
        reopened.entries().len()
    );
    println!("append: {append_elapsed:?}");
    println!("cold context reconstruction: {cold_context_elapsed:?}");
    println!("warm context cache: {warm_context_elapsed:?}");
    println!("reopen + replay: {open_elapsed:?}");
}
