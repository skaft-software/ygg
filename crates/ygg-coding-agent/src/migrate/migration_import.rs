#![allow(missing_docs)]

//! Typed, read-only Pi adapter and host-owned migration ingestion.
//!
//! The adapter process is deliberately unable to persist anything: it receives
//! one source root over API 0.3 and returns bounded non-secret values. This
//! module owns all destination reads, conflict decisions, backups, and writes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, IsTerminal as _, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest as _, Sha256};
use ygg_agent::extension_api_v03 as api;
use ygg_ai::{ModelCatalog, ModelId};
use ygg_migrate_types::{
    Diagnostic as SetupDiagnostic, DiagnosticSeverity, McpServer, McpTransport, MigratedSetup,
    MigrationOutcome, Model, Skill,
};

use super::{absolute_path, MigrationAdapterCommand, MigrationImportCommand};

const ADAPTER_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_MCP_CONFIG_BYTES: usize = 256 * 1024;
const MAX_SKILL_BYTES: usize = 128 * 1024;
const MAX_STATE_BYTES: usize = 256 * 1024;
const MAX_BACKUP_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_SOURCE_ITEMS: usize = 128;
const MIGRATION_STATE_VERSION: u32 = 1;
const BACKUP_VERSION: u32 = 1;
const CONFIG_CANDIDATES: &[&str] = &[
    "settings.json",
    "config.json",
    ".pi/settings.json",
    ".pi/agent/settings.json",
];
const SKILL_ROOT_CANDIDATES: &[&str] = &["skills", ".pi/skills", ".pi/agent/skills"];

/// Dispatch the public `ygg migrate import ...` command.
pub(crate) fn run_import(
    command: MigrationImportCommand,
    invocation_cwd: &Path,
) -> anyhow::Result<()> {
    match command {
        MigrationImportCommand::Pi {
            source,
            yes,
            dry_run,
            json,
        } => run_import_pi(source, yes, dry_run, json, invocation_cwd),
    }
}

/// Dispatch the intentionally hidden, current-binary Pi adapter entrypoint.
pub(crate) fn run_adapter(command: MigrationAdapterCommand) -> anyhow::Result<()> {
    match command {
        MigrationAdapterCommand::Pi => run_pi_adapter_stdio(),
    }
}

/// Restore a migration backup after checking that the destination still matches
/// the import it backs up. `force` is an explicit user override for changed
/// destinations.
pub(crate) fn run_restore(
    backup: PathBuf,
    force: bool,
    invocation_cwd: &Path,
) -> anyhow::Result<()> {
    let home = migration_home()?;
    let backup = absolute_path(&backup, invocation_cwd)?;
    let paths = MigrationPaths::new(home)?;
    let lock = MigrationLock::acquire(&paths)?;
    let restored = restore_backup(&paths, &backup, force)?;
    lock.release()?;
    crate::output::stdout_line(format!(
        "Restored {restored} migration target(s) from {}.",
        backup.display()
    ));
    Ok(())
}

fn run_import_pi(
    source: Option<PathBuf>,
    yes: bool,
    dry_run: bool,
    json_output: bool,
    invocation_cwd: &Path,
) -> anyhow::Result<()> {
    let home = migration_home()?;
    let explicit_source = source.is_some();
    let source = match source {
        Some(source) => absolute_path(&source, invocation_cwd)?,
        None => default_pi_source(&home)?,
    };
    if !source.is_dir() {
        if explicit_source {
            anyhow::bail!("Pi source directory does not exist: {}", source.display())
        }
        emit_no_source_report(&source, json_output);
        return Ok(());
    }
    let paths = MigrationPaths::new(home)?;
    let lock = MigrationLock::acquire(&paths)?;

    let mut adapter = AdapterClient::start()?;
    let detected = adapter.detect(&source)?;
    if !detected.detected {
        adapter.shutdown();
        lock.release()?;
        emit_no_source_report(&source, json_output);
        return Ok(());
    }
    let imported = adapter.import(&source, &detected.config_paths)?;
    adapter.shutdown();
    let setup = normalize_adapter_result(imported)?;

    let preview = build_ingestion_plan(&paths, &setup, false)?;
    if dry_run {
        lock.release()?;
        emit_import_report(&source, &preview, None, true, json_output);
        return Ok(());
    }
    if !preview.conflicts.is_empty() && !confirm_conflicts(&preview.conflicts, yes)? {
        lock.release()?;
        anyhow::bail!("migration cancelled; no files were changed")
    }

    let plan = if preview.conflicts.is_empty() {
        preview
    } else {
        build_ingestion_plan(&paths, &setup, true)?
    };
    let backup = apply_ingestion_plan(&paths, &plan)?;
    lock.release()?;
    emit_import_report(&source, &plan, backup.as_deref(), false, json_output);
    Ok(())
}

fn migration_home() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("home directory is unavailable"))?;
    let current = std::env::current_dir()?;
    absolute_path(&home, &current)
}

fn default_pi_source(home: &Path) -> anyhow::Result<PathBuf> {
    if let Some(value) = std::env::var_os("PI_CODING_AGENT_DIR") {
        return absolute_path(Path::new(&value), &std::env::current_dir()?);
    }

    let mut candidates = vec![home.join(".pi/agent"), home.join(".config/pi/agent")];
    #[cfg(target_os = "macos")]
    candidates.push(home.join("Library/Application Support/pi/agent"));
    let selected = candidates
        .iter()
        .find(|candidate| candidate.is_dir())
        .cloned()
        .unwrap_or_else(|| candidates.remove(0));
    absolute_path(&selected, &std::env::current_dir()?)
}

#[derive(Clone, Debug)]
struct MigrationPaths {
    home: PathBuf,
    config: PathBuf,
    mcp: PathBuf,
    skills: PathBuf,
    state: PathBuf,
    lock: PathBuf,
    backups: PathBuf,
}

impl MigrationPaths {
    fn new(home: PathBuf) -> anyhow::Result<Self> {
        if !home.is_absolute() {
            anyhow::bail!("migration home must be absolute")
        }
        let ygg = home.join(".ygg");
        Ok(Self {
            config: ygg.join("config.toml"),
            mcp: ygg.join("mcp.json"),
            skills: ygg.join("skills"),
            state: ygg.join("migrations/pi-state.json"),
            lock: ygg.join("migrations/pi-import.lock"),
            backups: ygg.join("backups/migrate"),
            home,
        })
    }
}

struct MigrationLock {
    path: PathBuf,
    file: fs::File,
    identity: ygg_agent::secure_fs::PrivateLockIdentity,
}

impl MigrationLock {
    fn acquire(paths: &MigrationPaths) -> anyhow::Result<Self> {
        let file = ygg_agent::secure_fs::open_private_lock_file(&paths.lock).map_err(|error| {
            anyhow::anyhow!(
                "cannot create the private migration lock {}: {error}",
                paths.lock.display()
            )
        })?;
        file.try_lock_exclusive().map_err(|error| {
            anyhow::anyhow!("another migration is already updating this Ygg home: {error}")
        })?;
        let identity =
            ygg_agent::secure_fs::validate_private_lock_after_acquire(&paths.lock, &file)
                .map_err(|error| anyhow::anyhow!("cannot validate migration lock: {error}"))?;
        Ok(Self {
            path: paths.lock.clone(),
            file,
            identity,
        })
    }

    fn release(self) -> anyhow::Result<()> {
        ygg_agent::secure_fs::revalidate_private_lock_before_release(
            &self.path,
            &self.file,
            &self.identity,
        )
        .map_err(|error| anyhow::anyhow!("migration lock changed while held: {error}"))?;
        fs2::FileExt::unlock(&self.file)?;
        Ok(())
    }
}

/// A synchronous, bounded API 0.3 client for the current binary's read-only
/// adapter mode. It intentionally accepts no user-selected command or adapter
/// path, so source data cannot turn migration into arbitrary process execution.
struct AdapterClient {
    child: Child,
    stdin: ChildStdin,
    responses: mpsc::Receiver<anyhow::Result<String>>,
    next_id: u64,
    contract: api::NegotiatedContract,
}

