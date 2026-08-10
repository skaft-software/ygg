#![cfg(unix)]

//! Process-boundary tests for the versioned `ygg-host` NDJSON contract.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const PROTOCOL_VERSION: u64 = 1;
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(20);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

struct HostProcess {
    child: Child,
    input: Option<ChildStdin>,
    output: Receiver<String>,
}

impl HostProcess {
    fn spawn(home: &std::path::Path, workspace: &std::path::Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ygg-host"));
        command
            .current_dir(workspace)
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().expect("spawn ygg-host");
        let input = child.stdin.take().expect("host stdin");
        let stdout = child.stdout.take().expect("host stdout");
        let stderr = child.stderr.take().expect("host stderr");
        let (sender, output) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        std::thread::spawn(move || {
            let mut stderr = BufReader::new(stderr);
            let mut sink = Vec::new();
            let _ = stderr.by_ref().take(256 * 1024).read_to_end(&mut sink);
        });
        Self {
            child,
            input: Some(input),
            output,
        }
    }

    fn send(&mut self, request: &Value) {
        let line = serde_json::to_vec(request).expect("serialize request");
        assert!(line.len() < MAX_FRAME_BYTES);
        self.send_raw(&line);
    }

    fn send_raw(&mut self, line: &[u8]) {
        let input = self.input.as_mut().expect("host stdin is open");
        input.write_all(line).expect("write host request");
        input.write_all(b"\n").expect("terminate host request");
        input.flush().expect("flush host request");
    }

    fn recv(&self) -> Value {
        let line = self
            .output
            .recv_timeout(MESSAGE_TIMEOUT)
            .expect("timed out waiting for ygg-host");
        assert!(line.len() < MAX_FRAME_BYTES, "oversized frame");
        serde_json::from_str(&line).expect("valid host JSON")
    }

    fn close_input(&mut self) {
        drop(self.input.take());
    }

    fn send_unterminated_and_close(&mut self, bytes: &[u8]) {
        let mut input = self.input.take().expect("host stdin is open");
        input.write_all(bytes).expect("write unterminated request");
        input.flush().expect("flush unterminated request");
        drop(input);
    }

    fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + EXIT_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll host") {
                return status;
            }
            assert!(Instant::now() < deadline, "ygg-host did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn terminate_group(&mut self) {
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGTERM);
        }
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.wait();
        }
    }
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("temp root");
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&home).expect("home");
    std::fs::create_dir(&workspace).expect("workspace");
    (root, home, workspace)
}

fn assert_envelope(message: &Value, request_id: &str, sequence: u64, kind: &str) {
    assert_eq!(message["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(message["request_id"], request_id);
    assert_eq!(message["seq"], sequence);
    assert_eq!(message["type"], kind);
}

#[test]
fn handshake_resynchronization_shutdown_and_eof() {
    let (_root, home, workspace) = fixture();
    let mut host = HostProcess::spawn(&home, &workspace);

    host.send(&json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": "hello-1",
        "command": "hello"
    }));
    let hello = host.recv();
    assert_envelope(&hello, "hello-1", 1, "hello");
    assert_eq!(hello["data"]["max_frame_bytes"], MAX_FRAME_BYTES);
    assert_eq!(hello["data"]["max_concurrent_runs"], 1);
    assert_eq!(hello["data"]["features"]["typed_media_input"], true);
    assert_eq!(hello["data"]["features"]["typed_audio_input"], true);
    assert_eq!(hello["data"]["features"]["prompt_display_text"], true);
    assert_eq!(hello["data"]["features"]["in_band_abort"], false);

    host.send_raw(b"{not-json}");
    let malformed = host.recv();
    assert_envelope(&malformed, "invalid", 1, "protocol_error");

    host.send(&json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": "unknown-field",
        "command": "hello",
        "future_option": true
    }));
    let unknown = host.recv();
    assert_envelope(&unknown, "invalid", 1, "protocol_error");
    assert!(unknown["data"]["error"]
        .as_str()
        .is_some_and(|error| error.contains("unknown request field")));

    host.send_raw(&vec![b'x'; MAX_FRAME_BYTES + 1]);
    let oversized = host.recv();
    assert_envelope(&oversized, "invalid", 1, "protocol_error");

    host.send(&json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": "hello-2",
        "command": "hello"
    }));
    assert_envelope(&host.recv(), "hello-2", 1, "hello");

    host.send(&json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": "bad-run",
        "command": "run",
        "run_id": "run-1",
        "workspace": workspace,
        "model": "local-test",
        "base_url": "https://example.com/v1",
        "provider_mode": "unsupported",
        "prompt": "test"
    }));
    let rejected = host.recv();
    assert_envelope(&rejected, "bad-run", 1, "final_result");
    assert_eq!(rejected["data"]["status"], "error");
    assert!(rejected["data"]["error"]
        .as_str()
        .is_some_and(|error| error.contains("unsupported provider_mode")));

    host.send(&json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": "shutdown-1",
        "command": "shutdown"
    }));
    assert_envelope(&host.recv(), "shutdown-1", 1, "shutdown");
    host.close_input();
    assert!(host.wait().success());

    let mut eof_host = HostProcess::spawn(&home, &workspace);
    eof_host.close_input();
    assert!(eof_host.wait().success());
}

