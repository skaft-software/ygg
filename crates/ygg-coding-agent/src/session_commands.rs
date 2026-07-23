#![allow(missing_docs)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Subcommand;
use regex::{Captures, Regex};
use serde::Serialize;
use serde_json::Value;
use ygg_agent::Session;

use crate::config::Config;
use crate::session_store::{
    active_branch_title, SessionMeta, SessionStore, SessionUserMetadata, MAX_SESSION_FILE_BYTES,
};
use crate::session_tree::render_session_tree;

#[derive(Clone, Debug, Subcommand)]
pub enum SessionCommand {
    /// List sessions for the selected workspace.
    List {
        /// Search names, tags, ids, paths, and dates encoded in session ids.
        #[arg(short, long)]
        query: Option<String>,
    },
    /// Inspect one session without modifying it.
    Inspect { id: String },
    /// Give a session a readable name (an empty name clears it).
    Rename { id: String, name: String },
    /// Replace a session's searchable tags.
    Tag { id: String, tags: Vec<String> },
    /// Export a validated, redacted portable JSON package.
    Export {
        id: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Include raw values. Use only when the destination is trusted.
        #[arg(long)]
        include_secrets: bool,
        /// Replace an existing export path.
        #[arg(long)]
        force: bool,
    },
    /// Move a session into the store's recoverable trash directory.
    Delete { id: String },
    /// Validate a session and repair only an interrupted final append.
    Repair { id: String },
}

pub fn run(command: SessionCommand, config: &Config) -> anyhow::Result<()> {
    let store = SessionStore::new(&config.session_dir, &config.workspace);
    match command {
        SessionCommand::List { query } => list(&store, query.as_deref()),
        SessionCommand::Inspect { id } => inspect(&store, &id),
        SessionCommand::Rename { id, name } => rename(&store, &id, &name),
        SessionCommand::Tag { id, tags } => tag(&store, &id, tags),
        SessionCommand::Export {
            id,
            output,
            include_secrets,
            force,
        } => export_cli(
            &store,
            &id,
            output,
            &config.invocation_cwd,
            include_secrets,
            force,
        ),
        SessionCommand::Delete { id } => delete(&store, &id),
        SessionCommand::Repair { id } => repair(&store, &id),
    }
}

fn list(store: &SessionStore, query: Option<&str>) -> anyhow::Result<()> {
    let query = query.map(|value| value.trim().to_ascii_lowercase());
    let sessions = store
        .list()
        .into_iter()
        .filter(|session| matches_query(session, query.as_deref()))
        .collect::<Vec<_>>();
    if sessions.is_empty() {
        println!("No matching sessions in {}", store.dir().display());
        return Ok(());
    }
    println!("ID\tNAME\tTAGS\tMODIFIED");
    for session in sessions {
        println!(
            "{}\t{}\t{}\t{}",
            session.id,
            session.name.as_deref().unwrap_or(&session.title),
            session.tags.join(","),
            relative_time(session.modified, SystemTime::now())
        );
    }
    Ok(())
}

fn matches_query(session: &SessionMeta, query: Option<&str>) -> bool {
    let Some(query) = query.filter(|query| !query.is_empty()) else {
        return true;
    };
    let haystack = format!(
        "{} {} {} {} {}",
        session.id,
        session.name.as_deref().unwrap_or_default(),
        session.title,
        session.tags.join(" "),
        session.path.display()
    )
    .to_ascii_lowercase();
    haystack.contains(query)
}

fn inspect(store: &SessionStore, id: &str) -> anyhow::Result<()> {
    let path = store.path_by_id(id)?;
    let session = Session::open_read_only(&path)
        .map_err(|error| anyhow::anyhow!("session {id:?} is not readable: {error}"))?;
    let metadata = store.load_metadata(id)?;
    let file = path.metadata()?;
    let parent_ids = session
        .entries()
        .iter()
        .filter_map(|entry| entry.parent.as_ref().map(|parent| parent.0.as_str()))
        .collect::<HashSet<_>>();
    let leaves = session
        .entries()
        .iter()
        .filter(|entry| !parent_ids.contains(entry.id.0.as_str()))
        .count();
    let roots = session
        .entries()
        .iter()
        .filter(|entry| entry.parent.is_none())
        .count();

    println!("Session: {id}");
    println!("Title: {}", active_branch_title(&session));
    println!(
        "Name: {}",
        metadata
            .name
            .as_deref()
            .unwrap_or("(derived from first prompt)")
    );
    println!(
        "Tags: {}",
        if metadata.tags.is_empty() {
            "(none)".to_owned()
        } else {
            metadata.tags.join(", ")
        }
    );
    println!("Path: {}", path.display());
    let modified = file.modified()?;
    println!(
        "Modified: {} (unix {})",
        relative_time(modified, SystemTime::now()),
        unix_seconds(modified)
    );
    println!("Size: {}", human_bytes(file.len()));
    println!("Entries: {}", session.entries().len());
    println!("Branches: {leaves} leaves from {roots} roots");
    println!(
        "Head: {}",
        session
            .head()
            .map_or_else(|| "(empty)".into(), |head| head.0)
    );
    println!("Checkpoints: {}", session.checkpoints().len());
    println!("Usage records: {}", session.usage_records().len());
    println!("Cost: {} microdollars", session.total_cost_microdollars());
    println!();
    println!("{}", render_session_tree(&session));
    Ok(())
}