impl AdapterClient {
    fn start() -> anyhow::Result<Self> {
        let executable = std::env::current_exe()
            .map_err(|error| anyhow::anyhow!("cannot locate the current Ygg binary: {error}"))?;
        let mut command = Command::new(executable);
        command
            .args(["migrate", "adapter", "pi"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_clear();
        command.envs(ygg_agent::extension_process::sanitized_subprocess_environment());
        let mut child = command.spawn().map_err(|error| {
            anyhow::anyhow!("could not start the built-in Pi migration adapter: {error}")
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("adapter stdin was not available"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("adapter stdout was not available"))?;
        let (sender, responses) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let result = read_bounded_adapter_line(&mut reader)
                    .map_err(|error| anyhow::anyhow!("cannot read adapter response: {error}"));
                match result {
                    Ok(Some(line)) => {
                        if sender.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });

        let offer = api::host_offer(api::MAX_FRAME_BYTES, 1)
            .map_err(|error| anyhow::anyhow!("cannot create migration adapter offer: {error}"))?;
        let mut client = Self {
            child,
            stdin,
            responses,
            next_id: 1,
            contract: api::NegotiatedContract {
                capabilities: BTreeSet::new(),
                methods: BTreeSet::new(),
                limits: api::ProtocolLimits {
                    max_frame_bytes: api::MAX_FRAME_BYTES,
                    max_concurrent_requests: 1,
                    max_tools: 0,
                },
            },
        };
        let request = api::InitializeRequest {
            api_version: api::API_VERSION.to_owned(),
            ygg_version: env!("CARGO_PKG_VERSION").to_owned(),
            extension: json!({"name":"ygg-import-pi","version":"1"}),
            workspace: "/migration-source".to_owned(),
            capabilities: json!({"filesystem":"source_read_only","network":false,"process":false}),
            contributes: json!({}),
            flag_values: Vec::new(),
            host: json!({"migration_ingestion":"host_owned"}),
            contract: offer.clone(),
        };
        api::validate_initialize_request(&request)
            .map_err(|error| anyhow::anyhow!("invalid adapter initialization request: {error}"))?;
        let response = client.call("initialize", serde_json::to_value(request)?)?;
        let response = api::parse_initialize_response(response).map_err(|error| {
            anyhow::anyhow!("adapter returned invalid initialization response: {error}")
        })?;
        let contract = api::negotiate(&offer, &response.contract)
            .map_err(|error| anyhow::anyhow!("adapter contract negotiation failed: {error}"))?;
        for method in ["migration/detect", "migration/import"] {
            api::require_method(&contract, method, api::MethodDirection::HostToExtension)
                .map_err(|error| anyhow::anyhow!("adapter does not provide {method}: {error}"))?;
        }
        client.contract = contract;
        Ok(client)
    }

    fn detect(&mut self, source: &Path) -> anyhow::Result<api::MigrationDetectResult> {
        let params = api::MigrationDetectParams {
            source_root: path_to_utf8(source, "migration source")?,
        };
        let value = self.call("migration/detect", serde_json::to_value(params)?)?;
        api::parse_migration_detect_result(value)
            .map_err(|error| anyhow::anyhow!("adapter returned invalid detect result: {error}"))
    }

    fn import(
        &mut self,
        source: &Path,
        config_paths: &[String],
    ) -> anyhow::Result<api::MigrationImportResult> {
        let params = api::MigrationImportParams {
            source_root: path_to_utf8(source, "migration source")?,
            config_paths: config_paths.to_vec(),
        };
        let value = self.call("migration/import", serde_json::to_value(params)?)?;
        api::parse_migration_import_result(value)
            .map_err(|error| anyhow::anyhow!("adapter returned invalid import result: {error}"))
    }

    fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        // Initialization is the one request sent before a negotiated contract
        // exists. Every later request must be explicitly selected.
        if method != "initialize" {
            api::require_method(
                &self.contract,
                method,
                api::MethodDirection::HostToExtension,
            )
            .map_err(|error| anyhow::anyhow!("adapter method {method} is unavailable: {error}"))?;
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let frame = api::canonical_frame(&request, api::MAX_FRAME_BYTES)
            .map_err(|error| anyhow::anyhow!("cannot encode adapter request: {error}"))?;
        self.stdin.write_all(frame.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        let raw = match self.responses.recv_timeout(ADAPTER_TIMEOUT) {
            Ok(result) => result?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = self.child.kill();
                anyhow::bail!("Pi migration adapter timed out")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("Pi migration adapter exited before responding")
            }
        };
        let envelope = parse_canonical_adapter_frame(&raw)?;
        match envelope {
            api::JsonRpcEnvelope::SuccessResponse(response) => {
                if response.id != api::JsonRpcId::Number(id) {
                    anyhow::bail!("Pi migration adapter returned a response with an unexpected id")
                }
                Ok(response.result)
            }
            api::JsonRpcEnvelope::ErrorResponse(response) => {
                if response.id != api::JsonRpcId::Number(id) {
                    anyhow::bail!("Pi migration adapter returned an error with an unexpected id")
                }
                anyhow::bail!(
                    "Pi migration adapter rejected {method} with protocol code {}",
                    response.error.code
                )
            }
            _ => anyhow::bail!("Pi migration adapter returned a non-response frame"),
        }
    }

    fn shutdown(&mut self) {
        let _ = self.call("shutdown", json!({}));
        let _ = self.child.wait();
    }
}

impl Drop for AdapterClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_canonical_adapter_frame(raw: &str) -> anyhow::Result<api::JsonRpcEnvelope> {
    if raw.is_empty() || raw.len() > api::MAX_FRAME_BYTES {
        anyhow::bail!("adapter response is empty or exceeds the API 0.3 frame limit")
    }
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| anyhow::anyhow!("adapter response is not valid JSON"))?;
    let canonical = api::canonical_json(&value)
        .map_err(|error| anyhow::anyhow!("adapter response is not canonical: {error}"))?;
    if canonical != raw {
        anyhow::bail!("adapter response is not canonical API 0.3 JSON")
    }
    api::parse_json_rpc_envelope(value).map_err(|error| {
        anyhow::anyhow!("adapter response has an invalid JSON-RPC envelope: {error}")
    })
}

fn read_bounded_adapter_line(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::with_capacity(api::MAX_FRAME_BYTES.min(4096));
    loop {
        let (consumed, newline) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if bytes.is_empty() {
                    return Ok(None);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "adapter frame is not newline terminated",
                ));
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index.saturating_add(1));
            let payload = consumed.saturating_sub(usize::from(newline.is_some()));
            if bytes.len().saturating_add(payload) > api::MAX_FRAME_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "adapter frame exceeds the API 0.3 limit",
                ));
            }
            bytes.extend_from_slice(&available[..payload]);
            (consumed, newline.is_some())
        };
        reader.consume(consumed);
        if newline {
            return String::from_utf8(bytes).map(Some).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "adapter frame is not UTF-8",
                )
            });
        }
    }
}

fn run_pi_adapter_stdio() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut initialized: Option<api::NegotiatedContract> = None;

    let mut reader = stdin.lock();
    while let Some(line) = read_bounded_adapter_line(&mut reader)? {
        let request = match parse_canonical_adapter_frame(&line) {
            Ok(api::JsonRpcEnvelope::Request(request)) => request,
            Ok(_) | Err(_) => {
                write_adapter_error(&mut writer, api::JsonRpcId::Number(0), "invalid_request")?;
                continue;
            }
        };
        let id = request.id.clone();
        let mut should_shutdown = false;
        let response = match request.method.as_str() {
            "initialize" => (|| -> anyhow::Result<Value> {
                let request = api::parse_initialize_request(request.params)
                    .map_err(|error| anyhow::anyhow!("invalid initialize request: {error}"))?;
                api::validate_initialize_request(&request)
                    .map_err(|error| anyhow::anyhow!("invalid initialize request: {error}"))?;
                let mut selection = api::select_required(&request.contract).map_err(|error| {
                    anyhow::anyhow!("could not select adapter contract: {error}")
                })?;
                selection
                    .capabilities
                    .push("migration.adapter.v1".to_owned());
                selection
                    .methods
                    .extend(["migration/detect".to_owned(), "migration/import".to_owned()]);
                let contract = api::negotiate(&request.contract, &selection)
                    .map_err(|error| anyhow::anyhow!("invalid adapter contract: {error}"))?;
                initialized = Some(contract);
                serde_json::to_value(api::InitializeResponse {
                    api_version: api::API_VERSION.to_owned(),
                    tools: Vec::new(),
                    contract: selection,
                })
                .map_err(Into::into)
            })(),
            "migration/detect" => (|| -> anyhow::Result<Value> {
                let contract = initialized
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("adapter is not initialized"))?;
                api::require_method(
                    contract,
                    "migration/detect",
                    api::MethodDirection::HostToExtension,
                )
                .map_err(|error| anyhow::anyhow!("unnegotiated migration/detect: {error}"))?;
                let params = api::parse_migration_detect_params(request.params)
                    .map_err(|error| anyhow::anyhow!("invalid detect parameters: {error}"))?;
                serde_json::to_value(pi_detect(Path::new(&params.source_root))?).map_err(Into::into)
            })(),
            "migration/import" => (|| -> anyhow::Result<Value> {
                let contract = initialized
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("adapter is not initialized"))?;
                api::require_method(
                    contract,
                    "migration/import",
                    api::MethodDirection::HostToExtension,
                )
                .map_err(|error| anyhow::anyhow!("unnegotiated migration/import: {error}"))?;
                let params = api::parse_migration_import_params(request.params)
                    .map_err(|error| anyhow::anyhow!("invalid import parameters: {error}"))?;
                serde_json::to_value(pi_import(
                    Path::new(&params.source_root),
                    &params.config_paths,
                )?)
                .map_err(Into::into)
            })(),
            "shutdown" => (|| -> anyhow::Result<Value> {
                let params = api::parse_shutdown_params(request.params)
                    .map_err(|error| anyhow::anyhow!("invalid shutdown parameters: {error}"))?;
                api::validate_shutdown_params(&params)
                    .map_err(|error| anyhow::anyhow!("invalid shutdown parameters: {error}"))?;
                should_shutdown = true;
                serde_json::to_value(api::ShutdownResult {
                    terminal: "shutdown".to_owned(),
                })
                .map_err(Into::into)
            })(),
            _ => {
                write_adapter_error(&mut writer, id, "unknown_method")?;
                continue;
            }
        };
        match response {
            Ok(result) => write_adapter_success(&mut writer, id, result)?,
            Err(_) => write_adapter_error(&mut writer, id, "invalid_params")?,
        }
        if should_shutdown {
            break;
        }
    }
    Ok(())
}

