//! Tauri IPC handlers; mirror [`crate::store::KeyringStore`] for frontend access.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tauri::{command, AppHandle, Manager, Runtime};
use zeroize::Zeroize;

use crate::models::*;
use crate::plugin::KeyringPlugin;
use crate::store::KeyringStore;
use crate::Result;

/// Max items per bulk / backup list (doS / IPC payload guard).
const MAX_BULK_ITEMS: usize = 256;

const BACKUP_FORMAT_VERSION: u32 = 1;

fn normalize_account(account: String) -> crate::Result<String> {
    let account = account.trim().to_owned();
    if account.is_empty() {
        return Err(crate::Error::Naming("account must not be empty".into()));
    }
    if account.len() > 512 {
        return Err(crate::Error::Naming(
            "account string too long (max 512)".into(),
        ));
    }
    Ok(account)
}

fn ensure_bulk_len(n: usize) -> crate::Result<()> {
    if n > MAX_BULK_ITEMS {
        return Err(crate::Error::Naming(format!(
            "too many items (max {MAX_BULK_ITEMS})"
        )));
    }
    Ok(())
}

fn apply_plain_backup(store: &KeyringStore, backup: PasswordBackupPlainDto) -> crate::Result<()> {
    if backup.format_version != BACKUP_FORMAT_VERSION {
        return Err(crate::Error::Encoding(format!(
            "unsupported backup format version {} (expected {BACKUP_FORMAT_VERSION})",
            backup.format_version
        )));
    }
    for row in backup.entries {
        if let Some(secret) = row.secret {
            let account = normalize_account(row.account)?;
            store.set_password(&account, &secret)?;
        }
    }
    Ok(())
}

async fn collect_plain_backup<R: Runtime>(
    app: &AppHandle<R>,
    accounts: Vec<String>,
) -> Result<PasswordBackupPlainDto> {
    ensure_bulk_len(accounts.len())?;
    let store = app.state::<KeyringPlugin>().store.clone();
    let mut entries = Vec::with_capacity(accounts.len());
    for a in accounts {
        let account = normalize_account(a)?;
        let secret = store.get_password(&account)?;
        entries.push(PasswordBackupEntryDto { account, secret });
    }
    Ok(PasswordBackupPlainDto {
        format_version: BACKUP_FORMAT_VERSION,
        entries,
    })
}

#[command]
pub(crate) async fn get_passwords<R: Runtime>(
    app: AppHandle<R>,
    accounts: Vec<String>,
) -> Result<Vec<Option<String>>> {
    ensure_bulk_len(accounts.len())?;
    let store = app.state::<KeyringPlugin>().store.clone();
    let mut out = Vec::with_capacity(accounts.len());
    for a in accounts {
        let account = normalize_account(a)?;
        out.push(store.get_password(&account)?);
    }
    Ok(out)
}

#[command]
pub(crate) async fn set_passwords<R: Runtime>(
    app: AppHandle<R>,
    entries: Vec<PasswordEntryDto>,
) -> Result<()> {
    ensure_bulk_len(entries.len())?;
    let store = app.state::<KeyringPlugin>().store.clone();
    for e in entries {
        let account = normalize_account(e.account)?;
        store.set_password(&account, &e.secret)?;
    }
    Ok(())
}

#[command]
pub(crate) async fn delete_passwords<R: Runtime>(
    app: AppHandle<R>,
    accounts: Vec<String>,
) -> Result<()> {
    ensure_bulk_len(accounts.len())?;
    let store = app.state::<KeyringPlugin>().store.clone();
    for a in accounts {
        let account = normalize_account(a)?;
        store.delete(&account)?;
    }
    Ok(())
}

#[command]
pub(crate) async fn password_exists<R: Runtime>(
    app: AppHandle<R>,
    account: String,
) -> Result<bool> {
    let account = normalize_account(account)?;
    let store = app.state::<KeyringPlugin>().store.clone();
    store.exists_nonempty(&account)
}