fn rename(store: &SessionStore, id: &str, name: &str) -> anyhow::Result<()> {
    let metadata = store.rename(id, name)?;
    match metadata.name {
        Some(name) => println!("Renamed session {id} to {name:?}."),
        None => println!("Cleared the custom name for session {id}."),
    }
    Ok(())
}

fn tag(store: &SessionStore, id: &str, tags: Vec<String>) -> anyhow::Result<()> {
    let metadata = store.set_tags(id, tags)?;
    if metadata.tags.is_empty() {
        println!("Cleared tags for session {id}.");
    } else {
        println!("Session {id} tags: {}", metadata.tags.join(", "));
    }
    Ok(())
}

#[derive(Serialize)]
struct PortableSessionExport {
    format: &'static str,
    version: u32,
    exported_at_unix_seconds: u64,
    source_id: String,
    source_title: String,
    metadata: SessionUserMetadata,
    redacted: bool,
    redaction_count: usize,
    records: Vec<Value>,
}

pub(crate) struct SessionExportReport {
    pub destination: PathBuf,
    pub redaction_count: usize,
    pub included_secrets: bool,
    pub ignored_torn_tail: bool,
}

pub(crate) fn export_portable(
    store: &SessionStore,
    id: &str,
    output: Option<PathBuf>,
    cwd: &Path,
    include_secrets: bool,
    force: bool,
) -> anyhow::Result<SessionExportReport> {
    let path = store.path_by_id(id)?;
    Session::open_read_only(&path)
        .map_err(|error| anyhow::anyhow!("refusing to export corrupt session {id:?}: {error}"))?;
    let opened_path = crate::session_store::absolute_read_path(&path)?;
    let bytes =
        ygg_agent::secure_fs::read_regular_file_bounded(&opened_path, MAX_SESSION_FILE_BYTES)?;
    let (records, ignored_torn_tail) = parse_export_records(&bytes)?;
    let mut redaction_count = 0usize;
    let meta = store
        .list()
        .into_iter()
        .find(|candidate| candidate.id == id)
        .ok_or_else(|| anyhow::anyhow!("session {id:?} has no resumable conversation"))?;
    let package = PortableSessionExport {
        format: "ygg-session-export",
        version: 1,
        exported_at_unix_seconds: unix_seconds(SystemTime::now()),
        source_id: id.to_owned(),
        source_title: meta.title,
        metadata: store.load_metadata(id)?,
        redacted: !include_secrets,
        redaction_count: 0,
        records,
    };
    // Redact the complete serialized package, not only source records. Session
    // names, tags, derived titles, and future metadata fields are all sharing
    // surfaces and must receive the same scrubber.
    let mut package = serde_json::to_value(package)?;
    if !include_secrets {
        redact_value(&mut package, None, &mut redaction_count)?;
    }
    package["redaction_count"] = Value::from(redaction_count);
    let destination = output.unwrap_or_else(|| PathBuf::from(format!("{id}.ygg-session.json")));
    let destination = if destination.is_absolute() {
        destination
    } else {
        cwd.join(destination)
    };
    if destination.exists() && !force {
        anyhow::bail!(
            "export destination {} already exists; pass --force to replace it",
            destination.display()
        );
    }
    let payload = serde_json::to_vec_pretty(&package)?;
    crate::auth::write_private_atomic(&destination, &payload, ".session-export-")?;
    Ok(SessionExportReport {
        destination,
        redaction_count,
        included_secrets: include_secrets,
        ignored_torn_tail,
    })
}

fn export_cli(
    store: &SessionStore,
    id: &str,
    output: Option<PathBuf>,
    cwd: &Path,
    include_secrets: bool,
    force: bool,
) -> anyhow::Result<()> {
    let report = export_portable(store, id, output, cwd, include_secrets, force)?;
    println!("Exported session {id} to {}.", report.destination.display());
    if report.included_secrets {
        println!("Warning: the export contains raw session values.");
    } else {
        println!(
            "Redacted {} potentially sensitive values.",
            report.redaction_count
        );
    }
    if report.ignored_torn_tail {
        println!("Ignored an interrupted final append; run `ygg sessions repair {id}` to normalize the source.");
    }
    Ok(())
}

