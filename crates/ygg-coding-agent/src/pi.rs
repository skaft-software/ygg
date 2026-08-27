#![allow(missing_docs)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ygg_agent::extension_process::{
    ExtensionCapabilities, ExtensionEntrypoint, ExtensionFilesystemAccess, ExtensionHook,
    ExtensionManifest, ExtensionUiSurface, ManifestContributions,
};
use ygg_agent::EXTENSION_API_VERSION_0_2;

const BRIDGE_VERSION: &str = "0.1.1";
const LINK_RECORD: &str = "pi-link.json";
const MAX_SOURCE_PATH_BYTES: usize = 4096;

#[derive(Clone, Debug, Subcommand)]
pub enum PiCommand {
    /// Create an inert Ygg wrapper for an existing local Pi extension/package.
    Install {
        /// A local .ts/.js extension file or an installed Pi package directory.
        source: PathBuf,
        /// Override the generated Ygg extension name.
        #[arg(long)]
        name: Option<String>,
        /// Pi's user agent directory. Defaults to PI_CODING_AGENT_DIR or ~/.pi/agent.
        #[arg(long, value_name = "DIR")]
        pi_home: Option<PathBuf>,
        /// Ygg extension root used for the generated compatibility link.
        #[arg(long, value_name = "DIR")]
        extension_root: Option<PathBuf>,
    },
    /// List generated Pi compatibility links.
    List {
        /// Ygg extension root used for generated compatibility links.
        #[arg(long, value_name = "DIR")]
        extension_root: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PiLinkRecord {
    schema_version: u32,
    bridge_version: String,
    name: String,
    source: PathBuf,
    pi_home: PathBuf,
}

pub fn run(command: PiCommand, invocation_cwd: &Path) -> anyhow::Result<()> {
    match command {
        PiCommand::Install {
            source,
            name,
            pi_home,
            extension_root,
        } => install(
            &source,
            name.as_deref(),
            pi_home.as_deref(),
            extension_root.as_deref(),
            invocation_cwd,
        ),
        PiCommand::List { extension_root } => list(extension_root.as_deref()),
    }
}

fn install(
    source: &Path,
    requested_name: Option<&str>,
    requested_pi_home: Option<&Path>,
    requested_extension_root: Option<&Path>,
    invocation_cwd: &Path,
) -> anyhow::Result<()> {
    let source = resolve_source(source, invocation_cwd)?;
    let pi_home = resolve_pi_home(requested_pi_home, invocation_cwd)?;
    let extension_root = resolve_extension_root(requested_extension_root)?;
    fs::create_dir_all(&extension_root).with_context(|| {
        format!(
            "cannot create Ygg extension root {}",
            extension_root.display()
        )
    })?;
    reject_symlink(&extension_root, "Ygg extension root")?;

    let name = requested_name
        .map(validate_name)
        .transpose()?
        .unwrap_or_else(|| generated_name(&source));
    let package = extension_root.join(&name);
    if package.exists() {
        anyhow::bail!(
            "Pi compatibility link {name:?} already exists at {}; remove it manually before reinstalling",
            package.display()
        );
    }
    fs::create_dir(&package)
        .with_context(|| format!("cannot create Pi compatibility link {}", package.display()))?;

    let bridge_path = package.join("bridge.mjs");
    write_private_file(
        &bridge_path,
        include_str!("../../../extensions/ygg-pi-compat/bridge.mjs"),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bridge_path, fs::Permissions::from_mode(0o700))?;
    }

    let manifest = manifest(&name, &source, &pi_home, &bridge_path)?;
    let manifest_path = package.join("extension.toml");
    let manifest_text = toml::to_string_pretty(&manifest)?;
    write_private_file(&manifest_path, &manifest_text)?;

    let record = PiLinkRecord {
        schema_version: 1,
        bridge_version: BRIDGE_VERSION.to_owned(),
        name: name.clone(),
        source: source.clone(),
        pi_home: pi_home.clone(),
    };
    write_private_file(
        &package.join(LINK_RECORD),
        &format!("{}\n", serde_json::to_string_pretty(&record)?),
    )?;

    crate::output::stdout_line(format!(
        "Installed Pi compatibility link {name} for {}.",
        source.display()
    ));
    crate::output::stdout_line(
        "The link remains disabled and untrusted until you explicitly enable and trust it.",
    );
    crate::output::stdout_line(format!(
        "Run: ygg --enable-extension {name} --trust-extension {name}"
    ));
    crate::output::stdout_line(
        "No Pi package code, npm lifecycle hook, or dependency installer was run.",
    );
    Ok(())
}

fn list(requested_extension_root: Option<&Path>) -> anyhow::Result<()> {
    let root = resolve_extension_root(requested_extension_root)?;
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::output::stdout_line("No Pi compatibility links installed.");
            return Ok(());
        }
        Err(error) => return Err(error).context("cannot read Pi compatibility links"),
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() || entry.file_type()?.is_symlink() {
            continue;
        }
        let record_path = path.join(LINK_RECORD);
        let bytes = match fs::read(&record_path) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let record: PiLinkRecord = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(_) => continue,
        };
        records.push(record);
    }
    records.sort_by(|left, right| left.name.cmp(&right.name));
    if records.is_empty() {
        crate::output::stdout_line("No Pi compatibility links installed.");
        return Ok(());
    }
    for record in records {
        crate::output::stdout_line(format!(
            "{} · source={} · pi_home={}",
            record.name,
            record.source.display(),
            record.pi_home.display()
        ));
    }
    Ok(())
}

