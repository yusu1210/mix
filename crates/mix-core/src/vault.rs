use crate::fs::{atomic_write, private_dir, read_bounded, remove_durable, MAX_CREDENTIAL_BYTES};
use crate::{Error, ErrorCode, Result, SecretRef};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub trait CredentialVault: Send + Sync {
    fn capability(&self) -> VaultCapability;
    fn contains(&self, reference: &SecretRef) -> Result<bool>;
    fn get(&self, reference: &SecretRef) -> Result<Vec<u8>>;
    fn set(&self, reference: &SecretRef, value: &[u8]) -> Result<()>;
    fn delete(&self, reference: &SecretRef) -> Result<()>;
}

#[derive(Clone, Debug, Serialize)]
pub struct VaultCapability {
    pub supported: bool,
    pub available: bool,
    pub backend: Option<&'static str>,
    pub kind: Option<&'static str>,
    pub reason: Option<&'static str>,
}

fn validate_value(value: &[u8]) -> Result<()> {
    if value.len() > MAX_CREDENTIAL_BYTES {
        return Err(Error::new(
            ErrorCode::MixCredentialTooLarge,
            "credential exceeds the 2 MiB safety limit",
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct LocalFileVault {
    directory: PathBuf,
}

impl LocalFileVault {
    fn new(root: &Path) -> Result<Self> {
        if !root.is_absolute() {
            return Err(Error::invalid("credential storage root must be absolute"));
        }
        Ok(Self {
            directory: root.join("credentials"),
        })
    }

    fn path(&self, reference: &SecretRef) -> PathBuf {
        let mut digest = Sha256::new();
        for value in [&reference.service, &reference.account] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        self.directory
            .join(format!("{:x}.secret", digest.finalize()))
    }

    fn validate_directory(&self) -> Result<bool> {
        let metadata = match std::fs::symlink_metadata(&self.directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(Error::io("cannot inspect credential storage", error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::new(
                ErrorCode::MixCredentialUnavailable,
                "credential storage is not a regular directory",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(Error::new(
                    ErrorCode::MixCredentialUnavailable,
                    "credential storage permissions are not private",
                ));
            }
        }
        Ok(true)
    }

    fn validate_file(&self, path: &Path) -> Result<bool> {
        if !self.validate_directory()? {
            return Ok(false);
        }
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(Error::io("cannot inspect stored credential", error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(Error::new(
                ErrorCode::MixCredentialUnavailable,
                "stored credential is not a regular file",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(Error::new(
                    ErrorCode::MixCredentialUnavailable,
                    "stored credential permissions are not private",
                ));
            }
        }
        Ok(true)
    }
}

impl CredentialVault for LocalFileVault {
    fn capability(&self) -> VaultCapability {
        let available = self.validate_directory().is_ok();
        VaultCapability {
            supported: true,
            available,
            backend: Some("Mix private files"),
            kind: Some("local-files"),
            reason: (!available).then_some("unsafe-local-credential-directory"),
        }
    }

    fn contains(&self, reference: &SecretRef) -> Result<bool> {
        self.validate_file(&self.path(reference))
    }

    fn get(&self, reference: &SecretRef) -> Result<Vec<u8>> {
        let path = self.path(reference);
        if !self.validate_file(&path)? {
            return Err(Error::new(
                ErrorCode::MixCredentialNotFound,
                "stored credential was not found",
            ));
        }
        let value = read_bounded(&path, MAX_CREDENTIAL_BYTES as u64, "cannot read credential")?;
        validate_value(&value)?;
        Ok(value)
    }

    fn set(&self, reference: &SecretRef, value: &[u8]) -> Result<()> {
        validate_value(value)?;
        if !self.validate_directory()? {
            private_dir(&self.directory)?;
        }
        atomic_write(&self.path(reference), value)
    }

    fn delete(&self, reference: &SecretRef) -> Result<()> {
        if !self.validate_file(&self.path(reference))? {
            return Ok(());
        }
        remove_durable(&self.path(reference))
    }
}

pub fn local_vault(root: &Path) -> Result<Arc<dyn CredentialVault>> {
    Ok(Arc::new(LocalFileVault::new(root)?))
}

#[cfg(test)]
#[derive(Default)]
pub struct MemoryVault {
    values: parking_lot::Mutex<std::collections::BTreeMap<(String, String), Vec<u8>>>,
    fail_reads: std::sync::atomic::AtomicBool,
    fail_read_on: std::sync::atomic::AtomicUsize,
    fail_writes: std::sync::atomic::AtomicBool,
    fail_deletes: std::sync::atomic::AtomicBool,
    reads: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl MemoryVault {
    pub fn set_read_failure(&self, value: bool) {
        self.fail_reads
            .store(value, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn set_read_failure_on(&self, read: usize) {
        self.fail_read_on
            .store(read, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn set_write_failure(&self, value: bool) {
        self.fail_writes
            .store(value, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn set_delete_failure(&self, value: bool) {
        self.fail_deletes
            .store(value, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_empty(&self) -> bool {
        self.values.lock().is_empty()
    }

    pub fn read_count(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
impl CredentialVault for MemoryVault {
    fn capability(&self) -> VaultCapability {
        VaultCapability {
            supported: true,
            available: true,
            backend: Some("memory"),
            kind: Some("test"),
            reason: None,
        }
    }

    fn contains(&self, reference: &SecretRef) -> Result<bool> {
        Ok(self
            .values
            .lock()
            .contains_key(&(reference.service.clone(), reference.account.clone())))
    }

    fn get(&self, reference: &SecretRef) -> Result<Vec<u8>> {
        let read = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if self.fail_reads.load(std::sync::atomic::Ordering::SeqCst)
            || self.fail_read_on.load(std::sync::atomic::Ordering::SeqCst) == read
        {
            return Err(Error::new(
                ErrorCode::MixCredentialUnavailable,
                "injected credential read failure",
            ));
        }
        self.values
            .lock()
            .get(&(reference.service.clone(), reference.account.clone()))
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::MixCredentialNotFound,
                    "test credential not found",
                )
            })
    }

    fn set(&self, reference: &SecretRef, value: &[u8]) -> Result<()> {
        validate_value(value)?;
        if self.fail_writes.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::new(
                ErrorCode::MixCredentialUnavailable,
                "injected credential write failure",
            ));
        }
        self.values.lock().insert(
            (reference.service.clone(), reference.account.clone()),
            value.to_vec(),
        );
        Ok(())
    }

    fn delete(&self, reference: &SecretRef) -> Result<()> {
        if self.fail_deletes.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::new(
                ErrorCode::MixCredentialUnavailable,
                "injected credential deletion failure",
            ));
        }
        self.values
            .lock()
            .remove(&(reference.service.clone(), reference.account.clone()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(service: &str, account: &str) -> SecretRef {
        SecretRef {
            service: service.into(),
            account: account.into(),
        }
    }

    #[test]
    fn local_files_round_trip_without_exposing_reference_names() {
        let root = tempfile::tempdir().unwrap();
        let vault = LocalFileVault::new(root.path()).unwrap();
        let credential = reference("com.mix.client-auth", "codex/account-1/id");

        assert!(!vault.contains(&credential).unwrap());
        vault.set(&credential, b"secret").unwrap();
        assert_eq!(vault.get(&credential).unwrap(), b"secret");
        assert!(vault.contains(&credential).unwrap());
        assert!(!vault
            .path(&credential)
            .to_string_lossy()
            .contains("account-1"));

        vault.delete(&credential).unwrap();
        assert!(!vault.contains(&credential).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn local_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let vault = LocalFileVault::new(root.path()).unwrap();
        let credential = reference("service", "account");
        vault.set(&credential, b"secret").unwrap();

        assert_eq!(
            std::fs::metadata(&vault.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(vault.path(&credential))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_files_reject_links_and_non_private_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        let vault = LocalFileVault::new(root.path()).unwrap();
        let linked = reference("service", "linked");
        private_dir(&vault.directory).unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, b"secret").unwrap();
        symlink(&outside, vault.path(&linked)).unwrap();
        assert!(vault.get(&linked).is_err());

        let exposed = reference("service", "exposed");
        vault.set(&exposed, b"secret").unwrap();
        std::fs::set_permissions(vault.path(&exposed), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(vault.get(&exposed).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn local_files_reject_a_replaced_credential_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let vault = LocalFileVault::new(root.path()).unwrap();
        let credential = reference("service", "account");
        let outside_file = outside
            .path()
            .join(vault.path(&credential).file_name().unwrap());
        std::fs::write(&outside_file, b"must-stay").unwrap();
        symlink(outside.path(), &vault.directory).unwrap();

        assert!(!vault.capability().available);
        assert_eq!(
            vault.contains(&credential).unwrap_err().code,
            ErrorCode::MixCredentialUnavailable
        );
        assert_eq!(
            vault.get(&credential).unwrap_err().code,
            ErrorCode::MixCredentialUnavailable
        );
        assert_eq!(
            vault.set(&credential, b"replacement").unwrap_err().code,
            ErrorCode::MixCredentialUnavailable
        );
        assert_eq!(
            vault.delete(&credential).unwrap_err().code,
            ErrorCode::MixCredentialUnavailable
        );
        assert_eq!(std::fs::read(outside_file).unwrap(), b"must-stay");
    }

    #[cfg(unix)]
    #[test]
    fn local_files_reject_an_exposed_credential_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let vault = LocalFileVault::new(root.path()).unwrap();
        std::fs::create_dir(&vault.directory).unwrap();
        std::fs::set_permissions(&vault.directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        let credential = reference("service", "account");

        assert!(!vault.capability().available);
        assert_eq!(
            vault.set(&credential, b"secret").unwrap_err().code,
            ErrorCode::MixCredentialUnavailable
        );
        assert!(!vault.path(&credential).exists());
    }
}
