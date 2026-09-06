use crate::fs::{
    atomic_json, expand_home, open_lock, private_dir, read_bounded, MAX_LOCAL_JSON_BYTES,
};
use crate::model::is_environment_variable_name;
use crate::{Config, Error, ErrorCode, Result, CONFIG_VERSION};
use fs2::FileExt;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = expand_home(path.as_ref())?;
        if !path.is_absolute() {
            return Err(Error::new(
                ErrorCode::MixConfigInvalid,
                "the Mix config path must be absolute",
            ));
        }
        Ok(Self { path })
    }

    pub fn default_path() -> Result<PathBuf> {
        let home = dirs::home_dir().ok_or_else(|| {
            Error::new(
                ErrorCode::MixLocalFailure,
                "the user home directory is unavailable",
            )
        })?;
        Ok(home.join(".mix/config.json"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn root(&self) -> Result<&Path> {
        self.path
            .parent()
            .ok_or_else(|| Error::invalid("config path has no parent"))
    }

    fn lock_path(&self) -> Result<PathBuf> {
        Ok(self
            .path
            .parent()
            .ok_or_else(|| Error::invalid("config path has no parent"))?
            .join(".mix-config.lock"))
    }

    fn load_unlocked(&self) -> Result<Config> {
        if !self.path.exists() {
            let root = self.root()?.to_path_buf();
            return Ok(Config::empty(root));
        }
        let payload = read_bounded(&self.path, MAX_LOCAL_JSON_BYTES, "cannot read Mix config")?;
        let config: Config = serde_json::from_slice(&payload)?;
        validate(&config)?;
        if config.root != self.root()? {
            return Err(Error::new(
                ErrorCode::MixConfigInvalid,
                "Mix root must be the config directory",
            ));
        }
        Ok(config)
    }

    pub fn load(&self) -> Result<Config> {
        let lock = open_lock(&self.lock_path()?)?;
        lock.lock_shared()
            .map_err(|error| Error::io("cannot lock Mix config", error))?;
        self.load_unlocked()
    }

    pub fn mutate<T>(&self, operation: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
        let lock = open_lock(&self.lock_path()?)?;
        lock.lock_exclusive()
            .map_err(|error| Error::io("cannot lock Mix config", error))?;
        let mut config = self.load_unlocked()?;
        let result = operation(&mut config)?;
        let revision = uuid::Uuid::new_v4().to_string();
        config.version = CONFIG_VERSION;
        config.meta.updated_at = Some(chrono::Utc::now().to_rfc3339());
        config.meta.revision = Some(revision.clone());
        validate(&config)?;
        match atomic_json(&self.path, &config) {
            Ok(()) => Ok(result),
            Err(_) if self.mutation_was_persisted(&revision) => Ok(result),
            Err(error) => Err(error),
        }
    }

    pub fn initialize(&self) -> Result<Config> {
        let parent = self.root()?;
        private_dir(parent)?;
        if self.path.exists() {
            return self.load();
        }
        self.mutate(|config| Ok(config.clone()))
    }

    fn mutation_was_persisted(&self, revision: &str) -> bool {
        self.load_unlocked()
            .ok()
            .and_then(|config| config.meta.revision)
            .as_deref()
            == Some(revision)
    }
}

fn validate_identifier(value: &str, label: &str) -> Result<()> {
    let mut chars = value.chars();
    let first = chars
        .next()
        .ok_or_else(|| Error::invalid(format!("{label} is empty")))?;
    if value.len() > 64
        || !first.is_ascii_alphanumeric()
        || chars.any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')))
    {
        return Err(Error::new(
            ErrorCode::MixConfigInvalid,
            format!("{label} contains unsupported characters: {value}"),
        ));
    }
    Ok(())
}

pub fn validate(config: &Config) -> Result<()> {
    if config.version != CONFIG_VERSION {
        return Err(Error::new(
            ErrorCode::MixConfigInvalid,
            format!(
                "Mix config schema {} is unsupported; expected {}",
                config.version, CONFIG_VERSION
            ),
        ));
    }
    if !config.root.is_absolute() {
        return Err(Error::new(
            ErrorCode::MixConfigInvalid,
            "Mix root must be absolute",
        ));
    }
    if config.meta.revision.as_deref().is_some_and(|revision| {
        !uuid::Uuid::parse_str(revision).is_ok_and(|value| value.to_string() == revision)
    }) {
        return Err(Error::new(
            ErrorCode::MixConfigInvalid,
            "Mix config revision is invalid",
        ));
    }
    let mut configured_adapters = std::collections::BTreeSet::new();
    for (name, app) in &config.apps {
        validate_identifier(name, "client name")?;
        if !configured_adapters.insert(app.adapter) {
            return Err(Error::new(
                ErrorCode::MixConfigInvalid,
                format!(
                    "client type {} is configured more than once",
                    app.adapter.as_str()
                ),
            ));
        }
        if !app.live_dir.is_absolute() {
            return Err(Error::new(
                ErrorCode::MixConfigInvalid,
                format!("client {name} live directory must be absolute"),
            ));
        }
        if let Some(active) = &app.active_profile {
            if !app.profiles.contains_key(active) {
                return Err(Error::new(
                    ErrorCode::MixConfigInvalid,
                    format!("client {name} selects unknown account {active}"),
                ));
            }
        }
        for (profile_name, profile) in &app.profiles {
            validate_identifier(profile_name, "account name")?;
            if !uuid::Uuid::parse_str(&profile.id)
                .is_ok_and(|value| value.to_string() == profile.id)
            {
                return Err(Error::new(
                    ErrorCode::MixConfigInvalid,
                    format!("account {name}/{profile_name} has an invalid immutable identity"),
                ));
            }
            if invalid_display_text(&profile.label, 120) {
                return Err(Error::new(
                    ErrorCode::MixConfigInvalid,
                    format!("account {name}/{profile_name} has an invalid label"),
                ));
            }
            for (relative, source) in &profile.files {
                crate::fs::safe_relative(relative)?;
                if !source.is_absolute() {
                    return Err(Error::new(
                        ErrorCode::MixConfigInvalid,
                        format!("account file source must be absolute: {name}/{profile_name}/{relative}"),
                    ));
                }
            }
            for (relative, reference) in &profile.secret_files {
                crate::fs::safe_relative(relative)?;
                validate_secret_ref(reference, &format!("{name}/{profile_name}/{relative}"))?;
            }
            for name in profile.env.keys().chain(profile.secrets.keys()) {
                validate_env_name(name)?;
            }
            for (name, value) in &profile.env {
                if value.contains('\0') {
                    return Err(Error::new(
                        ErrorCode::MixConfigInvalid,
                        format!("environment variable contains a null byte: {name}"),
                    ));
                }
            }
            for (name, reference) in &profile.secrets {
                validate_secret_ref(reference, &format!("{name}/{profile_name}/{name}"))?;
            }
        }
    }
    for (workspace, bindings) in &config.workspace_bindings {
        let path = Path::new(workspace);
        if !path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            return Err(Error::new(
                ErrorCode::MixConfigInvalid,
                format!("project path is invalid: {workspace}"),
            ));
        }
        for (app, profile) in bindings {
            let client = config.apps.get(app).ok_or_else(|| {
                Error::new(
                    ErrorCode::MixConfigInvalid,
                    format!("project references unknown client: {app}"),
                )
            })?;
            if !client.profiles.contains_key(profile) {
                return Err(Error::new(
                    ErrorCode::MixConfigInvalid,
                    format!("project references unknown account: {app}/{profile}"),
                ));
            }
        }
    }
    for (workspace, metadata) in &config.workspace_meta {
        if !config.workspace_bindings.contains_key(workspace) {
            return Err(Error::new(
                ErrorCode::MixConfigInvalid,
                format!("project metadata has no project: {workspace}"),
            ));
        }
        if metadata
            .name
            .as_deref()
            .is_some_and(|name| invalid_display_text(name, 120))
        {
            return Err(Error::new(
                ErrorCode::MixConfigInvalid,
                format!("project name is invalid: {workspace}"),
            ));
        }
    }
    if config.pending_cleanup.profile_directories.len() > 10_000
        || config.pending_cleanup.credentials.len() > 10_000
        || config.pending_cleanup.runtime_revocations.len() > 10_000
    {
        return Err(Error::new(
            ErrorCode::MixConfigInvalid,
            "pending cleanup exceeds the safety limit",
        ));
    }
    let profiles_root = config.root.join("profiles");
    for path in &config.pending_cleanup.profile_directories {
        if !path.is_absolute()
            || !path.starts_with(&profiles_root)
            || path == &profiles_root
            || path
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            return Err(Error::new(
                ErrorCode::MixConfigInvalid,
                "pending profile cleanup path is outside Mix storage",
            ));
        }
    }
    for reference in &config.pending_cleanup.credentials {
        validate_secret_ref(reference, "pending cleanup")?;
    }
    for revocation in &config.pending_cleanup.runtime_revocations {
        validate_identifier(&revocation.app, "pending runtime client")?;
        validate_identifier(&revocation.profile, "pending runtime account")?;
        if !uuid::Uuid::parse_str(&revocation.profile_id)
            .is_ok_and(|value| value.to_string() == revocation.profile_id)
        {
            return Err(Error::new(
                ErrorCode::MixConfigInvalid,
                "pending runtime account identity is invalid",
            ));
        }
        if revocation.files.len() > 10_000 {
            return Err(Error::new(
                ErrorCode::MixConfigInvalid,
                "pending runtime revocation exceeds the safety limit",
            ));
        }
        for relative in &revocation.files {
            crate::fs::safe_relative(relative)?;
        }
    }
    Ok(())
}