fn write_adapter_success(
    writer: &mut impl Write,
    id: api::JsonRpcId,
    result: Value,
) -> anyhow::Result<()> {
    let value = serde_json::to_value(api::JsonRpcSuccessResponse {
        jsonrpc: "2.0".to_owned(),
        id,
        result,
    })?;
    let frame = api::canonical_frame(&value, api::MAX_FRAME_BYTES)
        .map_err(|error| anyhow::anyhow!("cannot write adapter response: {error}"))?;
    writer.write_all(frame.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn write_adapter_error(
    writer: &mut impl Write,
    id: api::JsonRpcId,
    name: &str,
) -> anyhow::Result<()> {
    let error = api::error_object(name, None)
        .map_err(|error| anyhow::anyhow!("cannot encode adapter error: {error}"))?;
    let value = serde_json::to_value(api::JsonRpcErrorResponse {
        jsonrpc: "2.0".to_owned(),
        id,
        error,
    })?;
    let frame = api::canonical_frame(&value, api::MAX_FRAME_BYTES)
        .map_err(|error| anyhow::anyhow!("cannot write adapter error: {error}"))?;
    writer.write_all(frame.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn adapter_source_root(source: &Path) -> anyhow::Result<PathBuf> {
    if !source.is_absolute() {
        anyhow::bail!("migration source root must be absolute")
    }
    let metadata = fs::symlink_metadata(source)
        .map_err(|_| anyhow::anyhow!("migration source root does not exist"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        anyhow::bail!("migration source root must be a regular directory")
    }
    let canonical = source
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("migration source root does not exist"))?;
    let metadata = fs::symlink_metadata(&canonical)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        anyhow::bail!("migration source root must be a regular directory")
    }
    Ok(canonical)
}

fn pi_detect(source: &Path) -> anyhow::Result<api::MigrationDetectResult> {
    let source = adapter_source_root(source)?;
    let mut config_paths = Vec::new();
    let mut diagnostics = Vec::new();
    for candidate in CONFIG_CANDIDATES {
        let path = source.join(candidate);
        match read_optional_regular(&path, MAX_CONFIG_BYTES) {
            Ok(Some(_)) => config_paths.push((*candidate).to_owned()),
            Ok(None) => {}
            Err(_) => diagnostics.push(adapter_diagnostic(
                candidate,
                "warning",
                "The Pi configuration file could not be read and will be skipped.",
            )),
        }
    }
    let has_skills = discover_skill_files(&source, &mut diagnostics)?
        .into_iter()
        .next()
        .is_some();
    Ok(api::MigrationDetectResult {
        detected: !config_paths.is_empty() || has_skills,
        config_paths,
        diagnostics,
    })
}

fn pi_import(source: &Path, config_paths: &[String]) -> anyhow::Result<api::MigrationImportResult> {
    let source = adapter_source_root(source)?;
    if config_paths.len() > MAX_SOURCE_ITEMS {
        anyhow::bail!("too many detected Pi configuration paths")
    }
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    let mut mcp_servers = Vec::new();
    let mut diagnostics = Vec::new();

    for relative in config_paths {
        if !CONFIG_CANDIDATES.contains(&relative.as_str()) || !seen.insert(relative.clone()) {
            anyhow::bail!("adapter import received an unauthorized configuration path")
        }
        let path = source.join(relative);
        let Some(bytes) = read_optional_regular(&path, MAX_CONFIG_BYTES)? else {
            diagnostics.push(adapter_diagnostic(
                relative,
                "warning",
                "A configuration file reported during detection is no longer present.",
            ));
            continue;
        };
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => {
                diagnostics.push(adapter_diagnostic(
                    relative,
                    "error",
                    "The Pi configuration is not valid JSON and was not imported.",
                ));
                continue;
            }
        };
        let Some(object) = value.as_object() else {
            diagnostics.push(adapter_diagnostic(
                relative,
                "error",
                "The Pi configuration root must be an object and was not imported.",
            ));
            continue;
        };
        extract_pi_models(object, relative, &mut models, &mut diagnostics);
        extract_pi_mcp_servers(object, relative, &mut mcp_servers, &mut diagnostics);
        if object.contains_key("permissions") || object.contains_key("permission") {
            diagnostics.push(adapter_diagnostic(
                relative,
                "warning",
                "Pi permission decisions were not imported; review Ygg policy settings separately.",
            ));
        }
    }

    let skill_files = discover_skill_files(&source, &mut diagnostics)?;
    let mut skills = Vec::new();
    for (name, path) in skill_files {
        let relative = source_relative(&source, &path)?;
        match ygg_agent::secure_fs::read_regular_file_bounded(&path, MAX_SKILL_BYTES) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(content) => skills.push(api::MigrationSkill {
                    path: relative,
                    name,
                    content,
                }),
                Err(_) => diagnostics.push(adapter_diagnostic(
                    &source_relative(&source, &path)?,
                    "warning",
                    "A Pi skill is not UTF-8 and was not imported.",
                )),
            },
            Err(_) => diagnostics.push(adapter_diagnostic(
                &source_relative(&source, &path)?,
                "warning",
                "A Pi skill could not be read and was not imported.",
            )),
        }
    }

    if models.len() > MAX_SOURCE_ITEMS
        || skills.len() > MAX_SOURCE_ITEMS
        || mcp_servers.len() > MAX_SOURCE_ITEMS
        || diagnostics.len() > MAX_SOURCE_ITEMS
    {
        anyhow::bail!("Pi setup exceeds migration adapter item limits")
    }
    Ok(api::MigrationImportResult {
        models,
        skills,
        mcp_servers,
        diagnostics,
    })
}

fn extract_pi_models(
    object: &Map<String, Value>,
    path: &str,
    models: &mut Vec<api::MigrationModel>,
    diagnostics: &mut Vec<api::MigrationDiagnostic>,
) {
    let configured_provider = object.get("provider").and_then(Value::as_str);
    for key in ["model", "defaultModel", "selectedModel"] {
        let Some(value) = object.get(key) else {
            continue;
        };
        match pi_model_parts(value, configured_provider) {
            Some((provider, model)) => models.push(api::MigrationModel {
                path: path.to_owned(),
                provider,
                model,
            }),
            None => diagnostics.push(adapter_diagnostic(
                path,
                "warning",
                "A Pi model selection could not be mapped without guessing a provider.",
            )),
        }
    }
    if let Some(values) = object.get("models").and_then(Value::as_array) {
        for value in values {
            match pi_model_parts(value, configured_provider) {
                Some((provider, model)) => models.push(api::MigrationModel {
                    path: path.to_owned(),
                    provider,
                    model,
                }),
                None => diagnostics.push(adapter_diagnostic(
                    path,
                    "warning",
                    "A Pi model selection could not be mapped without guessing a provider.",
                )),
            }
        }
    }
}

fn pi_model_parts(value: &Value, configured_provider: Option<&str>) -> Option<(String, String)> {
    match value {
        Value::String(value) => {
            let (provider, model) = value.split_once('/')?;
            valid_adapter_text(provider, api::MAX_MIGRATION_NAME_BYTES)
                .then(|| (provider.to_owned(), model.to_owned()))
        }
        Value::Object(object) => {
            let provider = object
                .get("provider")
                .and_then(Value::as_str)
                .or(configured_provider)?;
            let model = object
                .get("model")
                .or_else(|| object.get("id"))
                .and_then(Value::as_str)?;
            (valid_adapter_text(provider, api::MAX_MIGRATION_NAME_BYTES)
                && valid_adapter_text(model, api::MAX_MIGRATION_NAME_BYTES))
            .then(|| (provider.to_owned(), model.to_owned()))
        }
        _ => None,
    }
}

