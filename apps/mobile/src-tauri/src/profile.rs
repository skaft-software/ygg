use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use ygg_companion_protocol::{DeviceSummary, PairingDeviceClaim, PairingTicket, PROTOCOL_VERSION};

const PROFILE_FILE: &str = "host-profile-v1.json";
const PENDING_FILE: &str = "pending-pairing-v1.json";
const ACCESS_REMOVAL_FILE: &str = "access-removal-v1.json";
const MAX_STATE_BYTES: usize = 32 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccessRemovalMarker {
    version: u16,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProfileError {
    #[error("companion profile storage is unavailable")]
    Unavailable,
    #[error("companion profile storage is invalid")]
    Invalid,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HostTarget {
    pub(crate) host_id: String,
    pub(crate) host_endpoint_id: String,
    pub(crate) relay_urls: Vec<String>,
    pub(crate) direct_addresses: Vec<String>,
}

impl HostTarget {
    pub(crate) fn from_ticket(ticket: &PairingTicket) -> Self {
        Self {
            host_id: ticket.host_id.clone(),
            host_endpoint_id: ticket.host_endpoint_id.clone(),
            relay_urls: ticket.relay_urls.clone(),
            direct_addresses: ticket.direct_addresses.clone(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ProfileError> {
        validate_identifier(&self.host_id)?;
        validate_identifier(&self.host_endpoint_id)?;
        if self.relay_urls.is_empty()
            || self.relay_urls.len() > 8
            || self.direct_addresses.len() > 16
        {
            return Err(ProfileError::Invalid);
        }
        if self.relay_urls.iter().any(|relay| {
            relay.len() > 512
                || !relay.starts_with("https://")
                || !relay.is_ascii()
                || relay.bytes().any(|byte| byte.is_ascii_control())
        }) || self.direct_addresses.iter().any(|address| {
            address.is_empty()
                || address.len() > 128
                || !address.is_ascii()
                || address.bytes().any(|byte| byte.is_ascii_control())
        }) {
            return Err(ProfileError::Invalid);
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PendingPairing {
    version: u16,
    pub(crate) request_id: String,
    pub(crate) target: HostTarget,
    pub(crate) device: PairingDeviceClaim,
    pub(crate) phrase: String,
    pub(crate) expires_at_ms: u64,
}

impl PendingPairing {
    pub(crate) fn new(
        request_id: String,
        target: HostTarget,
        device: PairingDeviceClaim,
        phrase: String,
        expires_at_ms: u64,
    ) -> Result<Self, ProfileError> {
        let pending = Self {
            version: PROTOCOL_VERSION,
            request_id,
            target,
            device,
            phrase,
            expires_at_ms,
        };
        pending.validate()?;
        Ok(pending)
    }

    pub(crate) fn validate(&self) -> Result<(), ProfileError> {
        if self.version != PROTOCOL_VERSION || self.expires_at_ms == 0 {
            return Err(ProfileError::Invalid);
        }
        validate_identifier(&self.request_id)?;
        self.target.validate()?;
        self.device.validate().map_err(|_| ProfileError::Invalid)?;
        if self.phrase.is_empty()
            || self.phrase.len() > 128
            || self.phrase.chars().any(char::is_control)
        {
            return Err(ProfileError::Invalid);
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HostProfile {
    version: u16,
    pub(crate) target: HostTarget,
    pub(crate) device_endpoint_id: String,
    pub(crate) device_id: String,
    pub(crate) device_name: String,
    pub(crate) paired_at_ms: u64,
}

impl HostProfile {
    pub(crate) fn from_approval(
        target: HostTarget,
        device_endpoint_id: String,
        device: &DeviceSummary,
    ) -> Result<Self, ProfileError> {
        let profile = Self {
            version: PROTOCOL_VERSION,
            target,
            device_endpoint_id,
            device_id: device.id.clone(),
            device_name: device.name.clone(),
            paired_at_ms: device.paired_at_ms,
        };
        profile.validate()?;
        Ok(profile)
    }

    pub(crate) fn validate(&self) -> Result<(), ProfileError> {
        if self.version != PROTOCOL_VERSION || self.paired_at_ms == 0 {
            return Err(ProfileError::Invalid);
        }
        self.target.validate()?;
        validate_identifier(&self.device_endpoint_id)?;
        validate_identifier(&self.device_id)?;
        if self.device_name.trim().is_empty()
            || self.device_name.len() > 128
            || self.device_name.chars().any(char::is_control)
        {
            return Err(ProfileError::Invalid);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct ProfileStore {
    directory: PathBuf,
}

impl ProfileStore {
    pub(crate) fn open(directory: PathBuf) -> Result<Self, ProfileError> {
        ensure_private_directory(&directory).map_err(|_| ProfileError::Unavailable)?;
        Ok(Self { directory })
    }

    pub(crate) fn load_profile(&self) -> Result<Option<HostProfile>, ProfileError> {
        let profile: Option<HostProfile> = read_json(&self.directory.join(PROFILE_FILE))?;
        if let Some(profile) = &profile {
            profile.validate()?;
        }
        Ok(profile)
    }

    pub(crate) fn store_profile(&self, profile: &HostProfile) -> Result<(), ProfileError> {
        profile.validate()?;
        write_json_atomic(&self.directory, PROFILE_FILE, profile)
    }

    pub(crate) fn remove_profile(&self) -> Result<(), ProfileError> {
        remove_file_if_present(&self.directory.join(PROFILE_FILE))
    }

    pub(crate) fn load_pending(&self) -> Result<Option<PendingPairing>, ProfileError> {
        let pending: Option<PendingPairing> = read_json(&self.directory.join(PENDING_FILE))?;
        if let Some(pending) = &pending {
            pending.validate()?;
        }
        Ok(pending)
    }

    pub(crate) fn store_pending(&self, pending: &PendingPairing) -> Result<(), ProfileError> {
        pending.validate()?;
        write_json_atomic(&self.directory, PENDING_FILE, pending)
    }

    pub(crate) fn remove_pending(&self) -> Result<(), ProfileError> {
        remove_file_if_present(&self.directory.join(PENDING_FILE))
    }

    pub(crate) fn begin_access_removal(&self) -> Result<(), ProfileError> {
        write_json_atomic(
            &self.directory,
            ACCESS_REMOVAL_FILE,
            &AccessRemovalMarker {
                version: PROTOCOL_VERSION,
            },
        )
    }

    pub(crate) fn access_removal_pending(&self) -> Result<bool, ProfileError> {
        let marker: Option<AccessRemovalMarker> =
            read_json(&self.directory.join(ACCESS_REMOVAL_FILE))?;
        match marker {
            Some(marker) if marker.version == PROTOCOL_VERSION => Ok(true),
            Some(_) => Err(ProfileError::Invalid),
            None => Ok(false),
        }
    }

    pub(crate) fn finish_access_removal(&self) -> Result<(), ProfileError> {
        remove_file_if_present(&self.directory.join(ACCESS_REMOVAL_FILE))
    }
}

fn validate_identifier(value: &str) -> Result<(), ProfileError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err(ProfileError::Invalid)
    } else {
        Ok(())
    }
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unsafe state directory",
                    ));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(cursor.clone());
                cursor = cursor
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "state directory has no existing parent",
                        )
                    })?
                    .to_path_buf();
            }
            Err(error) => return Err(error),
        }
    }

    if missing.is_empty() {
        return set_private_directory_mode(path);
    }
    for directory in missing.iter().rev() {
        match fs::create_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(directory)?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unsafe state directory",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
        set_private_directory_mode(directory)?;
        let parent = directory.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "state directory has no parent")
        })?;
        sync_directory(parent)?;
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, ProfileError> {
    let file = match open_nofollow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ProfileError::Unavailable),
    };
    let metadata = file.metadata().map_err(|_| ProfileError::Unavailable)?;
    if !is_private_regular_file(&metadata) || metadata.len() as usize > MAX_STATE_BYTES {
        return Err(ProfileError::Invalid);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_STATE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ProfileError::Unavailable)?;
    if bytes.is_empty() || bytes.len() > MAX_STATE_BYTES {
        return Err(ProfileError::Invalid);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| ProfileError::Invalid)
}

fn write_json_atomic<T: Serialize>(
    directory: &Path,
    filename: &str,
    value: &T,
) -> Result<(), ProfileError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ProfileError::Invalid)?;
    if bytes.is_empty() || bytes.len() > MAX_STATE_BYTES {
        return Err(ProfileError::Invalid);
    }
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|_| ProfileError::Unavailable)?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let target = directory.join(filename);
    let temporary = directory.join(format!(".{filename}.{suffix}.tmp"));
    let result = (|| {
        let mut file = create_private(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &target)?;
        sync_directory(directory)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|_| ProfileError::Unavailable)
}