fn parse_export_records(bytes: &[u8]) -> anyhow::Result<(Vec<Value>, bool)> {
    let completed_end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    let completed = std::str::from_utf8(&bytes[..completed_end]).map_err(|error| {
        anyhow::anyhow!("completed session records are not valid UTF-8: {error}")
    })?;
    let (text, invalid_utf8_tail) = match std::str::from_utf8(bytes) {
        Ok(text) => (text, false),
        Err(error) if error.valid_up_to() >= completed_end => (completed, true),
        Err(error) => return Err(anyhow::anyhow!("session is not valid UTF-8: {error}")),
    };
    let mut records = Vec::new();
    let mut ignored_torn_tail = invalid_utf8_tail;
    let mut segments = text.split_inclusive('\n').peekable();
    while let Some(segment) = segments.next() {
        let is_last = segments.peek().is_none();
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(_) if is_last && !segment.ends_with('\n') => ignored_torn_tail = true,
            Err(error) => return Err(anyhow::anyhow!("invalid JSONL record: {error}")),
        }
    }
    Ok((records, ignored_torn_tail))
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    let separated = [
        "api_key",
        "apikey",
        "authorization",
        "access_token",
        "refresh_token",
        "id_token",
        "auth_token",
        "session_token",
        "api_token",
        "client_secret",
        "password",
        "passwd",
        "secret",
        "credential",
        "cookie",
        "private_key",
    ];
    if separated
        .iter()
        .any(|needle| key == *needle || key.ends_with(&format!("_{needle}")))
    {
        return true;
    }

    let compact = key.replace('_', "");
    [
        "apikey",
        "authorization",
        "accesstoken",
        "refreshtoken",
        "idtoken",
        "authtoken",
        "sessiontoken",
        "apitoken",
        "clientsecret",
        "password",
        "passwd",
        "secret",
        "credential",
        "cookie",
        "privatekey",
    ]
    .iter()
    .any(|needle| compact == *needle || compact.ends_with(needle))
}

fn redact_value(value: &mut Value, key: Option<&str>, count: &mut usize) -> anyhow::Result<()> {
    redact_value_at_depth(value, key, count, 0)
}

const REDACTION: &str = "[REDACTED]";
const MAX_NESTED_JSON_STRING_BYTES: usize = 1024 * 1024;
const MAX_NESTED_JSON_DEPTH: usize = 4;

static PRIVATE_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----.*?(?:-----END (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----|\z)",
    )
    .expect("private-key redaction regex is valid")
});

static AUTHORIZATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?P<prefix>\b(?:proxy-)?authorization\b[\"']?\s*[:=]\s*[\"']?(?:bearer|basic)[ \t]+)(?P<secret>[^\s\"'&,;}\]]+)"#,
    )
    .expect("authorization redaction regex is valid")
});

static LEADING_AUTH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\A(?P<prefix>\s*(?:bearer|basic)[ \t]+)(?P<secret>[A-Za-z0-9._~+/=-]{8,})(?P<suffix>\s*)\z"#,
    )
    .expect("leading authorization redaction regex is valid")
});

static URL_USERINFO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?P<scheme>\b[a-z][a-z0-9+.-]*://)(?P<userinfo>[^/\s?#@\"']+)@"#)
        .expect("URL userinfo redaction regex is valid")
});

static COOKIE_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?P<prefix>\b(?:set-cookie|cookie)\b[\"']?\s*:\s*)(?:\"(?P<double>[^\"\r\n]+)\"|'(?P<single>[^'\r\n]+)'|(?P<bare>[^\s,\"']+))"#,
    )
    .expect("cookie-header redaction regex is valid")
});

static SINGLE_QUOTED_COOKIE_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?P<prefix>'(?:set-cookie|cookie)\b\s*:\s*)(?P<secret>[^'\r\n]+)(?P<suffix>')")
        .expect("single-quoted cookie-header redaction regex is valid")
});

static DOUBLE_QUOTED_COOKIE_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?P<prefix>\"(?:set-cookie|cookie)\b\s*:\s*)(?P<secret>[^\"\r\n]+)(?P<suffix>\")"#,
    )
    .expect("double-quoted cookie-header redaction regex is valid")
});

static COOKIE_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?im)(?P<prefix>^\s*(?:set-cookie|cookie)\b[\"']?\s*:\s*)(?P<bare>[^\r\n]+)$"#)
        .expect("whole-line cookie-header redaction regex is valid")
});

static CREDENTIAL_ASSIGNMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    let names = concat!(
        r"(?:[a-z0-9]+[_-])?api[_-]?key",
        r"|access[_-]?token|refresh[_-]?token|auth[_-]?token",
        r"|session[_-]?token|api[_-]?token|bearer[_-]?token",
        r"|github[_-]?token|gitlab[_-]?token|id[_-]?token|token",
        r"|client[_-]?secret|aws[_-]?secret[_-]?access[_-]?key",
        r"|password|passwd|secret|credential|private[_-]?key"
    );
    Regex::new(&format!(
        r#"(?i)(?P<prefix>\b(?:{names})\b[\"']?\s*[:=]\s*)(?:\"(?P<double>[^\"\r\n]+)\"|'(?P<single>[^'\r\n]+)'|(?P<bare>[^\s&;,\"']+))"#
    ))
    .expect("credential-assignment redaction regex is valid")
});