#[command]
pub(crate) async fn export_passwords_plain<R: Runtime>(
    app: AppHandle<R>,
    accounts: Vec<String>,
) -> Result<PasswordBackupPlainDto> {
    collect_plain_backup(&app, accounts).await
}

#[command]
pub(crate) async fn import_passwords_plain<R: Runtime>(
    app: AppHandle<R>,
    backup: PasswordBackupPlainDto,
) -> Result<()> {
    let store = app.state::<KeyringPlugin>().store.clone();
    apply_plain_backup(store.as_ref(), backup)
}

#[command]
pub(crate) async fn export_passwords_encrypted<R: Runtime>(
    app: AppHandle<R>,
    accounts: Vec<String>,
    passphrase: String,
) -> Result<PasswordBackupEncryptedDto> {
    let plain = collect_plain_backup(&app, accounts).await?;
    let mut pw = passphrase.into_bytes();
    let json = serde_json::to_vec(&plain).map_err(|e| crate::Error::Encoding(e.to_string()))?;
    let out = crate::backup_crypto::encrypt_plain_bytes(&json, &pw);
    pw.zeroize();
    out
}

#[command]
pub(crate) async fn import_passwords_encrypted<R: Runtime>(
    app: AppHandle<R>,
    backup: PasswordBackupEncryptedDto,
    passphrase: String,
) -> Result<()> {
    let mut pw = passphrase.into_bytes();
    let plain = crate::backup_crypto::decrypt_to_plain(&backup, &pw)?;
    pw.zeroize();
    let backup_plain: PasswordBackupPlainDto =
        serde_json::from_slice(&plain).map_err(|e| crate::Error::Encoding(e.to_string()))?;
    let store = app.state::<KeyringPlugin>().store.clone();
    apply_plain_backup(store.as_ref(), backup_plain)
}

#[command]
pub(crate) async fn ping<R: Runtime>(
    _app: AppHandle<R>,
    payload: PingRequest,
) -> Result<PingResponse> {
    Ok(PingResponse {
        value: payload.value,
    })
}

#[command]
pub(crate) async fn initialize<R: Runtime>(
    app: AppHandle<R>,
    snapshot_path: PathBuf,
    mut password: String,
) -> Result<()> {
    password.zeroize();
    let plugin = app.state::<KeyringPlugin>();
    let path = snapshot_path.to_string_lossy().into_owned();
    plugin.sessions.insert(path);
    Ok(())
}

#[command]
pub(crate) async fn destroy<R: Runtime>(app: AppHandle<R>, snapshot_path: PathBuf) -> Result<()> {
    let plugin = app.state::<KeyringPlugin>();
    let path = snapshot_path.to_string_lossy();
    plugin.sessions.remove(path.as_ref());
    Ok(())
}

#[command]
pub(crate) async fn save<R: Runtime>(_app: AppHandle<R>, _snapshot_path: PathBuf) -> Result<()> {
    Ok(())
}

#[command]
pub(crate) async fn create_client<R: Runtime>(
    app: AppHandle<R>,
    snapshot_path: PathBuf,
    #[allow(unused_variables)] client: BytesDto,
) -> Result<()> {
    let plugin = app.state::<KeyringPlugin>();
    require_session(plugin.inner(), &snapshot_path)?;
    Ok(())
}

#[command]
pub(crate) async fn load_client<R: Runtime>(
    app: AppHandle<R>,
    snapshot_path: PathBuf,
    #[allow(unused_variables)] client: BytesDto,
) -> Result<()> {
    let plugin = app.state::<KeyringPlugin>();
    require_session(plugin.inner(), &snapshot_path)?;
    Ok(())
}

#[command]
pub(crate) async fn get_store_record<R: Runtime>(
    app: AppHandle<R>,
    snapshot_path: PathBuf,
    client: BytesDto,
    key: String,
) -> Result<Option<Vec<u8>>> {
    let plugin = app.state::<KeyringPlugin>();
    require_session(plugin.inner(), &snapshot_path)?;
    let store = plugin.store.as_ref();
    let account = store.account_store_key(&snapshot_path.to_string_lossy(), &client, &key);
    store.get_bytes(&account)
}