fn extract_pi_mcp_servers(
    object: &Map<String, Value>,
    path: &str,
    servers: &mut Vec<api::MigrationMcpServer>,
    diagnostics: &mut Vec<api::MigrationDiagnostic>,
) {
    let values = object
        .get("mcpServers")
        .or_else(|| object.get("mcp_servers"));
    let Some(Value::Object(values)) = values else {
        return;
    };
    for (name, value) in values {
        let Some(server) = value.as_object() else {
            diagnostics.push(adapter_diagnostic(
                path,
                "warning",
                "A Pi MCP entry is not an object and was not imported.",
            ));
            continue;
        };
        if server.contains_key("env") || server.contains_key("headers") {
            diagnostics.push(adapter_diagnostic(
                path,
                "warning",
                "MCP environment variables and headers were not imported.",
            ));
        }
        if server.contains_key("cwd") {
            diagnostics.push(adapter_diagnostic(
                path,
                "warning",
                "MCP working directories were not imported; configure them after review.",
            ));
        }
        if server
            .get("transport")
            .or_else(|| server.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|transport| transport != "stdio")
            || server.contains_key("url")
        {
            diagnostics.push(adapter_diagnostic(
                path,
                "warning",
                "Only local stdio MCP servers can be imported.",
            ));
            continue;
        }
        let Some(command) = server.get("command").and_then(Value::as_str) else {
            diagnostics.push(adapter_diagnostic(
                path,
                "warning",
                "An MCP server without a direct command was not imported.",
            ));
            continue;
        };
        let args = match server.get("args") {
            None => Vec::new(),
            Some(Value::Array(args)) => {
                let Some(args) = args.iter().map(Value::as_str).collect::<Option<Vec<_>>>() else {
                    diagnostics.push(adapter_diagnostic(
                        path,
                        "warning",
                        "An MCP server with non-string arguments was not imported.",
                    ));
                    continue;
                };
                args.into_iter().map(str::to_owned).collect()
            }
            Some(_) => {
                diagnostics.push(adapter_diagnostic(
                    path,
                    "warning",
                    "An MCP server with invalid arguments was not imported.",
                ));
                continue;
            }
        };
        if !valid_adapter_text(name, api::MAX_MIGRATION_NAME_BYTES)
            || !valid_adapter_text(command, api::MAX_MIGRATION_COMMAND_BYTES)
            || args.len() > 64
            || args
                .iter()
                .any(|arg| !valid_adapter_text(arg, api::MAX_MIGRATION_ARGUMENT_BYTES))
        {
            diagnostics.push(adapter_diagnostic(
                path,
                "warning",
                "An MCP server exceeded migration safety bounds and was not imported.",
            ));
            continue;
        }
        servers.push(api::MigrationMcpServer {
            path: path.to_owned(),
            name: name.to_owned(),
            command: command.to_owned(),
            args,
        });
    }
}

fn discover_skill_files(
    source: &Path,
    diagnostics: &mut Vec<api::MigrationDiagnostic>,
) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut found = Vec::new();
    for root in SKILL_ROOT_CANDIDATES {
        let root = source.join(root);
        let metadata = match fs::symlink_metadata(&root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                diagnostics.push(adapter_diagnostic(
                    "$",
                    "warning",
                    "A Pi skill directory could not be inspected and was skipped.",
                ));
                continue;
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            diagnostics.push(adapter_diagnostic(
                "$",
                "warning",
                "A Pi skill directory is not a regular directory and was skipped.",
            ));
            continue;
        }
        let mut entries = fs::read_dir(&root)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries.into_iter().take(MAX_SOURCE_ITEMS) {
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                diagnostics.push(adapter_diagnostic(
                    "$",
                    "warning",
                    "A Pi skill with a non-UTF-8 name was skipped.",
                ));
                continue;
            };
            let skill = path.join("SKILL.md");
            if ygg_agent::secure_fs::read_regular_file_bounded(&skill, MAX_SKILL_BYTES).is_ok() {
                found.push((name, skill));
            }
        }
    }
    found.sort_by(|left, right| left.1.cmp(&right.1));
    found.dedup_by(|left, right| left.1 == right.1);
    Ok(found)
}

fn adapter_diagnostic(path: &str, severity: &str, reason: &str) -> api::MigrationDiagnostic {
    api::MigrationDiagnostic {
        path: path.to_owned(),
        severity: severity.to_owned(),
        reason: reason.to_owned(),
    }
}

fn valid_adapter_text(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && !value.chars().any(|character| character.is_control())
}

fn source_relative(source: &Path, path: &Path) -> anyhow::Result<String> {
    let relative = path
        .strip_prefix(source)
        .map_err(|_| anyhow::anyhow!("source path escaped the authorized source root"))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            anyhow::bail!("source path is not normalized")
        };
        parts.push(
            part.to_str()
                .ok_or_else(|| anyhow::anyhow!("source path is not valid UTF-8"))?,
        );
    }
    if parts.is_empty() {
        anyhow::bail!("source path must not be the source root")
    }
    Ok(parts.join("/"))
}