fn invalid_display_text(value: &str, maximum_characters: usize) -> bool {
    value.trim().is_empty()
        || value.chars().count() > maximum_characters
        || value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
}

fn validate_secret_ref(reference: &crate::SecretRef, label: &str) -> Result<()> {
    validate_identifier(&reference.service, "credential service")?;
    if reference.account.is_empty()
        || reference.account.len() > 512
        || reference.account.chars().any(char::is_control)
    {
        return Err(Error::new(
            ErrorCode::MixConfigInvalid,
            format!("account credential reference is invalid: {label}"),
        ));
    }
    Ok(())
}

fn validate_env_name(name: &str) -> Result<()> {
    if !is_environment_variable_name(name) {
        return Err(Error::new(
            ErrorCode::MixConfigInvalid,
            format!("invalid environment variable name: {name}"),
        ));
    }
    Ok(())
}

pub fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    let path = expand_home(path)?;
    if !path.is_absolute() {
        return Err(Error::invalid("workspace path must be absolute"));
    }
    match fs::canonicalize(&path) {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            canonicalize_missing_workspace(&path)
        }
        Err(error) => Err(Error::io("cannot resolve workspace", error)),
    }
}

fn canonicalize_missing_workspace(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(mut canonical) => {
                for component in suffix.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| Error::invalid("workspace has no resolvable ancestor"))?;
                suffix.push(name.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| Error::invalid("workspace has no resolvable ancestor"))?;
            }
            Err(error) => return Err(Error::io("cannot resolve workspace", error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_concurrent_mutations() {
        let root = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(root.path().join("mix/config.json")).unwrap();
        store.initialize().unwrap();
        store
            .mutate(|config| {
                config
                    .workspace_bindings
                    .insert("/tmp/a".into(), std::collections::BTreeMap::new());
                Ok(())
            })
            .unwrap();
        assert!(store
            .load()
            .unwrap()
            .workspace_bindings
            .contains_key("/tmp/a"));
    }

    #[test]
    fn rejects_invalid_environment_names_in_persisted_profiles() {
        let root = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(root.path().join("mix/config.json")).unwrap();
        store.initialize().unwrap();
        let result = store.mutate(|config| {
            config.apps.insert(
                "codex".into(),
                crate::ClientConfig {
                    adapter: crate::AdapterKind::Claude,
                    live_dir: root.path().join("live"),
                    active_profile: None,
                    profiles: std::collections::BTreeMap::from([(
                        "work".into(),
                        crate::Profile {
                            label: "Work".into(),
                            env: std::collections::BTreeMap::from([(
                                "BAD-NAME".into(),
                                "value".into(),
                            )]),
                            ..crate::Profile::default()
                        },
                    )]),
                    run: crate::RunSpec::default(),
                },
            );
            Ok(())
        });
        assert!(result.is_err());
    }

    #[test]
    fn rejects_bidirectional_controls_in_user_visible_names() {
        assert!(invalid_display_text("safe name\u{202e}txt", 120));
        assert!(!invalid_display_text("公司账号 🚀", 120));
    }

    #[test]
    fn rejects_a_config_root_that_differs_from_its_storage_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mix/config.json");
        let store = ConfigStore::new(&path).unwrap();
        store.initialize().unwrap();
        let mut config = store.load().unwrap();
        config.root = root.path().join("other");
        atomic_json(&path, &config).unwrap();
        assert_eq!(store.load().unwrap_err().code, ErrorCode::MixConfigInvalid);
    }

    #[test]
    fn unknown_configuration_fields_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mix/config.json");
        let store = ConfigStore::new(&path).unwrap();
        store.initialize().unwrap();
        store
            .mutate(|config| {
                config.apps.insert(
                    "codex".into(),
                    crate::ClientConfig {
                        adapter: crate::AdapterKind::Codex,
                        live_dir: root.path().join(".codex"),
                        active_profile: None,
                        profiles: std::collections::BTreeMap::new(),
                        run: crate::RunSpec::default(),
                    },
                );
                Ok(())
            })
            .unwrap();
        let mut stale: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        stale["apps"]["codex"]["process"] = serde_json::json!({
            "start": ["open", "-a", "ChatGPT"],
            "restart": true
        });
        atomic_json(&path, &stale).unwrap();

        assert_eq!(
            store.initialize().unwrap_err().code,
            ErrorCode::MixConfigInvalid
        );
    }

    #[test]
    fn persisted_revision_resolves_an_ambiguous_write_as_committed() {
        let root = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(root.path().join("mix/config.json")).unwrap();
        store.initialize().unwrap();
        let revision = store
            .load()
            .unwrap()
            .meta
            .revision
            .expect("initialized config revision");
        assert!(store.mutation_was_persisted(&revision));
        assert!(!store.mutation_was_persisted(&uuid::Uuid::new_v4().to_string()));
    }
}