#[test]
fn rejects_duplicate_fields_and_unterminated_eof_frames() {
    let (_root, home, workspace) = fixture();
    let mut host = HostProcess::spawn(&home, &workspace);

    host.send_raw(
        br#"{"protocol_version":999,"protocol_version":1,"request_id":"dup","command":"hello"}"#,
    );
    let duplicate = host.recv();
    assert_envelope(&duplicate, "invalid", 1, "protocol_error");
    assert!(duplicate["data"]["error"]
        .as_str()
        .is_some_and(|error| error.contains("duplicate JSON field")));

    host.send_raw(
        br#"{"protocol_version":1,"request_id":"nested-dup","command":"run","run_id":"run","workspace":"/tmp","model":"test","prompt":"test","custom_headers":{"x-test":"one","x-test":"two"}}"#,
    );
    let nested_duplicate = host.recv();
    assert_envelope(&nested_duplicate, "invalid", 1, "protocol_error");
    assert!(nested_duplicate["data"]["error"]
        .as_str()
        .is_some_and(|error| error.contains("duplicate JSON field")));

    host.send_unterminated_and_close(
        br#"{"protocol_version":1,"request_id":"partial","command":"hello"}"#,
    );
    let incomplete = host.recv();
    assert_envelope(&incomplete, "invalid", 1, "protocol_error");
    assert_eq!(
        incomplete["data"]["error"],
        "incomplete protocol frame at EOF"
    );
    assert!(host.wait().success());
}

#[derive(Debug)]
struct HttpRequest {
    head: String,
    body: String,
}

fn spawn_openai_fixture(
    responses: &'static [&'static str],
) -> (String, Receiver<HttpRequest>, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let address = listener.local_addr().expect("provider address");
    let (sender, receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        for marker in responses {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("provider read timeout");
            let request = read_http_request(&mut stream);
            sender.send(request).expect("capture provider request");
            let chunks = format!(
                "data: {{\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"local-test\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":{marker:?}}},\"finish_reason\":null}}]}}\n\n\
                 data: {{\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"local-test\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13}}}}\n\n\
                 data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                chunks.len(),
                chunks
            );
            stream
                .write_all(response.as_bytes())
                .expect("write provider response");
        }
    });
    (format!("http://{address}/v1"), receiver, handle)
}

fn read_http_request(stream: &mut std::net::TcpStream) -> HttpRequest {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer).expect("read provider request");
        assert!(read > 0, "provider request ended before headers");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        assert!(
            bytes.len() < MAX_FRAME_BYTES,
            "provider headers are unbounded"
        );
    };
    let head = String::from_utf8(bytes[..header_end].to_vec()).expect("UTF-8 provider headers");
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .unwrap_or(0);
    while bytes.len() - header_end < content_length {
        let read = stream.read(&mut buffer).expect("read provider body");
        assert!(read > 0, "provider request body ended early");
        bytes.extend_from_slice(&buffer[..read]);
    }
    HttpRequest {
        head,
        body: String::from_utf8(bytes[header_end..header_end + content_length].to_vec())
            .expect("UTF-8 provider body"),
    }
}

fn run_request(
    request_id: &str,
    run_id: &str,
    workspace: &std::path::Path,
    sessions: &std::path::Path,
    base_url: &str,
    resume_session: Option<&str>,
) -> Value {
    let mut request = json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": request_id,
        "command": "run",
        "run_id": run_id,
        "session_id": "session-1",
        "workspace": workspace,
        "session_dir": sessions,
        "model": "local-test",
        "provider": "local",
        "base_url": base_url,
        "api_key": "test-only",
        "custom_headers": {"x-ygg-host-test": "present"},
        "provider_mode": "openai-compatible",
        "context_window_tokens": 8192,
        "max_output_tokens": 1024,
        "prompt": "Reply with the fixture marker.",
        "system_prompt": "This is a deterministic protocol test.",
        "tools": [],
        "allow_file_mutation": false,
        "context_files": false,
        "max_turns": 1,
        "history": [{"role": "assistant", "text": "Prior fixture context."}]
    });
    if let Some(path) = resume_session {
        request["resume_session"] = Value::String(path.to_owned());
    }
    request
}