fn normalize_adapter_result(result: api::MigrationImportResult) -> anyhow::Result<MigratedSetup> {
    let mut models = Vec::new();
    let mut skills = Vec::new();
    let mut servers = Vec::new();
    for model in result.models {
        models.push(MigrationOutcome::mapped(
            model.path,
            Model::new(model.provider, model.model)
                .map_err(|error| anyhow::anyhow!("adapter model is not migration-safe: {error}"))?,
        )?);
    }
    for skill in result.skills {
        skills.push(MigrationOutcome::mapped(
            skill.path,
            Skill::new(skill.name, skill.content)
                .map_err(|error| anyhow::anyhow!("adapter skill is not migration-safe: {error}"))?,
        )?);
    }
    for server in result.mcp_servers {
        let transport = McpTransport::stdio(server.command, server.args).map_err(|error| {
            anyhow::anyhow!("adapter MCP server is not migration-safe: {error}")
        })?;
        servers.push(MigrationOutcome::mapped(
            server.path,
            McpServer::new(server.name, transport).map_err(|error| {
                anyhow::anyhow!("adapter MCP server is not migration-safe: {error}")
            })?,
        )?);
    }
    let diagnostics = result
        .diagnostics
        .into_iter()
        .map(|diagnostic| {
            let severity = match diagnostic.severity.as_str() {
                "warning" => DiagnosticSeverity::Warning,
                "error" => DiagnosticSeverity::Error,
                _ => anyhow::bail!("adapter supplied an unknown diagnostic severity"),
            };
            SetupDiagnostic::new(diagnostic.path, severity, diagnostic.reason).map_err(|error| {
                anyhow::anyhow!("adapter diagnostic is not migration-safe: {error}")
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    MigratedSetup::with_parts("pi", models, skills, servers, Vec::new(), diagnostics)
        .map_err(|error| anyhow::anyhow!("adapter output exceeds migration schema bounds: {error}"))
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PiMigrationState {
    version: u32,
    #[serde(default)]
    skills: BTreeMap<String, StateEntry>,
    #[serde(default)]
    mcp_servers: BTreeMap<String, StateEntry>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StateEntry {
    hash: String,
}

impl PiMigrationState {
    fn empty() -> Self {
        Self {
            version: MIGRATION_STATE_VERSION,
            ..Self::default()
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.version != MIGRATION_STATE_VERSION {
            anyhow::bail!("unsupported Pi migration state version")
        }
        for hash in self
            .skills
            .values()
            .chain(self.mcp_servers.values())
            .map(|entry| entry.hash.as_str())
        {
            if !is_sha256(hash) {
                anyhow::bail!("Pi migration state has an invalid content hash")
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum ChangePrivacy {
    Regular,
    Private,
}

#[derive(Clone, Debug)]
struct PlannedChange {
    target: PathBuf,
    relative_target: String,
    original: Option<Vec<u8>>,
    desired: Vec<u8>,
    limit: usize,
    privacy: ChangePrivacy,
}

#[derive(Clone, Debug)]
struct RestoreChange {
    target: PathBuf,
    relative_target: String,
    expected: Option<Vec<u8>>,
    desired: Option<Vec<u8>>,
    privacy: ChangePrivacy,
    limit: usize,
}

#[derive(Clone, Debug, Default)]
struct PlanCounts {
    models: usize,
    skills: usize,
    mcp_servers: usize,
    unchanged: usize,
    skipped: usize,
}

#[derive(Clone, Debug)]
struct Conflict {
    target: String,
    key: String,
}

#[derive(Clone, Debug)]
struct IngestionPlan {
    changes: Vec<PlannedChange>,
    conflicts: Vec<Conflict>,
    counts: PlanCounts,
    diagnostic_count: usize,
}

#[derive(Clone, Debug)]
struct DesiredSkill {
    name: String,
    content: Vec<u8>,
    hash: String,
}

#[derive(Clone, Debug)]
struct DesiredMcpServer {
    name: String,
    value: Value,
    hash: String,
}

fn build_ingestion_plan(
    paths: &MigrationPaths,
    setup: &MigratedSetup,
    accept_conflicts: bool,
) -> anyhow::Result<IngestionPlan> {
    let mut counts = PlanCounts {
        skipped: setup.diagnostics().len(),
        ..PlanCounts::default()
    };
    let desired_skills = desired_skills(setup, &mut counts)?;
    let desired_mcp = desired_mcp_servers(setup, &mut counts)?;
    let selected_model = selected_model(setup, &mut counts)?;
    let has_desired_items =
        selected_model.is_some() || !desired_skills.is_empty() || !desired_mcp.is_empty();
    if !has_desired_items {
        return Ok(IngestionPlan {
            changes: Vec::new(),
            conflicts: Vec::new(),
            counts,
            diagnostic_count: setup.diagnostics().len(),
        });
    }

    let (state_original, mut state) = load_state(paths)?;
    let mut conflicts = Vec::new();
    let mut changes = Vec::new();

    if let Some(model) = selected_model {
        let original = read_optional_regular(&paths.config, MAX_CONFIG_BYTES)?;
        let current = current_model(original.as_deref(), &paths.config)?;
        let prior = state.model.as_deref();
        let conflict = current.as_deref() != Some(model.as_str())
            && !(prior.is_none() && current.is_none())
            && prior != current.as_deref();
        if conflict {
            conflicts.push(Conflict {
                target: relative_home_path(&paths.home, &paths.config)?,
                key: "model".to_owned(),
            });
        }
        if !conflict || accept_conflicts {
            if current.as_deref() != Some(model.as_str()) {
                let original_text = original
                    .as_deref()
                    .map(|bytes| {
                        std::str::from_utf8(bytes)
                            .map_err(|_| anyhow::anyhow!("Ygg config is not valid UTF-8"))
                    })
                    .transpose()?;
                let desired = crate::cli::render_model_persistence_update(
                    original_text,
                    &paths.config,
                    &model,
                )?
                .into_bytes();
                changes.push(PlannedChange {
                    target: paths.config.clone(),
                    relative_target: relative_home_path(&paths.home, &paths.config)?,
                    original,
                    desired,
                    limit: MAX_CONFIG_BYTES,
                    privacy: ChangePrivacy::Regular,
                });
                counts.models += 1;
            } else {
                counts.unchanged += 1;
            }
            state.model = Some(model);
        }
    }

    for desired in desired_skills.values() {
        let target = paths.skills.join(&desired.name).join("SKILL.md");
        let original = read_optional_private(&target, MAX_SKILL_BYTES)?;
        let current_hash = original.as_deref().map(sha256_hex);
        let prior = state
            .skills
            .get(&desired.name)
            .map(|entry| entry.hash.as_str());
        let conflict = match current_hash.as_deref() {
            Some(hash) if hash == desired.hash => false,
            Some(hash) => prior != Some(hash),
            None => prior.is_some(),
        };
        if conflict {
            conflicts.push(Conflict {
                target: relative_home_path(&paths.home, &target)?,
                key: format!("skill {}", desired.name),
            });
        }
        if !conflict || accept_conflicts {
            if current_hash.as_deref() != Some(desired.hash.as_str()) {
                changes.push(PlannedChange {
                    target: target.clone(),
                    relative_target: relative_home_path(&paths.home, &target)?,
                    original,
                    desired: desired.content.clone(),
                    limit: MAX_SKILL_BYTES,
                    privacy: ChangePrivacy::Private,
                });
                counts.skills += 1;
            } else {
                counts.unchanged += 1;
            }
            state.skills.insert(
                desired.name.clone(),
                StateEntry {
                    hash: desired.hash.clone(),
                },
            );
        }
    }

    if !desired_mcp.is_empty() {
        let (mcp_original, mut mcp) = load_mcp_config(paths)?;
        let servers = mcp
            .get_mut("servers")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow::anyhow!("Ygg MCP config has no servers object"))?;
        let mut mcp_changed = false;
        for desired in desired_mcp.values() {
            let current = servers.get(&desired.name);
            let current_hash = current.map(canonical_hash).transpose()?;
            let prior = state
                .mcp_servers
                .get(&desired.name)
                .map(|entry| entry.hash.as_str());
            let conflict = match current_hash.as_deref() {
                Some(hash) if hash == desired.hash => false,
                Some(hash) => prior != Some(hash),
                None => prior.is_some(),
            };
            if conflict {
                conflicts.push(Conflict {
                    target: relative_home_path(&paths.home, &paths.mcp)?,
                    key: format!("MCP server {}", desired.name),
                });
            }
            if !conflict || accept_conflicts {
                if current_hash.as_deref() != Some(desired.hash.as_str()) {
                    servers.insert(desired.name.clone(), desired.value.clone());
                    mcp_changed = true;
                    counts.mcp_servers += 1;
                } else {
                    counts.unchanged += 1;
                }
                state.mcp_servers.insert(
                    desired.name.clone(),
                    StateEntry {
                        hash: desired.hash.clone(),
                    },
                );
            }
        }
        if mcp_changed {
            let desired = pretty_json_bytes(&mcp)?;
            if desired.len() > MAX_MCP_CONFIG_BYTES {
                anyhow::bail!("updated Ygg MCP config exceeds its size limit")
            }
            changes.push(PlannedChange {
                target: paths.mcp.clone(),
                relative_target: relative_home_path(&paths.home, &paths.mcp)?,
                original: mcp_original,
                desired,
                limit: MAX_MCP_CONFIG_BYTES,
                privacy: ChangePrivacy::Private,
            });
        }
    }

    state.validate()?;
    let state_desired = pretty_json_bytes(&state)?;
    if state_desired.len() > MAX_STATE_BYTES {
        anyhow::bail!("Pi migration state exceeds its size limit")
    }
    if has_desired_items && state_original.as_deref() != Some(state_desired.as_slice()) {
        changes.push(PlannedChange {
            target: paths.state.clone(),
            relative_target: relative_home_path(&paths.home, &paths.state)?,
            original: state_original,
            desired: state_desired,
            limit: MAX_STATE_BYTES,
            privacy: ChangePrivacy::Private,
        });
    }

    // Publish state last. A successful state record is therefore never left
    // behind when a prior destination write failed and was rolled back.
    changes.sort_by(|left, right| {
        let left_state = left.target == paths.state;
        let right_state = right.target == paths.state;
        left_state
            .cmp(&right_state)
            .then_with(|| left.target.cmp(&right.target))
    });
    Ok(IngestionPlan {
        changes,
        conflicts,
        counts,
        diagnostic_count: setup.diagnostics().len(),
    })
}

fn desired_skills(
    setup: &MigratedSetup,
    counts: &mut PlanCounts,
) -> anyhow::Result<BTreeMap<String, DesiredSkill>> {
    let mut desired = BTreeMap::new();
    for outcome in setup.skills() {
        let Some((_path, skill)) = outcome.as_mapped() else {
            counts.skipped += 1;
            continue;
        };
        if !valid_skill_name(skill.name()) {
            counts.skipped += 1;
            continue;
        }
        let content = disabled_skill_content(skill.name(), skill.content()).into_bytes();
        if content.len() > MAX_SKILL_BYTES {
            counts.skipped += 1;
            continue;
        }
        desired.insert(
            skill.name().to_owned(),
            DesiredSkill {
                name: skill.name().to_owned(),
                hash: sha256_hex(&content),
                content,
            },
        );
    }
    Ok(desired)
}

fn disabled_skill_content(name: &str, source: &str) -> String {
    // Do not trust or execute source frontmatter. The original text remains
    // intact below a host-authored, disabled review envelope.
    format!(
        "---\nname: {name}\ndescription: Imported Pi skill; review before enabling.\ndisable-model-invocation: true\nmetadata:\n  migration:\n    source: pi\n    review_required: true\n---\n\n{source}"
    )
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn desired_mcp_servers(
    setup: &MigratedSetup,
    counts: &mut PlanCounts,
) -> anyhow::Result<BTreeMap<String, DesiredMcpServer>> {
    let mut desired = BTreeMap::new();
    for outcome in setup.mcp_servers() {
        let Some((_path, server)) = outcome.as_mapped() else {
            counts.skipped += 1;
            continue;
        };
        if !valid_mcp_server_name(server.name()) {
            counts.skipped += 1;
            continue;
        }
        let Some(command) = server.transport().command() else {
            counts.skipped += 1;
            continue;
        };
        let args = server.transport().args().unwrap_or_default();
        if command.is_empty()
            || command.chars().any(char::is_control)
            || args.len() > 64
            || args.iter().any(|arg| arg.chars().any(char::is_control))
        {
            counts.skipped += 1;
            continue;
        }
        let value = json!({
            "transport":"stdio",
            "label":format!("Imported Pi: {}", server.name()),
            "command":command,
            "args":args,
            "enabled":false,
            "required":false,
        });
        let hash = canonical_hash(&value)?;
        desired.insert(
            server.name().to_owned(),
            DesiredMcpServer {
                name: server.name().to_owned(),
                value,
                hash,
            },
        );
    }
    Ok(desired)
}

fn valid_mcp_server_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn selected_model(
    setup: &MigratedSetup,
    counts: &mut PlanCounts,
) -> anyhow::Result<Option<String>> {
    let catalog = ModelCatalog::builtin()
        .map_err(|error| anyhow::anyhow!("cannot load Ygg's static model catalog: {error}"))?;
    let mut selected = None;
    for outcome in setup.models() {
        let Some((_path, model)) = outcome.as_mapped() else {
            counts.skipped += 1;
            continue;
        };
        let provider = match model.provider() {
            "google-ai" | "google-generative-ai" => "google",
            "openai-codex" => "codex",
            provider => provider,
        };
        let route = if model.model().contains('/') {
            model.model().to_owned()
        } else {
            format!("{provider}/{}", model.model())
        };
        if catalog.resolve(&ModelId(route.clone())).is_ok() {
            selected = Some(route);
        } else {
            counts.skipped += 1;
        }
    }
    Ok(selected)
}

fn load_state(paths: &MigrationPaths) -> anyhow::Result<(Option<Vec<u8>>, PiMigrationState)> {
    let original = read_optional_private(&paths.state, MAX_STATE_BYTES)?;
    let state = match original.as_deref() {
        None => PiMigrationState::empty(),
        Some(bytes) => serde_json::from_slice(bytes).map_err(|_| {
            anyhow::anyhow!("Pi migration state is invalid; refuse to overwrite it")
        })?,
    };
    state.validate()?;
    Ok((original, state))
}

fn load_mcp_config(paths: &MigrationPaths) -> anyhow::Result<(Option<Vec<u8>>, Value)> {
    let original = read_optional_private(&paths.mcp, MAX_MCP_CONFIG_BYTES)?;
    let mut value = match original.as_deref() {
        None => json!({"version":1,"servers":{}}),
        Some(bytes) => serde_json::from_slice(bytes).map_err(|_| {
            anyhow::anyhow!("Ygg MCP config is invalid JSON; refuse to overwrite it")
        })?,
    };
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Ygg MCP config root must be an object"))?;
    if object.get("version").and_then(Value::as_u64) != Some(1) {
        anyhow::bail!("Ygg MCP config must have version 1")
    }
    if !object.get("servers").is_some_and(Value::is_object) {
        anyhow::bail!("Ygg MCP config must have a servers object")
    }
    Ok((original, value))
}

fn current_model(original: Option<&[u8]>, path: &Path) -> anyhow::Result<Option<String>> {
    let Some(bytes) = original else {
        return Ok(None);
    };
    let source = std::str::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("Ygg config {} is not valid UTF-8", path.display()))?;
    if source.trim().is_empty() {
        return Ok(None);
    }
    let document = source.parse::<toml_edit::DocumentMut>().map_err(|error| {
        anyhow::anyhow!("cannot update invalid config {}: {error}", path.display())
    })?;
    let Some(item) = document.get("model") else {
        return Ok(None);
    };
    item.as_str()
        .map(str::to_owned)
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("Ygg config model must be a string"))
}

fn read_optional_regular(path: &Path, limit: usize) -> anyhow::Result<Option<Vec<u8>>> {
    match ygg_agent::secure_fs::read_regular_file_bounded(path, limit) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(ygg_agent::secure_fs::SecureFileError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(error) => Err(anyhow::anyhow!(
            "cannot safely read {}: {error}",
            path.display()
        )),
    }
}

fn read_optional_private(path: &Path, limit: usize) -> anyhow::Result<Option<Vec<u8>>> {
    match ygg_agent::secure_fs::read_private_file_bounded(path, limit) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(ygg_agent::secure_fs::SecureFileError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(error) => Err(anyhow::anyhow!(
            "cannot safely read private migration target {}: {error}",
            path.display()
        )),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BackupManifest {
    version: u32,
    created_at_unix_ms: u128,
    source: String,
    entries: Vec<BackupEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BackupEntry {
    target: String,
    backup: Option<String>,
    before_sha256: Option<String>,
    after_sha256: String,
    private: bool,
}

fn apply_ingestion_plan(
    paths: &MigrationPaths,
    plan: &IngestionPlan,
) -> anyhow::Result<Option<PathBuf>> {
    if plan.changes.is_empty() {
        return Ok(None);
    }
    let backup = create_backup(paths, &plan.changes)?;
    let mut committed = Vec::new();
    for change in &plan.changes {
        if let Err(error) = write_planned_change(change) {
            let rollback_errors = rollback_changes(&committed);
            let recovery = format!(
                " Backup retained at {}. Restore with `ygg migrate restore {}`.",
                backup.display(),
                backup.display()
            );
            if rollback_errors.is_empty() {
                anyhow::bail!(
                    "migration failed while updating {} and was rolled back: {error}.{recovery}",
                    change.relative_target
                )
            }
            anyhow::bail!(
                "migration failed while updating {}; automatic rollback was incomplete ({} target(s)).{recovery}",
                change.relative_target,
                rollback_errors.len()
            )
        }
        committed.push(change.clone());
    }
    Ok(Some(backup))
}

fn write_planned_change(change: &PlannedChange) -> anyhow::Result<()> {
    let result = match change.privacy {
        ChangePrivacy::Regular => ygg_agent::secure_fs::write_atomic_if_unchanged(
            &change.target,
            change.original.as_deref(),
            &change.desired,
            change.limit,
        ),
        ChangePrivacy::Private => ygg_agent::secure_fs::write_private_atomic_if_unchanged(
            &change.target,
            change.original.as_deref(),
            &change.desired,
            change.limit,
        ),
    };
    result.map_err(|error| {
        anyhow::anyhow!(
            "atomic compare-and-swap refused {}: {error}",
            change.relative_target
        )
    })
}

fn rollback_changes(changes: &[PlannedChange]) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    for change in changes.iter().rev() {
        let result = match &change.original {
            Some(original) => match change.privacy {
                ChangePrivacy::Regular => ygg_agent::secure_fs::write_atomic_if_unchanged(
                    &change.target,
                    Some(&change.desired),
                    original,
                    change.limit,
                ),
                ChangePrivacy::Private => ygg_agent::secure_fs::write_private_atomic_if_unchanged(
                    &change.target,
                    Some(&change.desired),
                    original,
                    change.limit,
                ),
            },
            None => remove_created_change(change),
        };
        if let Err(error) = result {
            errors.push(anyhow::anyhow!(
                "could not roll back {}: {error}",
                change.relative_target
            ));
        }
    }
    errors
}

fn remove_created_change(
    change: &PlannedChange,
) -> Result<(), ygg_agent::secure_fs::SecureFileError> {
    match change.privacy {
        ChangePrivacy::Regular => ygg_agent::secure_fs::remove_regular_file_if_unchanged(
            &change.target,
            &change.desired,
            change.limit,
        ),
        ChangePrivacy::Private => ygg_agent::secure_fs::remove_private_file_if_unchanged(
            &change.target,
            &change.desired,
            change.limit,
        ),
    }
}

fn create_backup(paths: &MigrationPaths, changes: &[PlannedChange]) -> anyhow::Result<PathBuf> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock is before the Unix epoch"))?
        .as_millis();
    let backup = ygg_agent::secure_fs::create_unique_private_directory(
        &paths.backups,
        &format!("{millis}-"),
    )
    .map_err(|error| anyhow::anyhow!("cannot create private migration backup: {error}"))?;
    let mut entries = Vec::with_capacity(changes.len());
    for (index, change) in changes.iter().enumerate() {
        let backup_name = change.original.as_ref().map(|_| format!("{index:03}.bin"));
        if let (Some(name), Some(original)) = (&backup_name, &change.original) {
            ygg_agent::secure_fs::write_private_atomic(&backup.join(name), original, change.limit)
                .map_err(|error| {
                    anyhow::anyhow!("cannot write private migration backup: {error}")
                })?;
        }
        entries.push(BackupEntry {
            target: change.relative_target.clone(),
            backup: backup_name,
            before_sha256: change.original.as_deref().map(sha256_hex),
            after_sha256: sha256_hex(&change.desired),
            private: matches!(change.privacy, ChangePrivacy::Private),
        });
    }
    let manifest = BackupManifest {
        version: BACKUP_VERSION,
        created_at_unix_ms: millis,
        source: "pi".to_owned(),
        entries,
    };
    let bytes = pretty_json_bytes(&manifest)?;
    ygg_agent::secure_fs::write_private_atomic(
        &backup.join("manifest.json"),
        &bytes,
        MAX_BACKUP_MANIFEST_BYTES,
    )
    .map_err(|error| anyhow::anyhow!("cannot write migration backup manifest: {error}"))?;
    Ok(backup)
}

fn restore_backup(paths: &MigrationPaths, backup: &Path, force: bool) -> anyhow::Result<usize> {
    let backup = authorized_backup_path(paths, backup)?;
    let manifest_path = backup.join("manifest.json");
    let bytes = read_optional_private(&manifest_path, MAX_BACKUP_MANIFEST_BYTES)?
        .ok_or_else(|| anyhow::anyhow!("migration backup manifest is missing"))?;
    let manifest: BackupManifest = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("migration backup manifest is invalid"))?;
    validate_backup_manifest(&manifest)?;

    let restored = manifest.entries.len();
    let mut changes = Vec::with_capacity(restored);
    for entry in manifest.entries {
        let (target, privacy) = target_from_backup_entry(paths, &entry.target)?;
        if entry.private != matches!(privacy, ChangePrivacy::Private) {
            anyhow::bail!("migration backup entry has an invalid privacy class")
        }
        let current = match privacy {
            ChangePrivacy::Private => read_optional_private(&target, MAX_CONFIG_BYTES)?,
            ChangePrivacy::Regular => read_optional_regular(&target, MAX_CONFIG_BYTES)?,
        };
        let current_hash = current.as_deref().map(sha256_hex);
        if !force && current_hash.as_deref() != Some(entry.after_sha256.as_str()) {
            anyhow::bail!(
                "{} changed after import; review it and rerun restore with --yes to overwrite it",
                entry.target
            )
        }
        let desired = match entry.backup {
            Some(name) => {
                let original = read_optional_private(&backup.join(name), MAX_CONFIG_BYTES)?
                    .ok_or_else(|| anyhow::anyhow!("migration backup payload is missing"))?;
                let before = entry
                    .before_sha256
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("migration backup payload hash is missing"))?;
                if sha256_hex(&original) != before {
                    anyhow::bail!("migration backup payload does not match its manifest hash")
                }
                Some(original)
            }
            None => None,
        };
        if current.is_some() || desired.is_some() {
            changes.push(RestoreChange {
                target,
                relative_target: entry.target,
                expected: current,
                desired,
                privacy,
                limit: MAX_CONFIG_BYTES,
            });
        }
    }
    apply_restore_changes(&changes)?;
    Ok(restored)
}

fn apply_restore_changes(changes: &[RestoreChange]) -> anyhow::Result<()> {
    let mut committed = Vec::new();
    for change in changes {
        if let Err(error) = write_restore_change(change) {
            let rollback_errors = rollback_restore_changes(&committed);
            if rollback_errors.is_empty() {
                anyhow::bail!(
                    "restore failed while updating {} and was rolled back: {error}",
                    change.relative_target
                )
            }
            anyhow::bail!(
                "restore failed while updating {}; automatic rollback was incomplete ({} target(s))",
                change.relative_target,
                rollback_errors.len()
            )
        }
        committed.push(change.clone());
    }
    Ok(())
}

fn write_restore_change(
    change: &RestoreChange,
) -> Result<(), ygg_agent::secure_fs::SecureFileError> {
    match (&change.expected, &change.desired) {
        (_, Some(desired)) => write_change_bytes(
            change.privacy,
            &change.target,
            change.expected.as_deref(),
            desired,
            change.limit,
        ),
        (Some(expected), None) => {
            remove_change_bytes(change.privacy, &change.target, expected, change.limit)
        }
        (None, None) => Ok(()),
    }
}

fn rollback_restore_changes(changes: &[RestoreChange]) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    for change in changes.iter().rev() {
        let result = match (&change.expected, &change.desired) {
            (Some(original), Some(restored)) => write_change_bytes(
                change.privacy,
                &change.target,
                Some(restored),
                original,
                change.limit,
            ),
            (Some(original), None) => {
                write_change_bytes(change.privacy, &change.target, None, original, change.limit)
            }
            (None, Some(restored)) => {
                remove_change_bytes(change.privacy, &change.target, restored, change.limit)
            }
            (None, None) => Ok(()),
        };
        if let Err(error) = result {
            errors.push(anyhow::anyhow!(
                "could not roll back restored {}: {error}",
                change.relative_target
            ));
        }
    }
    errors
}

fn write_change_bytes(
    privacy: ChangePrivacy,
    target: &Path,
    expected: Option<&[u8]>,
    desired: &[u8],
    limit: usize,
) -> Result<(), ygg_agent::secure_fs::SecureFileError> {
    match privacy {
        ChangePrivacy::Regular => {
            ygg_agent::secure_fs::write_atomic_if_unchanged(target, expected, desired, limit)
        }
        ChangePrivacy::Private => ygg_agent::secure_fs::write_private_atomic_if_unchanged(
            target, expected, desired, limit,
        ),
    }
}

fn remove_change_bytes(
    privacy: ChangePrivacy,
    target: &Path,
    expected: &[u8],
    limit: usize,
) -> Result<(), ygg_agent::secure_fs::SecureFileError> {
    match privacy {
        ChangePrivacy::Regular => {
            ygg_agent::secure_fs::remove_regular_file_if_unchanged(target, expected, limit)
        }
        ChangePrivacy::Private => {
            ygg_agent::secure_fs::remove_private_file_if_unchanged(target, expected, limit)
        }
    }
}

fn authorized_backup_path(paths: &MigrationPaths, backup: &Path) -> anyhow::Result<PathBuf> {
    let root = paths
        .backups
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("migration backup root does not exist"))?;
    let backup = backup
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("migration backup path does not exist"))?;
    if backup.parent() != Some(root.as_path()) {
        anyhow::bail!("backup path is outside this Ygg home's migration backup directory")
    }
    Ok(backup)
}

fn validate_backup_manifest(manifest: &BackupManifest) -> anyhow::Result<()> {
    if manifest.version != BACKUP_VERSION || manifest.source != "pi" || manifest.entries.is_empty()
    {
        anyhow::bail!("unsupported migration backup manifest")
    }
    if manifest.entries.len() > MAX_SOURCE_ITEMS.saturating_add(4) {
        anyhow::bail!("migration backup manifest has too many entries")
    }
    let mut targets = BTreeSet::new();
    let mut payloads = BTreeSet::new();
    for entry in &manifest.entries {
        if !targets.insert(&entry.target) {
            anyhow::bail!("migration backup manifest has duplicate targets")
        }
        validate_relative_target(&entry.target)?;
        if !is_sha256(&entry.after_sha256)
            || entry
                .before_sha256
                .as_deref()
                .is_some_and(|hash| !is_sha256(hash))
        {
            anyhow::bail!("migration backup manifest has an invalid hash")
        }
        if entry.backup.is_some() != entry.before_sha256.is_some() {
            anyhow::bail!("migration backup manifest has inconsistent payload metadata")
        }
        if let Some(name) = &entry.backup {
            if !payloads.insert(name) {
                anyhow::bail!("migration backup manifest has duplicate payload names")
            }
            if name.contains('/')
                || name.contains('\\')
                || !name.ends_with(".bin")
                || name.len() > 32
            {
                anyhow::bail!("migration backup manifest has an invalid payload name")
            }
        }
    }
    Ok(())
}

fn target_from_backup_entry(
    paths: &MigrationPaths,
    relative: &str,
) -> anyhow::Result<(PathBuf, ChangePrivacy)> {
    validate_relative_target(relative)?;
    let target = paths.home.join(relative);
    if !target.starts_with(&paths.home) {
        anyhow::bail!("migration backup target escaped the Ygg home")
    }
    if target == paths.config {
        return Ok((target, ChangePrivacy::Regular));
    }
    if target == paths.mcp || target == paths.state {
        return Ok((target, ChangePrivacy::Private));
    }
    let skill = target
        .strip_prefix(&paths.skills)
        .ok()
        .and_then(|relative| {
            let mut components = relative.components();
            match (components.next(), components.next(), components.next()) {
                (Some(Component::Normal(name)), Some(Component::Normal(file)), None)
                    if file == "SKILL.md" =>
                {
                    name.to_str().filter(|name| valid_skill_name(name))
                }
                _ => None,
            }
        });
    if skill.is_some() {
        return Ok((target, ChangePrivacy::Private));
    }
    anyhow::bail!("migration backup target is not an import-managed destination")
}

fn validate_relative_target(path: &str) -> anyhow::Result<()> {
    if path.is_empty()
        || path.len() > 4096
        || path.contains('\\')
        || path.chars().any(char::is_control)
    {
        anyhow::bail!("migration backup target is invalid")
    }
    let value = Path::new(path);
    if value.is_absolute()
        || value
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("migration backup target is invalid")
    }
    Ok(())
}

fn relative_home_path(home: &Path, target: &Path) -> anyhow::Result<String> {
    let relative = target
        .strip_prefix(home)
        .map_err(|_| anyhow::anyhow!("migration target escaped home"))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            anyhow::bail!("migration target is not normalized")
        };
        parts.push(
            part.to_str()
                .ok_or_else(|| anyhow::anyhow!("migration target is not valid UTF-8"))?,
        );
    }
    if parts.is_empty() {
        anyhow::bail!("migration target must not be the home directory")
    }
    Ok(parts.join("/"))
}

fn pretty_json_bytes(value: &impl Serialize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn canonical_hash(value: &Value) -> anyhow::Result<String> {
    let encoded = api::canonical_json(value)
        .map_err(|error| anyhow::anyhow!("cannot canonicalize MCP server: {error}"))?;
    Ok(sha256_hex(encoded.as_bytes()))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn path_to_utf8(path: &Path, label: &str) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{label} must be valid UTF-8"))
}

fn confirm_conflicts(conflicts: &[Conflict], yes: bool) -> anyhow::Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "migration found {} conflicting current entries; rerun with --yes after review",
            conflicts.len()
        )
    }
    crate::output::stdout_line(format!(
        "Migration found {} conflicting current entry(s):",
        conflicts.len()
    ));
    for conflict in conflicts {
        crate::output::stdout_line(format!("  {} ({})", conflict.target, conflict.key));
    }
    crate::output::stdout_line("Overwrite these entries? [y/N]");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(matches!(input.trim(), "y" | "Y" | "yes" | "YES"))
}