static API_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:",
        r"sk-[A-Za-z0-9_-]{8,}",
        r"|sk_live_[A-Za-z0-9]{8,}|rk_live_[A-Za-z0-9]{8,}",
        r"|gh[pousr]_[A-Za-z0-9]{8,}|github_pat_[A-Za-z0-9_]{8,}",
        r"|xox(?:[baprs]|app)-[A-Za-z0-9-]{8,}",
        r"|AKIA[A-Z0-9]{12,}|ASIA[A-Z0-9]{12,}",
        r"|AIza[A-Za-z0-9_-]{16,}|ya29\.[A-Za-z0-9_-]{8,}",
        r"|hf_[A-Za-z0-9]{8,}|glpat-[A-Za-z0-9_-]{8,}",
        r"|npm_[A-Za-z0-9]{8,}|pypi-[A-Za-z0-9_-]{8,}",
        r"|dop_v1_[A-Za-z0-9]{8,}",
        r")\b"
    ))
    .expect("API-token redaction regex is valid")
});

fn redact_value_at_depth(
    value: &mut Value,
    key: Option<&str>,
    count: &mut usize,
    nested_json_depth: usize,
) -> anyhow::Result<()> {
    if key.is_some_and(sensitive_key) {
        *value = Value::String(REDACTION.into());
        *count += 1;
        return Ok(());
    }
    match value {
        Value::Object(object) => {
            // Object keys can carry the same recognizable credentials as string
            // values (including inside JSON-encoded tool arguments). Plan every
            // rename before mutating the map so two redacted keys can never
            // silently overwrite one another.
            let mut key_plan = HashMap::with_capacity(object.len());
            let mut redacted_keys = HashSet::with_capacity(object.len());
            let mut key_redaction_count = 0;
            for key in object.keys() {
                let mut redacted_key = key.clone();
                redact_string(
                    &mut redacted_key,
                    &mut key_redaction_count,
                    nested_json_depth,
                )?;
                if !redacted_keys.insert(redacted_key.clone()) {
                    anyhow::bail!(
                        "refusing to export: credential redaction produced duplicate JSON object keys"
                    );
                }
                key_plan.insert(key.clone(), redacted_key);
            }

            for (key, value) in object.iter_mut() {
                redact_value_at_depth(value, Some(key), count, nested_json_depth)?;
            }

            *count += key_redaction_count;
            for (original_key, value) in std::mem::take(object) {
                let redacted_key = key_plan
                    .remove(&original_key)
                    .expect("redaction key plan is built from this object");
                object.insert(redacted_key, value);
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_value_at_depth(value, None, count, nested_json_depth)?;
            }
        }
        Value::String(text) => {
            redact_string(text, count, nested_json_depth)?;
        }
        _ => {}
    }
    Ok(())
}

fn redact_string(
    text: &mut String,
    count: &mut usize,
    nested_json_depth: usize,
) -> anyhow::Result<()> {
    if nested_json_depth < MAX_NESTED_JSON_DEPTH && text.len() <= MAX_NESTED_JSON_STRING_BYTES {
        let trimmed = text.trim();
        let nested = (trimmed.starts_with('{') || trimmed.starts_with('['))
            .then(|| serde_json::from_str::<Value>(trimmed))
            .transpose()
            .ok()
            .flatten();
        if let Some(mut nested) =
            nested.filter(|value| matches!(value, Value::Object(_) | Value::Array(_)))
        {
            let mut nested_count = 0;
            redact_value_at_depth(&mut nested, None, &mut nested_count, nested_json_depth + 1)?;
            if nested_count > 0 {
                let leading_bytes = text.len() - text.trim_start().len();
                let trailing_start = text.trim_end().len();
                let serialized = serde_json::to_string(&nested)
                    .expect("an in-memory JSON value always serializes");
                text.replace_range(leading_bytes..trailing_start, &serialized);
                *count += nested_count;
                return Ok(());
            }
        }
    }

    let mut local_count = 0;
    let mut redacted = replace_whole_matches(&PRIVATE_KEY_RE, text, &mut local_count);
    redacted = replace_secret_capture(&AUTHORIZATION_RE, &redacted, &mut local_count);
    redacted = replace_secret_capture(&LEADING_AUTH_RE, &redacted, &mut local_count);
    redacted = replace_url_userinfo(&redacted, &mut local_count);
    redacted = replace_secret_capture(&SINGLE_QUOTED_COOKIE_HEADER_RE, &redacted, &mut local_count);
    redacted = replace_secret_capture(&DOUBLE_QUOTED_COOKIE_HEADER_RE, &redacted, &mut local_count);
    redacted = replace_quoted_value(&COOKIE_LINE_RE, &redacted, &mut local_count);
    redacted = replace_quoted_value(&COOKIE_HEADER_RE, &redacted, &mut local_count);
    redacted = replace_assignments(&redacted, &mut local_count);
    redacted = replace_whole_matches(&API_TOKEN_RE, &redacted, &mut local_count);
    if local_count > 0 {
        *text = redacted;
        *count += local_count;
    }
    Ok(())
}

fn replace_whole_matches(regex: &Regex, text: &str, count: &mut usize) -> String {
    regex
        .replace_all(text, |_: &Captures<'_>| {
            *count += 1;
            REDACTION
        })
        .into_owned()
}

