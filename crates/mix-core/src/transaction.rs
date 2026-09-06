use crate::fs::{
    atomic_json, atomic_write, private_scoped_dir, read_bounded, remove_durable, safe_child,
    validate_scoped_path, MAX_LOCAL_JSON_BYTES,
};
use crate::{CredentialVault, Error, ErrorCode, Profile, Result, SecretRef};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const JOURNAL_VERSION: u32 = 5;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum JournalPhase {
    Prepared,
    Restored,
    Committed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchJournal {
    version: u32,
    pub id: String,
    pub app: String,
    pub from_profile: Option<String>,
    pub to_profile: String,
    pub created_at: String,
    live_dir: PathBuf,
    files: Vec<FileMutation>,
    rewrites: Vec<FileTokenRewrite>,
    secrets: Vec<SecretMutation>,
    restart_required: bool,
    phase: JournalPhase,
}

pub struct SwitchPlan<'a> {
    pub app: &'a str,
    pub live_dir: &'a Path,
    pub from_name: Option<&'a str>,
    pub from: Option<&'a Profile>,
    pub to_name: &'a str,
    pub to: &'a Profile,
    pub file_overrides: BTreeMap<String, Vec<u8>>,
    pub file_removals: BTreeSet<String>,
    pub file_rewrites: Vec<FileTokenRewrite>,
    pub restart_required: bool,
}