fn manifest(
    name: &str,
    source: &Path,
    pi_home: &Path,
    bridge_path: &Path,
) -> anyhow::Result<ExtensionManifest> {
    let source_text = source.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "Pi extension source path is not valid UTF-8: {}",
            source.display()
        )
    })?;
    let pi_home_text = pi_home
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Pi home path is not valid UTF-8: {}", pi_home.display()))?;
    let bridge_text = bridge_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "Pi bridge path is not valid UTF-8: {}",
            bridge_path.display()
        )
    })?;

    Ok(ExtensionManifest {
        name: name.to_owned(),
        version: BRIDGE_VERSION.to_owned(),
        api_version: EXTENSION_API_VERSION_0_2.to_owned(),
        requires_ygg: None,
        description: Some(format!("Pi compatibility link for {source_text}")),
        entrypoint: ExtensionEntrypoint {
            command: "node".to_owned(),
            args: vec![
                bridge_text.to_owned(),
                "--extension".to_owned(),
                source_text.to_owned(),
                "--agent-dir".to_owned(),
                pi_home_text.to_owned(),
                "--command".to_owned(),
                name.to_owned(),
            ],
            env: Default::default(),
        },
        capabilities: ExtensionCapabilities {
            filesystem: ExtensionFilesystemAccess::Unrestricted,
            process: true,
            network: true,
            secrets: Vec::new(),
            environment: Vec::new(),
        },
        contributes: ManifestContributions {
            tools: Vec::new(),
            commands: vec![name.to_owned()],
            hooks: vec![
                ExtensionHook::AfterResponse,
                ExtensionHook::BeforeToolCall,
                ExtensionHook::AfterToolCall,
            ],
            ui: vec![ExtensionUiSurface::Status],
            context: true,
            tool_renderers: Vec::new(),
            notifications: true,
            confirmations: true,
            presentation: false,
        },
    })
}

fn resolve_source(source: &Path, cwd: &Path) -> anyhow::Result<PathBuf> {
    let path = if source.is_absolute() {
        source.to_owned()
    } else {
        cwd.join(source)
    };
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("cannot inspect Pi extension source {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "Pi extension source must not be a symlink: {}",
            path.display()
        );
    }
    if !metadata.is_file() && !metadata.is_dir() {
        anyhow::bail!(
            "Pi extension source must be a regular file or directory: {}",
            path.display()
        );
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve Pi extension source {}", path.display()))?;
    if canonical.to_string_lossy().len() > MAX_SOURCE_PATH_BYTES {
        anyhow::bail!("Pi extension source path exceeds {MAX_SOURCE_PATH_BYTES} bytes");
    }
    Ok(canonical)
}

fn resolve_pi_home(requested: Option<&Path>, cwd: &Path) -> anyhow::Result<PathBuf> {
    if let Some(path) = requested {
        return absolute_path(path, cwd);
    }
    if let Some(path) = std::env::var_os("PI_CODING_AGENT_DIR") {
        return absolute_path(Path::new(&path), cwd);
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("user home is unavailable"))?;
    Ok(home.join(".pi/agent"))
}

fn resolve_extension_root(requested: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(path) = requested {
        return if path.is_absolute() {
            Ok(path.to_owned())
        } else {
            Ok(std::env::current_dir()?.join(path))
        };
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("user home is unavailable"))?;
    Ok(home.join(".ygg/extensions"))
}

fn absolute_path(path: &Path, cwd: &Path) -> anyhow::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    Ok(path)
}

fn reject_symlink(path: &Path, label: &str) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("{label} must not be a symlink: {}", path.display());
    }
    Ok(())
}

fn write_private_file(path: &Path, content: &str) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn validate_name(name: &str) -> anyhow::Result<String> {
    if name.len() > 64
        || name.is_empty()
        || !name.chars().enumerate().all(|(index, character)| {
            (index == 0 && character.is_ascii_lowercase())
                || (index > 0
                    && (character.is_ascii_lowercase()
                        || character.is_ascii_digit()
                        || character == '-'))
        })
    {
        anyhow::bail!(
            "invalid Pi compatibility name {name:?}; use a lowercase letter followed by lowercase letters, digits, or '-'")
    }
    Ok(name.to_owned())
}

fn generated_name(source: &Path) -> String {
    let stem = source
        .file_stem()
        .or_else(|| source.file_name())
        .and_then(|value| value.to_str())
        .unwrap_or("extension")
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() {
                character
            } else if character.is_ascii_uppercase() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let stem = stem.trim_matches('-');
    let stem = if stem.is_empty() { "extension" } else { stem };
    let digest = format!("{:x}", Sha256::digest(source.to_string_lossy().as_bytes()));
    format!("pi-{stem}-{}", &digest[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_are_stable_and_lowercase() {
        let name = generated_name(Path::new("/tmp/My Extension.ts"));
        assert!(name.starts_with("pi-my-extension-"));
        assert!(validate_name(&name).is_ok());
    }

    #[test]
    fn manifest_uses_the_generic_pi_command_and_lifecycle_hooks() {
        let manifest = manifest(
            "pi-example",
            Path::new("/tmp/example.ts"),
            Path::new("/tmp/pi/agent"),
            Path::new("/tmp/link/bridge.mjs"),
        )
        .unwrap();
        assert_eq!(manifest.entrypoint.command, "node");
        assert_eq!(manifest.contributes.commands, ["pi-example"]);
        assert_eq!(manifest.contributes.hooks.len(), 3);
        assert!(!manifest
            .contributes
            .hooks
            .contains(&ExtensionHook::BeforePrompt));
        assert!(manifest.contributes.context);
    }
}