fn replace_secret_capture(regex: &Regex, text: &str, count: &mut usize) -> String {
    regex
        .replace_all(text, |captures: &Captures<'_>| {
            *count += 1;
            let mut replacement = String::with_capacity(captures["prefix"].len() + 24);
            replacement.push_str(&captures["prefix"]);
            replacement.push_str(REDACTION);
            if let Some(suffix) = captures.name("suffix") {
                replacement.push_str(suffix.as_str());
            }
            replacement
        })
        .into_owned()
}

fn replace_url_userinfo(text: &str, count: &mut usize) -> String {
    URL_USERINFO_RE
        .replace_all(text, |captures: &Captures<'_>| {
            *count += 1;
            let userinfo = &captures["userinfo"];
            let mut replacement = String::with_capacity(captures["scheme"].len() + 32);
            replacement.push_str(&captures["scheme"]);
            if let Some((username, _)) = userinfo.split_once(':') {
                replacement.push_str(username);
                replacement.push(':');
            }
            replacement.push_str(REDACTION);
            replacement.push('@');
            replacement
        })
        .into_owned()
}

fn replace_assignments(text: &str, count: &mut usize) -> String {
    replace_quoted_value(&CREDENTIAL_ASSIGNMENT_RE, text, count)
}

fn replace_quoted_value(regex: &Regex, text: &str, count: &mut usize) -> String {
    regex
        .replace_all(text, |captures: &Captures<'_>| {
            let (secret, quote) = if let Some(value) = captures.name("double") {
                (value.as_str(), Some('"'))
            } else if let Some(value) = captures.name("single") {
                (value.as_str(), Some('\''))
            } else {
                (
                    captures
                        .name("bare")
                        .expect("assignment regex always captures one value")
                        .as_str(),
                    None,
                )
            };
            if credential_placeholder(secret) {
                return captures
                    .get(0)
                    .expect("a regex replacement always has a whole match")
                    .as_str()
                    .to_owned();
            }

            *count += 1;
            let mut replacement = String::with_capacity(captures["prefix"].len() + 26);
            replacement.push_str(&captures["prefix"]);
            if let Some(quote) = quote {
                replacement.push(quote);
            }
            replacement.push_str(REDACTION);
            if let Some(quote) = quote {
                replacement.push(quote);
            }
            replacement
        })
        .into_owned()
}

fn credential_placeholder(value: &str) -> bool {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    trimmed.starts_with('$')
        || (trimmed.starts_with("{{") && trimmed.ends_with("}}"))
        || (trimmed.starts_with('<') && trimmed.ends_with('>'))
        || trimmed == REDACTION
        || lower == "redacted"
        || lower == "none"
        || lower == "null"
        || lower.starts_with("your_")
        || lower.starts_with("your-")
}

fn delete(store: &SessionStore, id: &str) -> anyhow::Result<()> {
    let path = store.path_by_id(id)?;
    let trash = store.dir().join(".trash");
    std::fs::create_dir_all(&trash)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&trash, std::fs::Permissions::from_mode(0o700))?;
    }
    let stamp = unix_seconds(SystemTime::now());
    let destination = trash.join(format!("{id}-{stamp}.jsonl"));
    std::fs::rename(&path, &destination)?;

    let metadata = store.dir().join(".metadata").join(format!("{id}.json"));
    if metadata.is_file() {
        let metadata_destination = trash.join(format!("{id}-{stamp}.metadata.json"));
        if let Err(error) = std::fs::rename(&metadata, &metadata_destination) {
            anyhow::bail!(
                "session moved to {}, but its metadata could not be moved: {error}",
                destination.display()
            );
        }
    }
    println!(
        "Moved session {id} to {} (recoverable by moving it back).",
        destination.display()
    );
    Ok(())
}