#[test]
fn inline_provider_run_streams_and_resumes_a_native_session() {
    let (_root, home, workspace) = fixture();
    let sessions = workspace.join("sessions");
    let (base_url, provider_requests, provider) =
        spawn_openai_fixture(&["HOST_RUN_ONE", "HOST_RUN_TWO"]);
    let mut host = HostProcess::spawn(&home, &workspace);

    let mut first_request =
        run_request("request-1", "run-1", &workspace, &sessions, &base_url, None);
    first_request["prompt_display_text"] = json!("Visible first caller text.");
    host.send(&first_request);
    let mut sequence = 0;
    let mut kinds = Vec::new();
    let first_result = loop {
        let message = host.recv();
        sequence += 1;
        assert_eq!(message["request_id"], "request-1");
        assert_eq!(message["run_id"], "run-1");
        assert_eq!(message["session_id"], "session-1");
        assert_eq!(message["seq"], sequence);
        kinds.push(message["type"].as_str().unwrap().to_owned());
        if message["type"] == "final_result" {
            break message;
        }
    };
    assert!(kinds.starts_with(&["accepted".to_owned()]));
    assert!(kinds.contains(&"started".to_owned()));
    assert!(kinds.contains(&"model_delta".to_owned()));
    assert!(kinds.contains(&"model_step".to_owned()));
    assert_eq!(first_result["data"]["status"], "completed");
    assert_eq!(first_result["data"]["output"], "HOST_RUN_ONE");
    let session_file = first_result["data"]["sessionFile"]
        .as_str()
        .expect("session file")
        .to_owned();
    assert!(std::path::Path::new(&session_file).is_file());

    let mut second_request = run_request(
        "request-2",
        "run-2",
        &workspace,
        &sessions,
        &base_url,
        Some(&session_file),
    );
    second_request["prompt_display_text"] = json!("");
    host.send(&second_request);
    let second_result = loop {
        let message = host.recv();
        if message["type"] == "final_result" {
            break message;
        }
    };
    assert_eq!(second_result["data"]["status"], "completed");
    assert_eq!(second_result["data"]["output"], "HOST_RUN_TWO");
    assert_eq!(second_result["data"]["sessionFile"], session_file);

    host.send(&json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": "shutdown",
        "command": "shutdown"
    }));
    assert_envelope(&host.recv(), "shutdown", 1, "shutdown");
    host.close_input();
    assert!(host.wait().success());

    let session = ygg_agent::Session::open_read_only(&session_file).expect("reopen host session");
    let display_texts = session
        .entries()
        .iter()
        .filter_map(|entry| {
            entry
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.display_text.as_deref())
        })
        .collect::<Vec<_>>();
    assert_eq!(display_texts, ["Visible first caller text.", ""]);

    let first_provider_request = provider_requests
        .recv_timeout(MESSAGE_TIMEOUT)
        .expect("first provider request");
    let second_provider_request = provider_requests
        .recv_timeout(MESSAGE_TIMEOUT)
        .expect("second provider request");
    for request in [&first_provider_request, &second_provider_request] {
        let head = request.head.to_ascii_lowercase();
        assert!(head.starts_with("post /v1/chat/completions http/1.1"));
        assert!(head.contains("authorization: bearer test-only"));
        assert!(head.contains("x-ygg-host-test: present"));
        assert!(request.body.contains("local-test"));
        assert!(!request.body.contains("Visible first caller text."));
    }
    assert!(first_provider_request
        .body
        .contains("Prior fixture context."));
    provider.join().expect("provider fixture");
}