/// A short token update in a client-owned file. The journal stores both sides
/// so an interrupted switch can undo the update without copying a transcript.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileTokenRewrite {
    pub(crate) relative: String,
    pub(crate) offset: u64,
    pub(crate) from: Vec<u8>,
    pub(crate) to: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileMutation {
    relative: String,
    existed: bool,
    target: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SecretMutation {
    relative: String,
    existed: bool,
    target: Option<SecretRef>,
}

impl SwitchJournal {
    pub fn path(root: &Path) -> PathBuf {
        root.join("transactions/active-switch.json")
    }

    pub fn load(root: &Path) -> Result<Option<Self>> {
        let path = Self::path(root);
        let parent = path.parent().ok_or_else(invalid_journal)?;
        validate_scoped_path(parent, &root.join("transactions")).map_err(|_| {
            Error::new(
                ErrorCode::MixSwitchRecoveryFailed,
                "the switch journal path is unsafe",
            )
        })?;
        if !path.exists() {
            return Ok(None);
        }
        let payload = read_bounded(&path, MAX_LOCAL_JSON_BYTES, "cannot read switch journal")?;
        let journal: Self = serde_json::from_slice(&payload).map_err(|_| invalid_journal())?;
        validate_journal(root, &journal)?;
        Ok(Some(journal))
    }

    pub fn prepare(root: &Path, plan: SwitchPlan<'_>, vault: &dyn CredentialVault) -> Result<Self> {
        if Self::path(root).exists() {
            return Err(Error::new(
                ErrorCode::MixSwitchRecoveryRequired,
                "an interrupted switch must be recovered first",
            ));
        }
        let id = Uuid::new_v4().to_string();
        let backup_dir = backup_dir(root, &id);
        private_scoped_dir(&backup_dir, &root.join("backups"))?;

        let rewrite_names = plan
            .file_rewrites
            .iter()
            .map(|rewrite| rewrite.relative.clone())
            .collect::<BTreeSet<_>>();
        let file_names = plan
            .from
            .into_iter()
            .flat_map(|profile| profile.files.keys())
            .chain(plan.to.files.keys())
            .chain(plan.file_overrides.keys())
            .chain(plan.file_removals.iter())
            .chain(plan.file_rewrites.iter().map(|rewrite| &rewrite.relative))
            .filter(|relative| !rewrite_names.contains(*relative))
            .cloned()
            .collect::<BTreeSet<_>>();
        let secret_names = plan
            .from
            .into_iter()
            .flat_map(|profile| profile.secret_files.keys())
            .chain(plan.to.secret_files.keys())
            .cloned()
            .collect::<BTreeSet<_>>();

        let mut backup_references = Vec::new();
        let result = (|| {
            let mut files = Vec::new();
            for relative in file_names {
                let live = safe_child(plan.live_dir, &relative)?;
                let existed = existing_regular_file(&live)?;
                if existed {
                    let payload =
                        read_bounded(&live, 16 * 1024 * 1024, "cannot back up live client file")?;
                    let backup = safe_child(&backup_dir, &format!("files/{relative}"))?;
                    atomic_write(&backup, &payload)?;
                }
                let target = if plan.file_removals.contains(&relative) {
                    if plan.file_overrides.contains_key(&relative)
                        || plan.to.files.contains_key(&relative)
                    {
                        return Err(Error::invalid(
                            "a switch file cannot be both removed and materialized",
                        ));
                    }
                    None
                } else if let Some(payload) = plan.file_overrides.get(&relative) {
                    let target = safe_child(&backup_dir, &format!("targets/{relative}"))?;
                    atomic_write(&target, payload)?;
                    Some(target)
                } else {
                    plan.to.files.get(&relative).cloned()
                };
                files.push(FileMutation {
                    relative: relative.clone(),
                    existed,
                    target,
                });
            }

            let mut secrets = Vec::new();
            for (index, relative) in secret_names.into_iter().enumerate() {
                let live = safe_child(plan.live_dir, &relative)?;
                let existed = existing_regular_file(&live)?;
                if existed {
                    let payload = read_bounded(
                        &live,
                        2 * 1024 * 1024,
                        "cannot back up live credential file",
                    )?;
                    let reference = backup_secret(&id, index);
                    vault.set(&reference, &payload)?;
                    backup_references.push(reference.clone());
                }
                secrets.push(SecretMutation {
                    relative: relative.clone(),
                    existed,
                    target: plan.to.secret_files.get(&relative).cloned(),
                });
            }

            let mut rewrites = Vec::with_capacity(plan.file_rewrites.len());
            let mut rewrite_files = BTreeSet::new();
            for rewrite in &plan.file_rewrites {
                crate::fs::safe_relative(&rewrite.relative)?;
                if rewrite.from.is_empty()
                    || rewrite.from.len() > 256
                    || rewrite.to.is_empty()
                    || rewrite.to.len() > 256
                    || rewrite.from == rewrite.to
                {
                    return Err(Error::invalid("a switch file-token rewrite is invalid"));
                }
                if !rewrite_files.insert(rewrite.relative.clone()) {
                    return Err(Error::invalid(
                        "a switch cannot rewrite more than one token in the same file",
                    ));
                }
                let live = safe_child(plan.live_dir, &rewrite.relative)?;
                let current = read_at(&live, rewrite.offset, rewrite.from.len())?;
                if current != rewrite.from && current != rewrite.to {
                    return Err(Error::new(
                        ErrorCode::MixConflict,
                        format!(
                            "client history changed while preparing the switch: {}",
                            live.display()
                        ),
                    ));
                }
                rewrites.push(rewrite.clone());
            }

            let journal = Self {
                version: JOURNAL_VERSION,
                id,
                app: plan.app.into(),
                from_profile: plan.from_name.map(str::to_owned),
                to_profile: plan.to_name.into(),
                created_at: chrono::Utc::now().to_rfc3339(),
                live_dir: plan.live_dir.to_path_buf(),
                files,
                rewrites,
                secrets,
                restart_required: plan.restart_required,
                phase: JournalPhase::Prepared,
            };
            let path = Self::path(root);
            let parent = path.parent().ok_or_else(invalid_journal)?;
            private_scoped_dir(parent, &root.join("transactions"))?;
            if let Err(error) = atomic_json(&path, &journal) {
                let _ = journal.remove_backup_secrets(vault);
                return Err(error);
            }
            Ok(journal)
        })();
        if result.is_err() {
            for reference in backup_references {
                let _ = vault.delete(&reference);
            }
            let _ = remove_backup_dir(root, &backup_dir);
            let _ = remove_durable(&Self::path(root));
        }
        result
    }

    pub fn backup_dir(&self, root: &Path) -> PathBuf {
        backup_dir(root, &self.id)
    }

    pub fn restart_required(&self) -> bool {
        self.restart_required
    }

    pub fn live_dir(&self) -> &Path {
        &self.live_dir
    }

    pub fn is_committed(&self) -> bool {
        matches!(self.phase, JournalPhase::Committed)
    }

    pub fn is_cleanup_pending(&self) -> bool {
        matches!(self.phase, JournalPhase::Restored | JournalPhase::Committed)
    }

    pub fn apply(&self, vault: &dyn CredentialVault) -> Result<()> {
        self.require_prepared("apply")?;
        for mutation in &self.files {
            let live = safe_child(&self.live_dir, &mutation.relative)?;
            match &mutation.target {
                Some(source) => {
                    let payload =
                        read_bounded(source, 16 * 1024 * 1024, "cannot read account file")?;
                    atomic_write(&live, &payload)?;
                }
                None => remove_durable(&live)?,
            }
        }
        for mutation in &self.secrets {
            let live = safe_child(&self.live_dir, &mutation.relative)?;
            match &mutation.target {
                Some(reference) => {
                    let payload = vault.get(reference)?;
                    atomic_write(&live, &payload)?;
                }
                None => remove_durable(&live)?,
            }
        }
        for rewrite in &self.rewrites {
            apply_rewrite(&self.live_dir, rewrite, true)?;
        }
        Ok(())
    }

    pub fn restore(&self, root: &Path, vault: &dyn CredentialVault) -> Result<()> {
        self.require_prepared("restore")?;
        let backup_dir = self.backup_dir(root);
        let mut errors = Vec::new();
        for rewrite in self.rewrites.iter().rev() {
            let result = apply_rewrite(&self.live_dir, rewrite, false);
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        for mutation in &self.files {
            let result = (|| {
                let live = safe_child(&self.live_dir, &mutation.relative)?;
                if mutation.existed {
                    let backup = safe_child(&backup_dir, &format!("files/{}", mutation.relative))?;
                    let payload =
                        read_bounded(&backup, 16 * 1024 * 1024, "cannot read switch backup")?;
                    atomic_write(&live, &payload)
                } else {
                    remove_durable(&live)
                }
            })();
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        for (index, mutation) in self.secrets.iter().enumerate() {
            let result = (|| {
                let live = safe_child(&self.live_dir, &mutation.relative)?;
                if mutation.existed {
                    let payload = vault.get(&backup_secret(&self.id, index))?;
                    atomic_write(&live, &payload)
                } else {
                    remove_durable(&live)
                }
            })();
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::new(
                ErrorCode::MixSwitchRecoveryFailed,
                "one or more switch components could not be restored",
            )
            .details(BTreeMap::from([("errors", errors)])))
        }
    }

    pub fn commit(mut self, root: &Path, vault: &dyn CredentialVault) -> Result<()> {
        self.require_prepared("commit")?;
        // The live files and Mix config are already durable when this runs.
        // Persisting this phase first prevents cleanup failures from ever
        // being interpreted as permission to restore the old account.
        self.phase = JournalPhase::Committed;
        atomic_json(&Self::path(root), &self)?;
        self.cleanup_committed(root, vault)
    }

    pub fn finish_recovery(self, root: &Path, vault: &dyn CredentialVault) -> Result<()> {
        if self.is_cleanup_pending() {
            return self.cleanup_finalized(root, vault);
        }
        self.restore(root, vault)?;
        self.cleanup_after_restore(root, vault)
    }

    pub fn cleanup_after_restore(mut self, root: &Path, vault: &dyn CredentialVault) -> Result<()> {
        if self.is_cleanup_pending() {
            return Err(Error::new(
                ErrorCode::MixSwitchRecoveryFailed,
                "a finalized switch cannot be marked as restored",
            ));
        }
        self.phase = JournalPhase::Restored;
        atomic_json(&Self::path(root), &self)?;
        self.cleanup_finalized(root, vault)
    }

    pub fn cleanup_committed(&self, root: &Path, vault: &dyn CredentialVault) -> Result<()> {
        if !self.is_committed() {
            return Err(Error::new(
                ErrorCode::MixSwitchRecoveryFailed,
                "an uncommitted switch cannot use committed cleanup",
            ));
        }
        self.cleanup_finalized(root, vault)
    }

    fn cleanup_finalized(&self, root: &Path, vault: &dyn CredentialVault) -> Result<()> {
        let mut errors = self
            .remove_backup_secrets(vault)
            .err()
            .into_iter()
            .collect::<Vec<_>>();
        if let Err(error) = remove_backup_dir(root, &self.backup_dir(root)) {
            errors.push(error);
        }
        if errors.is_empty() {
            if let Err(error) = remove_durable(&Self::path(root)) {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::new(
                ErrorCode::MixSwitchRecoveryFailed,
                "switch state is finalized, but transaction cleanup is still pending",
            )
            .details(serde_json::json!({
                "applied": true,
                "cleanup_pending": true,
                "errors": errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            })))
        }
    }

    fn remove_backup_secrets(&self, vault: &dyn CredentialVault) -> Result<()> {
        let mut errors = Vec::new();
        for (index, mutation) in self.secrets.iter().enumerate() {
            if mutation.existed {
                if let Err(error) = vault.delete(&backup_secret(&self.id, index)) {
                    errors.push(error.to_string());
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::new(
                ErrorCode::MixSwitchRecoveryFailed,
                "one or more transaction credentials could not be removed",
            )
            .details(serde_json::json!({"errors": errors})))
        }
    }

    fn require_prepared(&self, action: &str) -> Result<()> {
        if matches!(self.phase, JournalPhase::Prepared) {
            Ok(())
        } else {
            Err(Error::new(
                ErrorCode::MixSwitchRecoveryFailed,
                format!("a finalized switch cannot {action}"),
            ))
        }
    }
}

fn validate_journal(root: &Path, journal: &SwitchJournal) -> Result<()> {
    if !root.is_absolute() || !journal.live_dir.is_absolute() {
        return Err(Error::new(
            ErrorCode::MixSwitchRecoveryFailed,
            "the interrupted switch journal contains an unsafe path",
        ));
    }
    let uuid_is_valid =
        Uuid::parse_str(&journal.id).is_ok_and(|value| value.to_string() == journal.id);
    let timestamp_is_valid = chrono::DateTime::parse_from_rfc3339(&journal.created_at).is_ok();
    if journal.version != JOURNAL_VERSION
        || !uuid_is_valid
        || !timestamp_is_valid
        || !safe_identifier(&journal.app)
        || !safe_identifier(&journal.to_profile)
        || journal
            .from_profile
            .as_deref()
            .is_some_and(|value| !safe_identifier(value))
    {
        return Err(invalid_journal());
    }
    validate_scoped_path(&journal.backup_dir(root), &root.join("backups")).map_err(|_| {
        Error::new(
            ErrorCode::MixSwitchRecoveryFailed,
            "the interrupted switch backup path is unsafe",
        )
    })?;
    if journal.files.len() > 1024
        || journal.secrets.len() > 1024
        || journal.rewrites.len() > 100_000
    {
        return Err(invalid_journal());
    }
    let mut file_names = BTreeSet::new();
    for mutation in &journal.files {
        crate::fs::safe_relative(&mutation.relative).map_err(|_| invalid_journal())?;
        if !file_names.insert(&mutation.relative) {
            return Err(invalid_journal());
        }
        if let Some(path) = &mutation.target {
            if !path.is_absolute() {
                return Err(Error::new(
                    ErrorCode::MixSwitchRecoveryFailed,
                    "the interrupted switch target is not absolute",
                ));
            }
        }
    }
    let mut secret_names = BTreeSet::new();
    for mutation in &journal.secrets {
        crate::fs::safe_relative(&mutation.relative).map_err(|_| invalid_journal())?;
        if !secret_names.insert(&mutation.relative) {
            return Err(invalid_journal());
        }
    }
    let mut rewrite_files = BTreeSet::new();
    for rewrite in &journal.rewrites {
        crate::fs::safe_relative(&rewrite.relative).map_err(|_| invalid_journal())?;
        if rewrite.from.is_empty()
            || rewrite.from.len() > 256
            || rewrite.to.is_empty()
            || rewrite.to.len() > 256
            || rewrite.from == rewrite.to
            || !rewrite_files.insert(rewrite.relative.clone())
        {
            return Err(invalid_journal());
        }
    }
    Ok(())
}

fn read_at(path: &Path, offset: u64, length: usize) -> Result<Vec<u8>> {
    let mut file = std::fs::OpenOptions::new();
    file.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        file.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = file
        .open(path)
        .map_err(|error| Error::io("cannot read client history", error))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .map_err(|error| Error::io("cannot seek client history", error))?;
    let mut bytes = vec![0; length];
    std::io::Read::read_exact(&mut file, &mut bytes)
        .map_err(|error| Error::io("cannot read client history", error))?;
    Ok(bytes)
}

fn apply_rewrite(root: &Path, rewrite: &FileTokenRewrite, forward: bool) -> Result<()> {
    let path = safe_child(root, &rewrite.relative)?;
    let (expected, replacement) = if forward {
        (&rewrite.from, &rewrite.to)
    } else {
        (&rewrite.to, &rewrite.from)
    };
    if read_at(&path, rewrite.offset, replacement.len()).is_ok_and(|value| value == *replacement) {
        return Ok(());
    }
    if read_at(&path, rewrite.offset, expected.len())? != *expected {
        return Err(Error::new(
            ErrorCode::MixSwitchRecoveryFailed,
            format!("client history has an unexpected value: {}", path.display()),
        ));
    }
    if expected.len() != replacement.len() {
        return replace_token_atomically(&path, rewrite.offset, expected, replacement);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| Error::io("cannot update client history", error))?;
    file.seek(std::io::SeekFrom::Start(rewrite.offset))
        .map_err(|error| Error::io("cannot seek client history", error))?;
    std::io::Write::write_all(&mut file, replacement)
        .map_err(|error| Error::io("cannot update client history", error))?;
    file.sync_all()
        .map_err(|error| Error::io("cannot flush client history", error))?;
    Ok(())
}

fn replace_token_atomically(
    path: &Path,
    offset: u64,
    expected: &[u8],
    replacement: &[u8],
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::invalid("client history has no parent directory"))?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut source = options
        .open(path)
        .map_err(|error| Error::io("cannot read client history", error))?;
    if !source
        .metadata()
        .map_err(|error| Error::io("cannot inspect client history", error))?
        .is_file()
    {
        return Err(Error::invalid("client history is not a regular file"));
    }

    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| Error::io("cannot create client history update", error))?;
    let copied = std::io::copy(&mut Read::by_ref(&mut source).take(offset), &mut temporary)
        .map_err(|error| Error::io("cannot copy client history", error))?;
    if copied != offset {
        return Err(Error::new(
            ErrorCode::MixSwitchRecoveryFailed,
            "client history ended before the Provider token",
        ));
    }
    let mut current = vec![0; expected.len()];
    source
        .read_exact(&mut current)
        .map_err(|error| Error::io("cannot read client history Provider token", error))?;
    if current != expected {
        return Err(Error::new(
            ErrorCode::MixSwitchRecoveryFailed,
            "client history changed during the Provider update",
        ));
    }
    temporary
        .write_all(replacement)
        .and_then(|()| std::io::copy(&mut source, &mut temporary).map(|_| ()))
        .map_err(|error| Error::io("cannot write client history update", error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| Error::io("cannot flush client history update", error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| Error::io("cannot protect client history update", error))?;
    }
    temporary
        .persist(path)
        .map_err(|error| Error::io("cannot commit client history update", error.error))?;
    crate::fs::sync_directory(parent)
}

fn safe_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    value.len() <= 64
        && first.is_ascii_alphanumeric()
        && characters.all(|value| value.is_ascii_alphanumeric() || matches!(value, '.' | '_' | '-'))
}

fn invalid_journal() -> Error {
    Error::new(
        ErrorCode::MixSwitchRecoveryFailed,
        "the interrupted switch journal is invalid",
    )
}

fn backup_secret(id: &str, index: usize) -> SecretRef {
    SecretRef {
        service: "com.mix.transaction-backup".into(),
        account: format!("{id}/{index}"),
    }
}

fn backup_dir(root: &Path, id: &str) -> PathBuf {
    root.join("backups").join(id)
}

fn remove_backup_dir(root: &Path, path: &Path) -> Result<()> {
    validate_scoped_path(path, &root.join("backups")).map_err(|_| {
        Error::new(
            ErrorCode::MixSwitchRecoveryFailed,
            "switch backup path is unsafe",
        )
    })?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(Error::new(
            ErrorCode::MixSwitchRecoveryFailed,
            "switch backup is not a regular directory",
        )),
        Ok(_) => {
            std::fs::remove_dir_all(path)
                .map_err(|error| Error::io("cannot remove switch backup", error))?;
            if let Some(parent) = path.parent() {
                crate::fs::sync_directory(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io("cannot inspect switch backup", error)),
    }
}

fn existing_regular_file(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(Error::invalid("a managed live file is a symbolic link"))
        }
        Ok(metadata) if !metadata.is_file() => {
            Err(Error::invalid("a managed live path is not a regular file"))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::io("cannot inspect live client file", error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::MemoryVault;
    use std::fs;

    #[test]
    fn variable_width_token_rewrites_are_idempotent_for_prefix_names() {
        for (from, to) in [
            (b"\"openai\"".as_slice(), b"\"open\"".as_slice()),
            (b"\"open\"".as_slice(), b"\"openai\"".as_slice()),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("history.jsonl");
            let mut original = b"{\"model_provider\":".to_vec();
            let offset = original.len() as u64;
            original.extend_from_slice(from);
            original.extend_from_slice(b"}\nbody\n");
            fs::write(&path, &original).unwrap();
            let rewrite = FileTokenRewrite {
                relative: "history.jsonl".into(),
                offset,
                from: from.to_vec(),
                to: to.to_vec(),
            };

            apply_rewrite(root.path(), &rewrite, true).unwrap();
            let switched = fs::read(&path).unwrap();
            apply_rewrite(root.path(), &rewrite, true).unwrap();
            assert_eq!(fs::read(&path).unwrap(), switched);

            apply_rewrite(root.path(), &rewrite, false).unwrap();
            apply_rewrite(root.path(), &rewrite, false).unwrap();
            assert_eq!(fs::read(&path).unwrap(), original);
        }
    }

    #[test]
    fn restore_returns_every_live_file_to_its_previous_state() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("config.toml"), b"old").unwrap();
        fs::write(live.join("auth.json"), b"old-secret").unwrap();
        let source = root.path().join("new.toml");
        fs::write(&source, b"new").unwrap();
        let vault = MemoryVault::default();
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "new".into(),
        };
        vault.set(&reference, b"new-secret").unwrap();
        let target = Profile {
            label: "new".into(),
            files: BTreeMap::from([("config.toml".into(), source)]),
            secret_files: BTreeMap::from([("auth.json".into(), reference)]),
            ..Profile::default()
        };
        let journal = SwitchJournal::prepare(
            root.path(),
            SwitchPlan {
                app: "codex",
                live_dir: &live,
                from_name: None,
                from: None,
                to_name: "new",
                to: &target,
                file_overrides: BTreeMap::new(),
                file_removals: BTreeSet::new(),
                file_rewrites: Vec::new(),
                restart_required: false,
            },
            &vault,
        )
        .unwrap();
        journal.apply(&vault).unwrap();
        assert_eq!(fs::read(live.join("auth.json")).unwrap(), b"new-secret");
        journal.restore(root.path(), &vault).unwrap();
        assert_eq!(fs::read(live.join("config.toml")).unwrap(), b"old");
        assert_eq!(fs::read(live.join("auth.json")).unwrap(), b"old-secret");
    }

    #[test]
    fn failed_credential_backup_leaves_no_transaction_or_backup_directory() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("auth.json"), b"old-secret").unwrap();
        let vault = MemoryVault::default();
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "target".into(),
        };
        vault.set(&reference, b"new-secret").unwrap();
        vault.set_write_failure(true);
        let target = Profile {
            label: "target".into(),
            secret_files: BTreeMap::from([("auth.json".into(), reference)]),
            ..Profile::default()
        };

        let error = SwitchJournal::prepare(
            root.path(),
            SwitchPlan {
                app: "codex",
                live_dir: &live,
                from_name: None,
                from: None,
                to_name: "target",
                to: &target,
                file_overrides: BTreeMap::new(),
                file_removals: BTreeSet::new(),
                file_rewrites: Vec::new(),
                restart_required: false,
            },
            &vault,
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::MixCredentialUnavailable);
        assert_eq!(fs::read(live.join("auth.json")).unwrap(), b"old-secret");
        assert!(!SwitchJournal::path(root.path()).exists());
        assert_eq!(
            fs::read_dir(root.path().join("backups")).unwrap().count(),
            0
        );
    }

    #[test]
    fn partial_projection_is_restored_after_a_credential_read_failure() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("config.toml"), b"old-config").unwrap();
        fs::write(live.join("auth.json"), b"old-secret").unwrap();
        let source = root.path().join("target.toml");
        fs::write(&source, b"new-config").unwrap();
        let vault = MemoryVault::default();
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "target".into(),
        };
        vault.set(&reference, b"new-secret").unwrap();
        let target = Profile {
            label: "target".into(),
            files: BTreeMap::from([("config.toml".into(), source)]),
            secret_files: BTreeMap::from([("auth.json".into(), reference)]),
            ..Profile::default()
        };
        let journal = SwitchJournal::prepare(
            root.path(),
            SwitchPlan {
                app: "codex",
                live_dir: &live,
                from_name: None,
                from: None,
                to_name: "target",
                to: &target,
                file_overrides: BTreeMap::new(),
                file_removals: BTreeSet::new(),
                file_rewrites: Vec::new(),
                restart_required: false,
            },
            &vault,
        )
        .unwrap();

        vault.set_read_failure(true);
        let error = journal.apply(&vault).unwrap_err();
        assert_eq!(error.code, ErrorCode::MixCredentialUnavailable);
        assert_eq!(fs::read(live.join("config.toml")).unwrap(), b"new-config");
        assert_eq!(fs::read(live.join("auth.json")).unwrap(), b"old-secret");

        vault.set_read_failure(false);
        journal.restore(root.path(), &vault).unwrap();
        assert_eq!(fs::read(live.join("config.toml")).unwrap(), b"old-config");
        assert_eq!(fs::read(live.join("auth.json")).unwrap(), b"old-secret");
    }

    #[test]
    fn committed_switch_survives_credential_cleanup_failure() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("auth.json"), b"old-secret").unwrap();
        let vault = MemoryVault::default();
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "target".into(),
        };
        vault.set(&reference, b"new-secret").unwrap();
        let target = Profile {
            label: "target".into(),
            secret_files: BTreeMap::from([("auth.json".into(), reference)]),
            ..Profile::default()
        };
        let journal = SwitchJournal::prepare(
            root.path(),
            SwitchPlan {
                app: "codex",
                live_dir: &live,
                from_name: None,
                from: None,
                to_name: "target",
                to: &target,
                file_overrides: BTreeMap::new(),
                file_removals: BTreeSet::new(),
                file_rewrites: Vec::new(),
                restart_required: false,
            },
            &vault,
        )
        .unwrap();
        journal.apply(&vault).unwrap();

        vault.set_delete_failure(true);
        let error = journal.commit(root.path(), &vault).unwrap_err();
        assert_eq!(error.code, ErrorCode::MixSwitchRecoveryFailed);
        assert_eq!(
            error
                .details
                .get("applied")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(fs::read(live.join("auth.json")).unwrap(), b"new-secret");
        let pending = SwitchJournal::load(root.path()).unwrap().unwrap();
        assert!(pending.is_committed());

        vault.set_delete_failure(false);
        pending.finish_recovery(root.path(), &vault).unwrap();
        assert_eq!(fs::read(live.join("auth.json")).unwrap(), b"new-secret");
        assert!(!SwitchJournal::path(root.path()).exists());
    }

    #[test]
    fn prepared_switch_uses_its_staged_override_instead_of_a_mutable_profile_file() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("config.toml"), b"old").unwrap();
        let source = root.path().join("profile.toml");
        fs::write(&source, b"profile-before").unwrap();
        let vault = MemoryVault::default();
        let target = Profile {
            label: "new".into(),
            files: BTreeMap::from([("config.toml".into(), source.clone())]),
            ..Profile::default()
        };
        let journal = SwitchJournal::prepare(
            root.path(),
            SwitchPlan {
                app: "codex",
                live_dir: &live,
                from_name: None,
                from: None,
                to_name: "new",
                to: &target,
                file_overrides: BTreeMap::from([(
                    "config.toml".into(),
                    b"effective-at-prepare".to_vec(),
                )]),
                file_removals: BTreeSet::new(),
                file_rewrites: Vec::new(),
                restart_required: false,
            },
            &vault,
        )
        .unwrap();
        fs::write(source, b"profile-after").unwrap();

        journal.apply(&vault).unwrap();

        assert_eq!(
            fs::read(live.join("config.toml")).unwrap(),
            b"effective-at-prepare"
        );
    }

    #[test]
    fn committed_journal_cleanup_never_restores_the_old_state() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("config.toml"), b"old").unwrap();
        let source = root.path().join("new.toml");
        std::fs::write(&source, b"new").unwrap();
        let vault = MemoryVault::default();
        let target = Profile {
            label: "new".into(),
            files: BTreeMap::from([("config.toml".into(), source)]),
            ..Profile::default()
        };
        let mut journal = SwitchJournal::prepare(
            root.path(),
            SwitchPlan {
                app: "codex",
                live_dir: &live,
                from_name: Some("old"),
                from: None,
                to_name: "new",
                to: &target,
                file_overrides: BTreeMap::new(),
                file_removals: BTreeSet::new(),
                file_rewrites: Vec::new(),
                restart_required: false,
            },
            &vault,
        )
        .unwrap();
        journal.apply(&vault).unwrap();
        journal.phase = JournalPhase::Committed;
        atomic_json(&SwitchJournal::path(root.path()), &journal).unwrap();
        assert!(journal.restore(root.path(), &vault).is_err());
        journal.finish_recovery(root.path(), &vault).unwrap();
        assert_eq!(std::fs::read(live.join("config.toml")).unwrap(), b"new");
        assert!(!SwitchJournal::path(root.path()).exists());
    }

    #[test]
    fn malformed_short_journal_id_is_rejected_without_panicking() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        fs::create_dir_all(&live).unwrap();
        let vault = MemoryVault::default();
        let target = Profile {
            label: "new".into(),
            ..Profile::default()
        };
        let mut journal = SwitchJournal::prepare(
            root.path(),
            SwitchPlan {
                app: "codex",
                live_dir: &live,
                from_name: None,
                from: None,
                to_name: "new",
                to: &target,
                file_overrides: BTreeMap::new(),
                file_removals: BTreeSet::new(),
                file_rewrites: Vec::new(),
                restart_required: false,
            },
            &vault,
        )
        .unwrap();
        journal.id = "x".into();
        atomic_json(&SwitchJournal::path(root.path()), &journal).unwrap();

        let error = SwitchJournal::load(root.path()).unwrap_err();
        assert_eq!(error.code, ErrorCode::MixSwitchRecoveryFailed);
    }

    #[test]
    fn journal_with_unknown_fields_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        fs::create_dir_all(&live).unwrap();
        let vault = MemoryVault::default();
        let target = Profile {
            label: "new".into(),
            ..Profile::default()
        };
        SwitchJournal::prepare(
            root.path(),
            SwitchPlan {
                app: "codex",
                live_dir: &live,
                from_name: None,
                from: None,
                to_name: "new",
                to: &target,
                file_overrides: BTreeMap::new(),
                file_removals: BTreeSet::new(),
                file_rewrites: Vec::new(),
                restart_required: false,
            },
            &vault,
        )
        .unwrap();
        let path = SwitchJournal::path(root.path());
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        document["unexpected"] = serde_json::Value::Bool(true);
        atomic_json(&path, &document).unwrap();

        let error = SwitchJournal::load(root.path()).unwrap_err();

        assert_eq!(error.code, ErrorCode::MixSwitchRecoveryFailed);
    }

    #[test]
    fn restored_journal_cleanup_never_repeats_the_restore() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("config.toml"), b"old").unwrap();
        let source = root.path().join("new.toml");
        std::fs::write(&source, b"new").unwrap();
        let vault = MemoryVault::default();
        let target = Profile {
            label: "new".into(),
            files: BTreeMap::from([("config.toml".into(), source)]),
            ..Profile::default()
        };
        let mut journal = SwitchJournal::prepare(
            root.path(),
            SwitchPlan {
                app: "codex",
                live_dir: &live,
                from_name: Some("old"),
                from: None,
                to_name: "new",
                to: &target,
                file_overrides: BTreeMap::new(),
                file_removals: BTreeSet::new(),
                file_rewrites: Vec::new(),
                restart_required: false,
            },
            &vault,
        )
        .unwrap();
        journal.apply(&vault).unwrap();
        journal.restore(root.path(), &vault).unwrap();
        journal.phase = JournalPhase::Restored;
        atomic_json(&SwitchJournal::path(root.path()), &journal).unwrap();
        std::fs::write(live.join("config.toml"), b"after-restore").unwrap();
        assert!(journal.apply(&vault).is_err());
        journal.finish_recovery(root.path(), &vault).unwrap();
        assert_eq!(
            std::fs::read(live.join("config.toml")).unwrap(),
            b"after-restore"
        );
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_a_symlinked_backup_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        let vault = MemoryVault::default();
        let target = Profile {
            label: "target".into(),
            ..Profile::default()
        };
        let journal = SwitchJournal::prepare(
            root.path(),
            SwitchPlan {
                app: "codex",
                live_dir: &live,
                from_name: None,
                from: None,
                to_name: "target",
                to: &target,
                file_overrides: BTreeMap::new(),
                file_removals: BTreeSet::new(),
                file_rewrites: Vec::new(),
                restart_required: false,
            },
            &vault,
        )
        .unwrap();
        let backup_dir = journal.backup_dir(root.path());
        std::fs::remove_dir(&backup_dir).unwrap();
        symlink(outside.path(), &backup_dir).unwrap();
        assert!(SwitchJournal::load(root.path()).is_err());
    }
}