fn repair(store: &SessionStore, id: &str) -> anyhow::Result<()> {
    let path = store.path_by_id(id)?;
    Session::open_read_only(&path).map_err(|error| {
        anyhow::anyhow!(
            "session {id:?} has completed-record corruption that automatic repair will not alter: {error}"
        )
    })?;
    let opened_path = crate::session_store::absolute_read_path(&path)?;
    let before =
        ygg_agent::secure_fs::read_regular_file_bounded(&opened_path, MAX_SESSION_FILE_BYTES)?;
    if before.is_empty() || before.ends_with(b"\n") {
        println!("Session {id} is structurally healthy; no repair was needed.");
        return Ok(());
    }

    let backup_dir = store.dir().join(".repair-backups");
    let stamp = unix_seconds(SystemTime::now());
    let backup = backup_dir.join(format!("{id}-{stamp}.jsonl"));
    crate::auth::write_private_atomic(&backup, &before, ".session-repair-")?;
    drop(Session::open(&path).map_err(|error| {
        anyhow::anyhow!(
            "repair validation failed; the source was not changed and backup is at {}: {error}",
            backup.display()
        )
    })?);
    let after = path.metadata()?.len();
    println!(
        "Repaired session {id}: {} -> {} bytes. Backup: {}",
        before.len(),
        after,
        backup.display()
    );
    Ok(())
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn relative_time(time: SystemTime, now: SystemTime) -> String {
    let Ok(elapsed) = now.duration_since(time) else {
        return "in the future".into();
    };
    let seconds = elapsed.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ygg_agent::EntryValue;
    use ygg_ai::{
        AssistantMessage, AssistantPart, Message, ModelId, Protocol, ToolCall, ToolCallId,
        ToolResult, ToolResultPart, UserMessage, UserPart,
    };

    #[test]
    fn export_redacts_keys_and_recognizable_tokens() {
        let mut value = serde_json::json!({
            "authorization": "Bearer abc",
            "nested": {"text": "sk-1234567890123456"},
            "ordinary": "hello"
        });
        let mut count = 0;
        redact_value(&mut value, None, &mut count).unwrap();
        assert_eq!(count, 2);
        assert_eq!(value["authorization"], "[REDACTED]");
        assert_eq!(value["nested"]["text"], "[REDACTED]");
        assert_eq!(value["ordinary"], "hello");
    }

    #[test]
    fn export_redacts_embedded_headers_urls_queries_and_api_tokens() {
        let mut value = Value::String(
            concat!(
                "curl -H 'Authorization: Bearer SECRET' ",
                "-H 'Proxy-Authorization: Basic dXNlcjpwYXNz' ",
                "'https://alice:hunter2@example.test/path?token=query-secret&mode=read' ",
                "payload=sk-1234567890123456; echo done"
            )
            .into(),
        );
        let mut count = 0;

        redact_value(&mut value, None, &mut count).unwrap();

        let text = value.as_str().unwrap();
        assert_eq!(count, 5, "{text}");
        assert!(text.contains("Authorization: Bearer [REDACTED]"), "{text}");
        assert!(
            text.contains("Proxy-Authorization: Basic [REDACTED]"),
            "{text}"
        );
        assert!(
            text.contains("https://alice:[REDACTED]@example.test/path?token=[REDACTED]&mode=read"),
            "{text}"
        );
        assert!(text.contains("payload=[REDACTED]; echo done"), "{text}");
        for secret in [
            "SECRET",
            "dXNlcjpwYXNz",
            "hunter2",
            "query-secret",
            "sk-1234567890123456",
        ] {
            assert!(!text.contains(secret), "secret survived redaction: {text}");
        }
    }

    #[test]
    fn export_redacts_private_key_blocks_without_discarding_prose() {
        let mut value = Value::String(
            concat!(
                "before café\n",
                "-----BEGIN ",
                "OPENSSH PRIVATE KEY-----\n",
                "b3BlbnNzaC1rZXktdjEAAAAA\n",
                "-----END ",
                "OPENSSH PRIVATE KEY-----\n",
                "after 東京"
            )
            .into(),
        );
        let mut count = 0;

        redact_value(&mut value, None, &mut count).unwrap();

        let text = value.as_str().unwrap();
        assert_eq!(count, 1);
        assert_eq!(text, "before café\n[REDACTED]\nafter 東京");
    }

    #[test]
    fn export_redacts_nested_json_strings_and_preserves_utf8() {
        let nested = serde_json::json!({
            "api-key": "sëcret value",
            "message": "keep café 東京",
            "nested": {"clientSecret": "another secret"}
        })
        .to_string();
        let mut value = serde_json::json!({"tool_result": nested});
        let mut count = 0;

        redact_value(&mut value, None, &mut count).unwrap();

        assert_eq!(count, 2);
        let reparsed: Value = serde_json::from_str(value["tool_result"].as_str().unwrap()).unwrap();
        assert_eq!(reparsed["api-key"], REDACTION);
        assert_eq!(reparsed["nested"]["clientSecret"], REDACTION);
        assert_eq!(reparsed["message"], "keep café 東京");
        assert!(!value.to_string().contains("sëcret value"));
        assert!(!value.to_string().contains("another secret"));
    }

    #[test]
    fn export_rejects_nested_json_key_collisions_after_redaction() {
        let original = serde_json::json!({
            "sk-collisionvalue123456": "first",
            "ghp_collisionvalue123456": "second"
        })
        .to_string();
        let mut value = Value::String(original.clone());
        let mut count = 0;

        let error = redact_value(&mut value, None, &mut count).unwrap_err();

        assert!(error.to_string().contains("duplicate JSON object keys"));
        assert!(!error.to_string().contains("collisionvalue"));
        assert_eq!(value, original);
        assert_eq!(count, 0);
    }

    #[test]
    fn export_redacts_cookie_headers_embedded_in_curl_and_json_strings() {
        let nested = serde_json::json!({
            "headers": "Set-Cookie: sid=server-secret; HttpOnly",
            "keep": "café"
        })
        .to_string();
        let mut value = serde_json::json!({
            "command": "curl -H 'Cookie: session=user-secret; csrf=csrf-secret' https://example.test",
            "payload": nested
        });
        let mut count = 0;

        redact_value(&mut value, None, &mut count).unwrap();

        assert_eq!(count, 2, "{value}");
        assert_eq!(
            value["command"],
            "curl -H 'Cookie: [REDACTED]' https://example.test"
        );
        let reparsed: Value = serde_json::from_str(value["payload"].as_str().unwrap()).unwrap();
        assert_eq!(reparsed["headers"], "Set-Cookie: [REDACTED]");
        assert_eq!(reparsed["keep"], "café");
        let rendered = value.to_string();
        for secret in ["user-secret", "csrf-secret", "server-secret"] {
            assert!(
                !rendered.contains(secret),
                "secret survived redaction: {rendered}"
            );
        }
    }

    #[test]
    fn export_keeps_ordinary_prose_urls_and_placeholders_unchanged() {
        let original = concat!(
            "The bearer carries a token across the bridge. ",
            "https://example.test/path?mode=read max_tokens=4096 ",
            "token=$TOKEN api_key={{API_KEY}} password=<PASSWORD>"
        );
        let mut value = Value::String(original.into());
        let mut count = 0;

        redact_value(&mut value, None, &mut count).unwrap();

        assert_eq!(count, 0);
        assert_eq!(value, original);
    }

    #[test]
    fn export_redaction_counts_each_embedded_credential_fragment_once() {
        let mut value = Value::String(
            "token='first secret' and api_key=second-secret and ghp_1234567890abcdef".into(),
        );
        let mut count = 0;

        redact_value(&mut value, None, &mut count).unwrap();

        assert_eq!(count, 3, "{}", value.as_str().unwrap());
        assert_eq!(
            value,
            "token='[REDACTED]' and api_key=[REDACTED] and [REDACTED]"
        );
    }

    #[test]
    fn export_ignores_only_an_unterminated_invalid_utf8_tail() {
        let mut bytes = b"{\"type\":\"head\",\"id\":\"001\"}\n{\"partial\":\"".to_vec();
        bytes.extend_from_slice(&[0xf0, 0x9f]);
        let (records, ignored) = parse_export_records(&bytes).unwrap();
        assert_eq!(records.len(), 1);
        assert!(ignored);

        let error = parse_export_records(b"{\"bad\":\"\xff\"}\n").unwrap_err();
        assert!(error.to_string().contains("completed session records"));
    }

    #[test]
    fn portable_export_validates_redacts_and_keeps_user_metadata() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let destination_root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("exportable.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("sk-1234567890123456".into())],
            })))
            .unwrap();
        drop(session);
        store.rename("exportable", "Local review").unwrap();
        store
            .set_tags("exportable", vec!["local-model".into()])
            .unwrap();

        let destination = destination_root.path().join("portable.json");
        let report = export_portable(
            &store,
            "exportable",
            Some(destination.clone()),
            destination_root.path(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(report.destination, destination);
        assert_eq!(report.redaction_count, 1);
        assert!(!report.included_secrets);
        let export: Value =
            serde_json::from_slice(&std::fs::read(&report.destination).unwrap()).unwrap();
        assert_eq!(export["format"], "ygg-session-export");
        assert_eq!(export["metadata"]["name"], "Local review");
        assert_eq!(export["metadata"]["tags"][0], "local-model");
        assert!(export["records"].to_string().contains("[REDACTED]"));
        assert!(!export["records"].to_string().contains("sk-123"));
    }

    #[test]
    fn portable_export_redacts_the_entire_package_including_derived_fields() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let destination_root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let id = "sk-sourceid12345678";
        let title_secret = "ghp_titlevalue12345678";
        let name_secret = "xoxb-namevalue123456";
        let tag_secret = "hf_tagvalue123456";
        let call_id_secret = "github_pat_toolcall123456";
        let metadata_secret = "extension-secret-value";
        let result_secret = "result-secret-value";
        let path = store.dir().join(format!("{id}.jsonl"));
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text(title_secret.into())],
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId(call_id_secret.into()),
                    name: "extension-review".into(),
                    arguments_json: serde_json::json!({
                        "client_secret": metadata_secret,
                    })
                    .to_string(),
                })],
                model: ModelId("test".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ToolResult {
                    tool_call_id: ToolCallId(call_id_secret.into()),
                    content: vec![ToolResultPart::Text(format!(
                        "Authorization: Bearer {result_secret}"
                    ))],
                    is_error: false,
                })],
            })))
            .unwrap();
        drop(session);
        store.rename(id, name_secret).unwrap();
        store.set_tags(id, vec![tag_secret.into()]).unwrap();

        let destination = destination_root.path().join("portable.json");
        let report = export_portable(
            &store,
            id,
            Some(destination),
            destination_root.path(),
            false,
            false,
        )
        .unwrap();
        let export: Value =
            serde_json::from_slice(&std::fs::read(report.destination).unwrap()).unwrap();
        let serialized = export.to_string();
        for secret in [
            id,
            title_secret,
            name_secret,
            tag_secret,
            call_id_secret,
            metadata_secret,
            result_secret,
        ] {
            assert!(
                !serialized.contains(secret),
                "secret survived full-package redaction: {secret}: {serialized}"
            );
        }
        assert_eq!(export["source_id"], REDACTION);
        assert_eq!(export["source_title"], REDACTION);
        assert_eq!(export["metadata"]["name"], REDACTION);
        assert_eq!(export["metadata"]["tags"][0], REDACTION);
        assert!(export["records"].to_string().contains(REDACTION));
        assert!(report.redaction_count >= 7);
    }

    #[test]
    fn portable_export_redacts_credentials_used_as_nested_json_object_keys() {
        fn find_field<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
            match value {
                Value::Object(object) => object
                    .get(field)
                    .or_else(|| object.values().find_map(|value| find_field(value, field))),
                Value::Array(values) => values.iter().find_map(|value| find_field(value, field)),
                _ => None,
            }
        }

        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let destination_root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let id = "nested-key-export";
        let object_key_secret = "sk-objectkeyvalue123456";
        let nested_arguments = Value::Object(
            [(
                "outer".to_owned(),
                Value::Object(
                    [(
                        object_key_secret.to_owned(),
                        Value::String("keep me".into()),
                    )]
                    .into_iter()
                    .collect(),
                ),
            )]
            .into_iter()
            .collect(),
        )
        .to_string();
        let path = store.dir().join(format!("{id}.jsonl"));
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("ordinary title".into())],
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("ordinary-call-id".into()),
                    name: "extension-review".into(),
                    arguments_json: nested_arguments,
                })],
                model: ModelId("test".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        drop(session);

        let destination = destination_root.path().join("portable.json");
        let report = export_portable(
            &store,
            id,
            Some(destination),
            destination_root.path(),
            false,
            false,
        )
        .unwrap();
        let export: Value =
            serde_json::from_slice(&std::fs::read(report.destination).unwrap()).unwrap();
        let serialized = export.to_string();
        assert!(!serialized.contains(object_key_secret));
        let arguments = find_field(&export["records"], "arguments_json")
            .and_then(Value::as_str)
            .expect("export keeps the tool arguments JSON string");
        let arguments: Value = serde_json::from_str(arguments).unwrap();
        assert_eq!(arguments["outer"][REDACTION], "keep me");
        assert_eq!(report.redaction_count, 1);
    }

    #[test]
    fn portable_export_write_failure_leaves_no_partial_destination_or_temp_file() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let destination_root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("atomic.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("atomic export".into())],
            })))
            .unwrap();
        drop(session);
        let destination = destination_root.path().join("occupied");
        std::fs::create_dir(&destination).unwrap();

        let error = match export_portable(
            &store,
            "atomic",
            Some(destination.clone()),
            destination_root.path(),
            false,
            true,
        ) {
            Ok(_) => panic!("replacing a directory with an export must fail"),
            Err(error) => error,
        };

        assert!(!error.to_string().is_empty());
        assert!(destination.is_dir());
        let leftovers = std::fs::read_dir(destination_root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("occupied")]);
    }

    #[test]
    fn repair_removes_only_a_torn_tail_and_keeps_a_backup() {
        use std::io::Write as _;

        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("torn.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("keep me".into())],
            })))
            .unwrap();
        drop(session);
        let healthy_len = path.metadata().unwrap().len();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":\"entry\"")
            .unwrap();

        repair(&store, "torn").unwrap();

        assert_eq!(path.metadata().unwrap().len(), healthy_len);
        assert!(std::fs::read(&path).unwrap().ends_with(b"\n"));
        assert_eq!(
            std::fs::read_dir(store.dir().join(".repair-backups"))
                .unwrap()
                .count(),
            1
        );
        assert!(Session::open_read_only(&path).is_ok());
    }

    #[test]
    fn delete_is_recoverable_and_moves_metadata() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("recoverable.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("hello".into())],
            })))
            .unwrap();
        drop(session);
        store.rename("recoverable", "Named").unwrap();

        delete(&store, "recoverable").unwrap();
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_dir(store.dir().join(".trash"))
                .unwrap()
                .count(),
            2
        );
    }

    #[test]
    fn query_covers_names_tags_ids_and_paths() {
        let session = SessionMeta {
            id: "2026-07-21-demo".into(),
            path: PathBuf::from("/work/project/session.jsonl"),
            title: "Compiler investigation".into(),
            name: Some("Compiler investigation".into()),
            tags: vec!["rust".into(), "local-model".into()],
            modified: UNIX_EPOCH,
        };
        for query in ["compiler", "local-model", "2026-07", "project"] {
            assert!(matches_query(&session, Some(query)));
        }
        assert!(!matches_query(&session, Some("unrelated")));
    }

    #[test]
    fn relative_modified_time_is_compact_and_readable() {
        let now = UNIX_EPOCH + std::time::Duration::from_secs(10 * 86_400);
        assert_eq!(relative_time(now, now), "0s ago");
        assert_eq!(
            relative_time(now - std::time::Duration::from_secs(90), now),
            "1m ago"
        );
        assert_eq!(
            relative_time(now - std::time::Duration::from_secs(7_200), now),
            "2h ago"
        );
        assert_eq!(
            relative_time(now - std::time::Duration::from_secs(3 * 86_400), now),
            "3d ago"
        );
        assert_eq!(
            relative_time(now + std::time::Duration::from_secs(1), now),
            "in the future"
        );
    }
}