#[command]
pub(crate) async fn save_store_record<R: Runtime>(
    app: AppHandle<R>,
    snapshot_path: PathBuf,
    client: BytesDto,
    key: String,
    value: Vec<u8>,
    #[allow(unused_variables)] lifetime: Option<DurationDto>,
) -> Result<Option<Vec<u8>>> {
    let _ = lifetime.map(Duration::from);
    let plugin = app.state::<KeyringPlugin>();
    require_session(plugin.inner(), &snapshot_path)?;
    let store = plugin.store.as_ref();
    let account = store.account_store_key(&snapshot_path.to_string_lossy(), &client, &key);
    let prev = store.get_bytes(&account)?;
    store.set_bytes(&account, &value)?;
    Ok(prev)
}

#[command]
pub(crate) async fn remove_store_record<R: Runtime>(
    app: AppHandle<R>,
    snapshot_path: PathBuf,
    client: BytesDto,
    key: String,
) -> Result<Option<Vec<u8>>> {
    let plugin = app.state::<KeyringPlugin>();
    require_session(plugin.inner(), &snapshot_path)?;
    let store = plugin.store.as_ref();
    let account = store.account_store_key(&snapshot_path.to_string_lossy(), &client, &key);
    let prev = store.get_bytes(&account)?;
    store.delete(&account)?;
    Ok(prev)
}

#[command]
pub(crate) async fn save_secret<R: Runtime>(
    app: AppHandle<R>,
    snapshot_path: PathBuf,
    client: BytesDto,
    vault: BytesDto,
    record_path: BytesDto,
    secret: Vec<u8>,
) -> Result<()> {
    let plugin = app.state::<KeyringPlugin>();
    require_session(plugin.inner(), &snapshot_path)?;
    let store = plugin.store.as_ref();
    let account = store.account_vault_record(
        &snapshot_path.to_string_lossy(),
        &client,
        &vault,
        &record_path,
    );
    store.set_bytes(&account, &secret)
}

#[command]
pub(crate) async fn remove_secret<R: Runtime>(
    app: AppHandle<R>,
    snapshot_path: PathBuf,
    client: BytesDto,
    vault: BytesDto,
    record_path: BytesDto,
) -> Result<()> {
    let plugin = app.state::<KeyringPlugin>();
    require_session(plugin.inner(), &snapshot_path)?;
    let store = plugin.store.as_ref();
    let account = store.account_vault_record(
        &snapshot_path.to_string_lossy(),
        &client,
        &vault,
        &record_path,
    );
    store.delete(&account)
}

#[cfg(feature = "crypto")]
#[command]
pub(crate) async fn execute_procedure<R: Runtime>(
    app: AppHandle<R>,
    snapshot_path: PathBuf,
    client: BytesDto,
    procedure: ProcedureDto,
) -> Result<Vec<u8>> {
    let plugin = app.state::<KeyringPlugin>();
    require_session(plugin.inner(), &snapshot_path)?;
    let store = plugin.store.as_ref();
    crate::crypto::execute_procedure(store, &snapshot_path.to_string_lossy(), &client, procedure)
}

#[cfg(not(feature = "crypto"))]
#[command]
pub(crate) async fn execute_procedure<R: Runtime>(
    _app: AppHandle<R>,
    _snapshot_path: PathBuf,
    _client: BytesDto,
    _procedure: ProcedureDto,
) -> Result<Vec<u8>> {
    Err(crate::Error::Crypto(
        "tauri-plugin-keyring-store was built without the `crypto` feature".into(),
    ))
}

fn require_session(plugin: &KeyringPlugin, snapshot: &Path) -> Result<()> {
    let s = snapshot.to_string_lossy();
    if plugin.sessions.contains(s.as_ref()) {
        Ok(())
    } else {
        Err(crate::Error::SessionNotInitialized(s.into_owned()))
    }
}