fn remove_file_if_present(path: &Path) -> Result<(), ProfileError> {
    match fs::remove_file(path) {
        Ok(()) => {
            let parent = path.parent().ok_or(ProfileError::Unavailable)?;
            sync_directory(parent).map_err(|_| ProfileError::Unavailable)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ProfileError::Unavailable),
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(unix)]
fn open_nofollow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_nofollow(path: &Path) -> io::Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsafe state file",
        ));
    }
    File::open(path)
}

#[cfg(unix)]
fn create_private(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> io::Result<File> {
    OpenOptions::new().create_new(true).write(true).open(path)
}

#[cfg(unix)]
fn is_private_regular_file(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    metadata.is_file() && metadata.permissions().mode() & 0o077 == 0 && metadata.nlink() == 1
}

#[cfg(not(unix))]
fn is_private_regular_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
}

#[cfg(unix)]
fn set_private_directory_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_mode(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use ygg_companion_protocol::DevicePlatform;

    fn target() -> HostTarget {
        HostTarget {
            host_id: "host-test".into(),
            host_endpoint_id: "endpoint-test".into(),
            relay_urls: vec!["https://relay.example".into()],
            direct_addresses: vec!["127.0.0.1:1234".into()],
        }
    }

    #[test]
    fn profile_round_trip_uses_private_atomic_file() {
        let temp = TempDir::new().unwrap();
        let store = ProfileStore::open(temp.path().join("state")).unwrap();
        let profile = HostProfile {
            version: PROTOCOL_VERSION,
            target: target(),
            device_endpoint_id: "device-endpoint".into(),
            device_id: "device-one".into(),
            device_name: "Phone".into(),
            paired_at_ms: 1,
        };
        store.store_profile(&profile).unwrap();
        let loaded = store.load_profile().unwrap().unwrap();
        assert_eq!(loaded.device_id, "device-one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.directory.join(PROFILE_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
    }

    #[test]
    fn malformed_or_linked_profile_fails_closed() {
        let temp = TempDir::new().unwrap();
        let store = ProfileStore::open(temp.path().join("state")).unwrap();
        fs::write(store.directory.join(PROFILE_FILE), b"{}").unwrap();
        assert!(matches!(store.load_profile(), Err(ProfileError::Invalid)));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            fs::remove_file(store.directory.join(PROFILE_FILE)).unwrap();
            let outside = temp.path().join("outside");
            fs::write(&outside, b"{}").unwrap();
            symlink(outside, store.directory.join(PROFILE_FILE)).unwrap();
            assert!(store.load_profile().is_err());
        }
    }

    #[test]
    fn access_removal_marker_is_private_and_validated() {
        let temp = TempDir::new().unwrap();
        let store = ProfileStore::open(temp.path().join("state")).unwrap();
        assert!(!store.access_removal_pending().unwrap());
        store.begin_access_removal().unwrap();
        assert!(store.access_removal_pending().unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.directory.join(ACCESS_REMOVAL_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        store.finish_access_removal().unwrap();
        assert!(!store.access_removal_pending().unwrap());

        fs::write(
            store.directory.join(ACCESS_REMOVAL_FILE),
            br#"{"version":999}"#,
        )
        .unwrap();
        assert!(matches!(
            store.access_removal_pending(),
            Err(ProfileError::Invalid)
        ));
    }

    #[test]
    fn nested_state_directories_are_private_and_creation_races_fail_closed() {
        let temp = TempDir::new().unwrap();
        let nested = temp.path().join("app/data/companion");
        ProfileStore::open(nested.clone()).unwrap();
        assert!(nested.is_dir());

        let raced = temp.path().join("race/data/companion");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let threads = (0..4)
            .map(|_| {
                let raced = raced.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    ProfileStore::open(raced)
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        assert!(raced.is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};

            for directory in [
                temp.path().join("app"),
                temp.path().join("app/data"),
                nested,
            ] {
                let mode = fs::symlink_metadata(directory)
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o077, 0);
            }

            let outside = temp.path().join("outside");
            fs::create_dir(&outside).unwrap();
            let linked = temp.path().join("linked");
            symlink(&outside, &linked).unwrap();
            assert!(matches!(
                ProfileStore::open(linked),
                Err(ProfileError::Unavailable)
            ));
        }
    }

    #[test]
    fn pending_metadata_rejects_control_characters() {
        assert!(PendingPairing::new(
            "pair-one".into(),
            target(),
            PairingDeviceClaim {
                name: "Bad\nName".into(),
                platform: DevicePlatform::Ios,
                app_version: "0.1.0".into(),
            },
            "amber · birch".into(),
            1,
        )
        .is_err());
    }
}