#[test]
fn ordered_image_and_audio_cross_the_process_and_provider_boundaries() {
    let (_root, home, workspace) = fixture();
    let sessions = workspace.join("sessions");
    let image_one = workspace.join("first.png");
    let audio = workspace.join("music.wav");
    let image_two = workspace.join("second.jpg");
    std::fs::write(&image_one, b"first-image").unwrap();
    std::fs::write(&audio, b"raw-audio").unwrap();
    std::fs::write(&image_two, b"second-image").unwrap();
    let (base_url, provider_requests, provider) = spawn_openai_fixture(&["MEDIA_OK"]);
    let mut request = run_request(
        "media-request",
        "media-run",
        &workspace,
        &sessions,
        &base_url,
        None,
    );
    request["context_window_tokens"] = json!(32_768);
    request["input_modalities"] = json!(["image", "audio"]);
    request["media"] = json!([
        {"type": "image", "path": image_one},
        {"type": "audio", "path": audio},
        {"type": "image", "path": image_two},
    ]);

    let mut host = HostProcess::spawn(&home, &workspace);
    host.send(&request);
    let result = loop {
        let message = host.recv();
        if message["type"] == "final_result" {
            break message;
        }
    };
    assert_eq!(
        result["data"]["status"], "completed",
        "unexpected host result: {result}"
    );
    assert_eq!(result["data"]["output"], "MEDIA_OK");

    host.send(&json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": "shutdown",
        "command": "shutdown"
    }));
    assert_envelope(&host.recv(), "shutdown", 1, "shutdown");
    host.close_input();
    assert!(host.wait().success());

    let captured = provider_requests
        .recv_timeout(MESSAGE_TIMEOUT)
        .expect("media provider request");
    let body: Value = serde_json::from_str(&captured.body).expect("provider request JSON");
    let content = body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_array())
        .expect("typed user content");
    let types = content
        .iter()
        .map(|part| part["type"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(types, ["text", "image_url", "input_audio", "image_url"]);
    assert_eq!(content[2]["input_audio"]["format"], "wav");
    assert_eq!(
        content[2]["input_audio"]["data"],
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"raw-audio")
    );
    provider.join().expect("provider fixture");
}

#[test]
fn coordinated_signal_cleanup_cancels_run_and_separate_extension_group() {
    let (_root, home, workspace) = fixture();
    let sessions = workspace.join("sessions");
    let extension_root = workspace.join("extensions");
    let extension_directory = extension_root.join("hung-extension");
    let extension_manifest = extension_directory.join("extension.toml");
    let extension_script = extension_directory.join("hung.sh");
    let extension_pid_file = workspace.join("extension.pid");
    std::fs::create_dir_all(&extension_directory).expect("extension directory");
    std::fs::write(
        &extension_manifest,
        r#"name = "hung-extension"
version = "0.1.0"
api_version = "0.1"

[entrypoint]
command = "hung.sh"
"#,
    )
    .expect("extension manifest");
    std::fs::write(
        &extension_script,
        r#"#!/bin/sh
printf '%s\n' "$$" > "$YGG_WORKSPACE/extension.pid"
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[],"commands":[]}}'
trap '' TERM
while IFS= read -r request; do
    sleep 30
done
"#,
    )
    .expect("extension script");
    let mut permissions = std::fs::metadata(&extension_script)
        .expect("extension metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&extension_script, permissions).expect("extension permissions");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind hanging provider");
    let address = listener.local_addr().unwrap();
    let (accepted_sender, accepted_receiver) = mpsc::channel();
    let provider = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept hanging request");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let _request = read_http_request(&mut stream);
        accepted_sender.send(()).unwrap();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => return,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return;
                }
                Err(_) => return,
            }
        }
    });

    let mut host = HostProcess::spawn(&home, &workspace);
    let mut request = run_request(
        "cancel-request",
        "cancel-run",
        &workspace,
        &sessions,
        &format!("http://{address}/v1"),
        None,
    );
    request["allow_file_mutation"] = json!(true);
    request["extension_paths"] = json!([extension_root]);
    request["enabled_extensions"] = json!(["hung-extension"]);
    request["trusted_extensions"] =
        json!([format!("hung-extension@{}", extension_manifest.display())]);
    host.send(&request);
    accepted_receiver
        .recv_timeout(MESSAGE_TIMEOUT)
        .expect("provider request was not started");
    let extension_pid = std::fs::read_to_string(&extension_pid_file)
        .expect("extension pid file")
        .trim()
        .parse::<i32>()
        .expect("extension pid");
    assert!(process_is_alive(extension_pid));

    host.terminate_group();
    let status = host.wait();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    let deadline = Instant::now() + EXIT_TIMEOUT;
    while process_is_alive(extension_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !process_is_alive(extension_pid),
        "extension process group survived host signal cleanup"
    );
    provider.join().expect("hanging provider fixture");
}

fn process_is_alive(pid: i32) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