#[derive(Serialize)]
struct PublicImportReport<'a> {
    source: String,
    dry_run: bool,
    models_updated: usize,
    skills_disabled: usize,
    mcp_servers_disabled: usize,
    unchanged: usize,
    skipped: usize,
    diagnostics: usize,
    conflicts: usize,
    backup: Option<&'a Path>,
}

fn emit_no_source_report(source: &Path, json_output: bool) {
    if json_output {
        crate::output::stdout_multiline(
            serde_json::to_string_pretty(&json!({
                "source":source,
                "detected":false,
                "changed":false,
            }))
            .expect("static no-source report serializes"),
        );
    } else {
        crate::output::stdout_line(format!(
            "No Pi setup was detected at {}; no files were changed.",
            source.display()
        ));
    }
}

fn emit_import_report(
    source: &Path,
    plan: &IngestionPlan,
    backup: Option<&Path>,
    dry_run: bool,
    json_output: bool,
) {
    let report = PublicImportReport {
        source: source.display().to_string(),
        dry_run,
        models_updated: plan.counts.models,
        skills_disabled: plan.counts.skills,
        mcp_servers_disabled: plan.counts.mcp_servers,
        unchanged: plan.counts.unchanged,
        skipped: plan.counts.skipped,
        diagnostics: plan.diagnostic_count,
        conflicts: plan.conflicts.len(),
        backup,
    };
    if json_output {
        crate::output::stdout_multiline(
            serde_json::to_string_pretty(&report).expect("public migration report serializes"),
        );
        return;
    }
    let action = if dry_run {
        "Pi migration import preview"
    } else {
        "Pi migration import complete"
    };
    crate::output::stdout_line(action);
    crate::output::stdout_line(format!("  Source: {}", source.display()));
    crate::output::stdout_line(format!("  Model updates: {}", report.models_updated));
    crate::output::stdout_line(format!(
        "  Disabled skills awaiting review: {}",
        report.skills_disabled
    ));
    crate::output::stdout_line(format!(
        "  Disabled MCP servers awaiting review: {}",
        report.mcp_servers_disabled
    ));
    crate::output::stdout_line(format!("  Unchanged: {}", report.unchanged));
    crate::output::stdout_line(format!("  Skipped: {}", report.skipped));
    if report.conflicts > 0 {
        crate::output::stdout_line(format!("  Conflicts: {}", report.conflicts));
    }
    if let Some(backup) = backup {
        crate::output::stdout_line(format!("  Backup: {}", backup.display()));
    }
    crate::output::stdout_line(
        "  Credentials, MCP environment values, headers, and Pi permissions were not copied.",
    );
    if report.skills_disabled > 0 || report.mcp_servers_disabled > 0 {
        crate::output::stdout_line(
            "  Review imported entries before enabling skills or MCP servers; migration never enables extensions.",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(temp: &tempfile::TempDir) -> MigrationPaths {
        let home = temp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        MigrationPaths::new(home).unwrap()
    }

    fn setup() -> MigratedSetup {
        let model =
            MigrationOutcome::mapped("settings.json", Model::new("openai", "gpt-4o").unwrap())
                .unwrap();
        let skill = MigrationOutcome::mapped(
            "skills/review/SKILL.md",
            Skill::new("review", "Review this change.").unwrap(),
        )
        .unwrap();
        let server = MigrationOutcome::mapped(
            "settings.json",
            McpServer::new(
                "docs",
                McpTransport::stdio("docs-mcp", vec!["--stdio".into()]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        MigratedSetup::with_parts("pi", vec![model], vec![skill], vec![server], vec![], vec![])
            .unwrap()
    }

    #[test]
    fn ingestion_is_disabled_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let plan = build_ingestion_plan(&paths, &setup(), false).unwrap();
        assert!(plan.conflicts.is_empty());
        let backup = apply_ingestion_plan(&paths, &plan).unwrap().unwrap();
        assert!(backup.join("manifest.json").exists());
        let skill = fs::read_to_string(paths.skills.join("review/SKILL.md")).unwrap();
        assert!(skill.contains("disable-model-invocation: true"));
        let mcp: Value = serde_json::from_slice(&fs::read(&paths.mcp).unwrap()).unwrap();
        assert_eq!(mcp["servers"]["docs"]["enabled"], false);
        assert_eq!(mcp["servers"]["docs"]["command"], "docs-mcp");
        let second = build_ingestion_plan(&paths, &setup(), false).unwrap();
        assert!(second.conflicts.is_empty());
        assert!(second.changes.is_empty());
    }

    #[test]
    fn unsupported_source_items_do_not_create_migration_state() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let setup = MigratedSetup::with_parts(
            "pi",
            vec![MigrationOutcome::mapped(
                "settings.json",
                Model::new("unsupported", "model").unwrap(),
            )
            .unwrap()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();

        let plan = build_ingestion_plan(&paths, &setup, false).unwrap();
        assert!(plan.changes.is_empty());
        assert!(apply_ingestion_plan(&paths, &plan).unwrap().is_none());
        assert!(!paths.state.exists());
    }

    #[test]
    fn changed_imported_skill_is_a_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let plan = build_ingestion_plan(&paths, &setup(), false).unwrap();
        apply_ingestion_plan(&paths, &plan).unwrap();
        let target = paths.skills.join("review/SKILL.md");
        ygg_agent::secure_fs::write_private_atomic(&target, b"user edit", MAX_SKILL_BYTES).unwrap();
        let conflict = build_ingestion_plan(&paths, &setup(), false).unwrap();
        assert_eq!(conflict.conflicts.len(), 1);
        let accepted = build_ingestion_plan(&paths, &setup(), true).unwrap();
        assert_eq!(accepted.conflicts.len(), 1);
        apply_ingestion_plan(&paths, &accepted).unwrap();
        assert!(fs::read_to_string(target)
            .unwrap()
            .contains("Imported Pi skill"));
    }

    #[test]
    fn backup_restores_original_targets() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        ygg_agent::secure_fs::create_private_directory_all(&paths.home.join(".ygg")).unwrap();
        ygg_agent::secure_fs::write_atomic_if_unchanged(
            &paths.config,
            None,
            b"model = \"openai/gpt-4.1\"\n",
            MAX_CONFIG_BYTES,
        )
        .unwrap();
        let plan = build_ingestion_plan(&paths, &setup(), false).unwrap();
        let backup = apply_ingestion_plan(&paths, &plan).unwrap().unwrap();
        restore_backup(&paths, &backup, false).unwrap();
        assert_eq!(
            fs::read_to_string(&paths.config).unwrap(),
            "model = \"openai/gpt-4.1\"\n"
        );
        assert!(!paths.skills.join("review/SKILL.md").exists());
    }

    #[test]
    fn adapter_reads_fixture_without_writing_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("pi");
        fs::create_dir_all(source.join("skills/review")).unwrap();
        fs::write(
            source.join("settings.json"),
            r#"{"model":"openai/gpt-4o","mcpServers":{"docs":{"command":"docs-mcp","args":["--stdio"],"env":{"TOKEN":"secret"}}}}"#,
        )
        .unwrap();
        fs::write(source.join("skills/review/SKILL.md"), "Review.").unwrap();
        let before = sha256_hex(&fs::read(source.join("settings.json")).unwrap());
        let detected = pi_detect(&source).unwrap();
        assert!(detected.detected);
        let imported = pi_import(&source, &detected.config_paths).unwrap();
        assert_eq!(imported.models.len(), 1);
        assert_eq!(imported.skills.len(), 1);
        assert_eq!(imported.mcp_servers.len(), 1);
        assert!(imported
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.reason.contains("environment")));
        assert_eq!(
            before,
            sha256_hex(&fs::read(source.join("settings.json")).unwrap())
        );
    }

    #[test]
    fn adapter_line_reader_is_bounded() {
        let mut exact = vec![b'x'; api::MAX_FRAME_BYTES];
        exact.push(b'\n');
        let mut reader = BufReader::new(exact.as_slice());
        assert_eq!(
            read_bounded_adapter_line(&mut reader)
                .unwrap()
                .unwrap()
                .len(),
            api::MAX_FRAME_BYTES
        );

        let mut oversized = vec![b'x'; api::MAX_FRAME_BYTES.saturating_add(1)];
        oversized.push(b'\n');
        let mut reader = BufReader::new(oversized.as_slice());
        assert_eq!(
            read_bounded_adapter_line(&mut reader).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn canonical_adapter_frames_reject_whitespace_and_duplicates() {
        assert!(parse_canonical_adapter_frame(r#"{"id":1, "jsonrpc":"2.0","result":{}}"#).is_err());
        assert!(
            parse_canonical_adapter_frame(r#"{"id":1,"id":1,"jsonrpc":"2.0","result":{}}"#)
                .is_err()
        );
    }
}
