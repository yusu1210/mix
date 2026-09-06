#[cfg(test)]
use crate::adapter::{
    codex_login_config, codex_materialized_config, merge_codex_credential, provider_from_toml,
    read_codex_config, verify_live_projection, AccountCapture, AccountEnrollment,
    AccountEnrollmentPlan, GlobalAccountProjection,
};
use crate::adapter::{
    discover_executable, Adapter, AdapterRegistry, CapturedAccount, CodexAdapter,
};
use crate::fs::{
    atomic_write, open_lock, private_dir, private_scoped_dir, read_bounded, remove_durable,
    safe_child, shell_quote, validate_scoped_path,
};
use crate::model::is_environment_variable_name;
use crate::process::ProcessController;
use crate::store::canonical_workspace;
use crate::transaction::{SwitchJournal, SwitchPlan};
use crate::{
    local_vault, Activity, AdapterKind, ClientConfig, ClientIssue, ClientIssueCode, ClientSnapshot,
    ClientStatus, ConfigStore, CredentialVault, Discovery, Error, ErrorCode, Health, HealthIssue,
    HealthStatus, InterruptedSwitch, PendingRuntimeRevocation, Profile, ProfileCategory,
    ProfileLabelOrigin, ProfileSnapshot, Recoverability, RecoverySnapshot, Result, RunSpec,
    SecretRef, SecuritySnapshot, Session, SessionCatalogSource, SessionProjectSource,
    SessionRecoveryReason, Snapshot, SwitchOutcome, VaultCapability, WorkspaceMeta,
    WorkspaceSnapshot,
};
#[cfg(test)]
use crate::{AccountSwitchCapability, AccountSwitchStatus, ProviderIdentity};
use fs2::FileExt;
use parking_lot::{Mutex, MutexGuard};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};
use uuid::Uuid;

pub struct MixService {
    store: ConfigStore,
    vault: Arc<dyn CredentialVault>,
    adapters: AdapterRegistry,
    mutation: Mutex<()>,
}

struct MutationGuard<'a> {
    _local: MutexGuard<'a, ()>,
    _cross_process: fs::File,
}

const ENROLLMENT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const ENROLLMENT_RECORD_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const TERMINAL_HANDOFF_RETENTION: Duration = Duration::from_secs(60 * 60);
const PENDING_RUNTIME_LEASE_RETENTION: Duration = Duration::from_secs(60 * 60);
const RUNTIME_REVOKE_MARKER: &str = ".mix-credentials-revoked.json";
const RUNTIME_CREDENTIAL_LOCK: &str = ".mix-credentials.lock";
const RUNTIME_LEASE_DIRECTORY: &str = ".mix-credential-leases";

struct RuntimeCredentialLease {
    path: PathBuf,
    directory: PathBuf,
    lock_path: PathBuf,
    _lock: fs::File,
}

struct ProjectedSecretFiles {
    files: Vec<PathBuf>,
    lease: Option<RuntimeCredentialLease>,
    armed: bool,
}

struct TerminalLaunch<'a> {
    argv: &'a [String],
    variable: Option<&'a str>,
    state_dir: String,
    cwd: Option<&'a str>,
    environment: &'a BTreeMap<String, String>,
    completion_path: Option<PathBuf>,
}

impl ProjectedSecretFiles {
    fn new() -> Self {
        Self {
            files: Vec::new(),
            lease: None,
            armed: true,
        }
    }

    fn for_runtime(runtime: &Path, controlled_root: &Path, files: Vec<PathBuf>) -> Result<Self> {
        validate_scoped_path(runtime, controlled_root)?;
        let lock_path = runtime.join(RUNTIME_CREDENTIAL_LOCK);
        let lock = open_lock(&lock_path)?;
        lock.lock_exclusive()
            .map_err(|error| Error::io("cannot lock runtime credentials", error))?;
        let directory = runtime.join(RUNTIME_LEASE_DIRECTORY);
        private_scoped_dir(&directory, controlled_root)?;
        prune_runtime_credential_leases(&directory)?;
        if runtime_lease_directory_is_empty(&directory)? {
            for path in &files {
                remove_durable(path)?;
            }
        }
        let path = directory.join(Uuid::new_v4().to_string());
        atomic_write(&path, b"pending\n")?;
        Ok(Self {
            files,
            lease: Some(RuntimeCredentialLease {
                path,
                directory,
                lock_path,
                _lock: lock,
            }),
            armed: true,
        })
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProjectedSecretFiles {
    fn drop(&mut self) {
        if self.armed {
            if let Some(lease) = &self.lease {
                let _ = remove_durable(&lease.path);
                if runtime_lease_directory_is_empty(&lease.directory).unwrap_or(false) {
                    for path in &self.files {
                        let _ = remove_durable(path);
                    }
                }
            } else {
                for path in &self.files {
                    let _ = remove_durable(path);
                }
            }
        }
    }
}

struct RuntimeDirectory {
    path: PathBuf,
    profile: String,
    profile_id: Option<String>,
    cwd: Option<String>,
    account_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentRecord {
    version: u32,
    id: String,
    app: String,
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repair_profile: Option<String>,
    temp_dir: PathBuf,
    created_at: String,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

const ENROLLMENT_VERSION: u32 = 1;

impl MixService {
    pub fn new(config_path: impl AsRef<Path>) -> Result<Self> {
        let store = ConfigStore::new(config_path)?;
        let vault = local_vault(store.root()?)?;
        Ok(Self {
            store,
            vault,
            adapters: AdapterRegistry::default(),
            mutation: Mutex::new(()),
        })
    }

    pub fn with_vault(
        config_path: impl AsRef<Path>,
        vault: Arc<dyn CredentialVault>,
    ) -> Result<Self> {
        Ok(Self {
            store: ConfigStore::new(config_path)?,
            vault,
            adapters: AdapterRegistry::default(),
            mutation: Mutex::new(()),
        })
    }

    pub fn initialize(&self) -> Result<Snapshot> {
        let _guard = self.lock_mutation()?;
        self.store.initialize()?;
        let _ = self.finish_pending_cleanup()?;
        let config = self.store.load()?;
        prune_terminal_handoffs(&config.root)?;
        prune_enrollments(&config.root)?;
        prune_revoked_runtime_files(&config)?;
        prune_inactive_runtime_credentials(&config)?;
        self.state()
    }

    pub fn config_path(&self) -> &Path {
        self.store.path()
    }

    pub fn vault_capability(&self) -> VaultCapability {
        self.vault.capability()
    }

    fn process_controller(&self, client: &ClientConfig) -> ProcessController {
        ProcessController::new(self.adapters.get(client.adapter).desktop_process(client))
    }

    pub fn state(&self) -> Result<Snapshot> {
        let config = self.store.load()?;
        let interrupted = interrupted(&config.root);
        let mut health_issues = Vec::new();
        if interrupted.required {
            health_issues.push(HealthIssue {
                app: interrupted.app.clone().unwrap_or_else(|| "mix".into()),
                issue: ClientIssue::new(ClientIssueCode::InterruptedSwitch),
            });
        }
        if !config.pending_cleanup.is_empty() {
            health_issues.push(HealthIssue {
                app: "mix".into(),
                issue: ClientIssue::new(ClientIssueCode::CleanupPending),
            });
        }
        let mut apps = Vec::new();
        for (name, client) in &config.apps {
            let adapter = self.adapters.get(client.adapter);
            let executable = discover_executable(client);
            let account = adapter.account_capability(client);
            let effective_active = adapter.observed_profile(client);
            let mut issues = Vec::new();
            if client.profiles.is_empty() {
                issues.push(ClientIssue::new(ClientIssueCode::NoProfiles));
            }
            if client.run.command.is_empty() {
                issues.push(ClientIssue::new(ClientIssueCode::RunCommandMissing));
            } else if executable.is_none() {
                issues.push(ClientIssue::new(ClientIssueCode::RunCommandUnavailable));
            }
            if adapter.global_account_projection().is_some()
                && !client.profiles.is_empty()
                && account.available
                && account.current_profile.is_none()
            {
                issues.push(ClientIssue::new(ClientIssueCode::UnmanagedAccount));
            }
            if adapter.global_account_projection().is_some()
                && client
                    .profiles
                    .values()
                    .any(|profile| profile.account_fingerprint.is_none())
            {
                issues.push(ClientIssue::new(ClientIssueCode::UnsupportedAccountProfile));
            }
            for issue in &issues {
                // A client without an account is still in onboarding. Keep
                // its setup details on the client card, but do not turn the
                // global health status amber before setup is complete.
                if client.profiles.is_empty() {
                    continue;
                }
                health_issues.push(HealthIssue {
                    app: name.clone(),
                    issue: issue.clone(),
                });
            }
            let profiles = client
                .profiles
                .iter()
                .map(|(profile_name, profile)| {
                    let mut identity = profile.account_identity.clone();
                    if account.current_profile.as_deref() == Some(profile_name) {
                        identity = account.identity.clone().or(identity);
                    }
                    let provider = adapter.profile_provider(profile).ok().flatten();
                    let display_label = adapter.profile_display_label(
                        profile,
                        identity.as_ref(),
                        provider.as_ref(),
                    );
                    ProfileSnapshot {
                        name: profile_name.clone(),
                        label: profile.label.clone(),
                        display_label,
                        managed_account: profile.account_fingerprint.is_some(),
                        has_credentials: !profile.secret_files.is_empty()
                            || !profile.secrets.is_empty(),
                        auth_strategy: profile.auth_strategy.unwrap_or_default(),
                        file_count: profile.files.len(),
                        secret_count: profile.secret_files.len() + profile.secrets.len(),
                        identity,
                        provider,
                    }
                })
                .collect::<Vec<_>>();
            let status = if client.profiles.is_empty() {
                ClientStatus::SetupRequired
            } else if issues.is_empty() {
                ClientStatus::Ready
            } else {
                ClientStatus::Warning
            };
            apps.push(ClientSnapshot {
                name: name.clone(),
                adapter: client.adapter,
                profile_category: adapter.descriptor().profile_category,
                live_dir: client.live_dir.clone(),
                active: effective_active,
                configured_active: client.active_profile.clone(),
                status,
                command: executable.as_ref().map(|value| value.display().to_string()),
                command_found: executable.is_some(),
                issues,
                account_switch: account,
                capabilities: adapter.capabilities(client, executable.is_some()),
                import_files: adapter
                    .descriptor()
                    .import_files
                    .iter()
                    .map(|value| (*value).into())
                    .collect(),
                profiles,
            });
        }
        apps.sort_by(|left, right| {
            left.adapter
                .cmp(&right.adapter)
                .then_with(|| left.name.cmp(&right.name))
        });
        let vault = self.vault.capability();
        if config
            .apps
            .values()
            .flat_map(|client| client.profiles.values())
            .any(|profile| !profile.secret_files.is_empty() || !profile.secrets.is_empty())
            && !vault.available
        {
            health_issues.push(HealthIssue {
                app: "mix".into(),
                issue: ClientIssue::new(ClientIssueCode::CredentialStoreUnavailable),
            });
        }
        let workspaces = workspaces_from(&config);
        let activities = self.activities(10)?;
        Ok(Snapshot {
            needs_setup: apps.is_empty()
                || apps
                    .iter()
                    .any(|app| matches!(app.status, ClientStatus::SetupRequired)),
            apps,
            workspaces,
            activity: activities,
            health: Health {
                status: if health_issues.is_empty() {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Attention
                },
                issues: health_issues,
                local_only: true,
            },
            security: SecuritySnapshot {
                credential_store: vault,
            },
            recovery: RecoverySnapshot {
                interrupted_switch: interrupted,
            },
        })
    }

    pub fn profile_category(&self, app: &str) -> Result<ProfileCategory> {
        let config = self.store.load()?;
        let adapter = config
            .apps
            .get(app)
            .map(|client| client.adapter)
            .ok_or_else(|| Error::not_found("client", app))?;
        Ok(self.adapters.get(adapter).descriptor().profile_category)
    }

    pub fn discovery(&self) -> Result<Vec<Discovery>> {
        let config = self.store.load()?;
        let home = dirs::home_dir().ok_or_else(|| {
            Error::new(
                ErrorCode::MixLocalFailure,
                "the user home directory is unavailable",
            )
        })?;
        let configured = config
            .apps
            .values()
            .map(|client| client.adapter)
            .collect::<BTreeSet<_>>();
        Ok(self
            .adapters
            .all()
            .map(|adapter| {
                let descriptor = adapter.descriptor();
                let kind = descriptor.kind;
                let live_dir = home.join(descriptor.default_home);
                let temporary = ClientConfig {
                    adapter: kind,
                    live_dir: live_dir.clone(),
                    active_profile: None,
                    profiles: BTreeMap::new(),
                    run: RunSpec {
                        command: descriptor
                            .default_command
                            .iter()
                            .map(|value| (*value).into())
                            .collect(),
                    },
                };
                let executable = discover_executable(&temporary);
                Discovery {
                    adapter: kind,
                    profile_category: descriptor.profile_category,
                    name: descriptor.name.into(),
                    label: descriptor.label.into(),
                    installed: executable.is_some(),
                    executable,
                    desktop_executable: adapter
                        .desktop_process(&temporary)
                        .and_then(|process| process.executable),
                    config_exists: live_dir.is_dir(),
                    sessions_detected: descriptor
                        .session_roots
                        .iter()
                        .any(|relative| live_dir.join(relative).exists()),
                    import_files: descriptor
                        .import_files
                        .iter()
                        .map(|value| (*value).into())
                        .collect(),
                    configured: configured.contains(&kind),
                    account_switch: adapter.account_capability(&temporary),
                    live_dir,
                }
            })
            .collect())
    }

    pub fn register_client(
        &self,
        name: &str,
        adapter: AdapterKind,
        live_dir: PathBuf,
    ) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        validate_identifier(name)?;
        let live_dir = absolute(live_dir)?;
        self.mutate_config(|config| {
            if config.apps.contains_key(name) {
                return Err(Error::new(
                    ErrorCode::MixConflict,
                    format!("client already exists: {name}"),
                ));
            }
            if config.apps.values().any(|client| client.adapter == adapter) {
                return Err(Error::new(
                    ErrorCode::MixConflict,
                    format!("client type {} is already connected", adapter.as_str()),
                ));
            }
            let command = self
                .adapters
                .get(adapter)
                .descriptor()
                .default_command
                .iter()
                .map(|value| (*value).into())
                .collect();
            config.apps.insert(
                name.into(),
                ClientConfig {
                    adapter,
                    live_dir,
                    active_profile: None,
                    profiles: BTreeMap::new(),
                    run: RunSpec { command },
                },
            );
            Ok(json!({"status":"registered","app":name}))
        })
    }

    pub fn capture_current_account(
        &self,
        app: &str,
        requested_name: Option<&str>,
        label: Option<&str>,
    ) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        let mut captured = None;
        let mut created_root = None;
        let mut created_references = Vec::new();
        let operation = self.mutate_config(|config| {
            let client = config
                .apps
                .get_mut(app)
                .ok_or_else(|| Error::not_found("client", app))?;
            let adapter = self.adapters.get(client.adapter);
            let account_capture = adapter.account_capture().ok_or_else(|| {
                Error::new(
                    ErrorCode::MixUnsupported,
                    "this client does not expose current-account capture",
                )
            })?;
            let account = account_capture.capture_current(client)?;
            validate_captured_account(&account)?;
            if let Some(existing) = account_capture.matching_profile(client, &account)? {
                account_capture.refresh_profile(
                    client,
                    &existing,
                    &account,
                    self.vault.as_ref(),
                )?;
                client.active_profile = Some(existing.clone());
                captured = Some(json!({"status":"already_added","app":app,"profile":existing}));
                return Ok(());
            }
            let name = match requested_name.filter(|value| !value.trim().is_empty()) {
                Some(name) => name.to_owned(),
                None => next_profile_name(
                    self.adapters
                        .get(client.adapter)
                        .descriptor()
                        .profile_prefix,
                    &client.profiles,
                )?,
            };
            validate_identifier(&name)?;
            if client.profiles.contains_key(&name) {
                return Err(Error::new(
                    ErrorCode::MixConflict,
                    format!("account already exists: {name}"),
                ));
            }
            let supplied_label = label
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.trim().to_owned());
            let label_origin = if supplied_label
                .as_deref()
                .is_some_and(|value| value != account.suggested_label)
            {
                ProfileLabelOrigin::User
            } else {
                ProfileLabelOrigin::Generated
            };
            let display = supplied_label.unwrap_or_else(|| account.suggested_label.clone());
            let profile_root = config.root.join("profiles").join(app).join(&name);
            if fs::symlink_metadata(&profile_root).is_ok() {
                return Err(Error::new(
                    ErrorCode::MixConflict,
                    format!("account storage already exists: {app}/{name}"),
                ));
            }
            private_scoped_dir(&profile_root, &config.root.join("profiles"))?;
            created_root = Some(profile_root.clone());
            let profile = materialize_captured_profile(
                self,
                client,
                &profile_root,
                &account,
                display,
                label_origin,
                &mut created_references,
            )?;
            client.profiles.insert(name.clone(), profile);
            client.active_profile = Some(name.clone());
            captured = Some(json!({"status":"added","app":app,"profile":name}));
            Ok(())
        });
        if let Err(error) = operation {
            return Err(self.failed_operation_with_cleanup(
                error,
                created_root.into_iter().collect(),
                created_references,
            ));
        }
        let mut result = captured.ok_or_else(|| {
            Error::new(
                ErrorCode::MixInternalError,
                "account capture completed without a result",
            )
        })?;
        let activity = self.record_activity("account_added", result.clone());
        attach_activity_warning(&mut result, activity);
        Ok(result)
    }

    pub fn start_account_enrollment(
        &self,
        app: &str,
        repair_profile: Option<&str>,
    ) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        let config = self.store.load()?;
        self.ensure_no_recovery_for(&config)?;
        let client = config
            .apps
            .get(app)
            .ok_or_else(|| Error::not_found("client", app))?;
        let enrollment = self
            .adapters
            .get(client.adapter)
            .account_enrollment()
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::MixUnsupported,
                    "this client does not support isolated account enrollment",
                )
            })?;
        if let Some(profile) = repair_profile {
            let target = client
                .profiles
                .get(profile)
                .ok_or_else(|| Error::not_found("account", profile))?;
            if target.account_fingerprint.is_none() {
                return Err(Error::new(
                    ErrorCode::MixUnsupported,
                    "only a managed account can be repaired",
                ));
            }
        }
        if let Some(record) = pending_enrollment(&config.root, app)? {
            return Ok(enrollment_value(&record));
        }
        let executable = discover_executable(client).ok_or_else(|| {
            Error::new(ErrorCode::MixNotFound, "client executable is unavailable")
        })?;
        let plan = enrollment.prepare(client, &executable)?;
        let id = Uuid::new_v4().to_string();
        let temp_dir = config.root.join("transactions/enrollments").join(&id);
        private_scoped_dir(&temp_dir, &config.root.join("transactions/enrollments"))?;
        let setup = (|| {
            for (relative, payload) in &plan.files {
                atomic_write(&safe_child(&temp_dir, relative)?, payload)?;
            }
            let record = EnrollmentRecord {
                version: ENROLLMENT_VERSION,
                id: id.clone(),
                app: app.into(),
                mode: if repair_profile.is_some() {
                    "repair".into()
                } else {
                    "add".into()
                },
                repair_profile: repair_profile.map(str::to_owned),
                temp_dir: temp_dir.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
                state: "pending".into(),
                profile: None,
                error: None,
                code: None,
            };
            write_enrollment(&config.root, &record)?;
            Ok::<_, Error>(record)
        })();
        let mut record = match setup {
            Ok(record) => record,
            Err(error) => {
                if let Ok(mut persisted) = read_enrollment(&config.root, &id) {
                    persisted.state = "failed".into();
                    persisted.code = Some(error.code.as_str().into());
                    persisted.error = Some(error.to_string());
                    if let Err(cleanup) = finish_enrollment(&config.root, &mut persisted) {
                        return Err(Error::new(
                            error.code,
                            format!("{error}; enrollment cleanup failed: {cleanup}"),
                        ));
                    }
                } else if let Err(cleanup) = remove_enrollment_dir(&config.root, &temp_dir) {
                    return Err(Error::new(
                        error.code,
                        format!("{error}; enrollment cleanup failed: {cleanup}"),
                    ));
                }
                return Err(error);
            }
        };
        let empty_environment = BTreeMap::new();
        let launch = launch_terminal(
            &config.root,
            TerminalLaunch {
                argv: &plan.argv,
                variable: plan.runtime_variable,
                state_dir: temp_dir.display().to_string(),
                cwd: None,
                environment: &empty_environment,
                completion_path: Some(temp_dir.join(".mix-login-exit")),
            },
            ProjectedSecretFiles::new(),
        );
        if let Err(error) = launch {
            record.state = "failed".into();
            record.code = Some(error.code.as_str().into());
            record.error = Some(error.to_string());
            if let Err(cleanup) = finish_enrollment(&config.root, &mut record) {
                return Err(Error::new(
                    error.code,
                    format!("{error}; enrollment cleanup failed: {cleanup}"),
                ));
            }
            return Err(error);
        }
        Ok(enrollment_value(&record))
    }

    pub fn account_enrollment_status(&self, id: &str) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        validate_identifier(id)?;
        let config = self.store.load()?;
        let mut record = read_enrollment(&config.root, id)?;
        if record.state != "pending" {
            if let Err(error) = cleanup_enrollment_record(&config.root, &record) {
                record.error = Some(format!("cleanup pending: {error}"));
                let _ = write_enrollment(&config.root, &record);
            }
            return Ok(enrollment_value(&record));
        }
        if enrollment_expired(&record) {
            record.state = "failed".into();
            record.code = Some("MIX_ENROLLMENT_TIMEOUT".into());
            record.error = Some("client sign-in timed out".into());
            finish_enrollment(&config.root, &mut record)?;
            return Ok(enrollment_value(&record));
        }
        let current = config
            .apps
            .get(&record.app)
            .ok_or_else(|| Error::not_found("client", &record.app))?;
        let adapter = self.adapters.get(current.adapter);
        let enrollment = adapter.account_enrollment().ok_or_else(|| {
            Error::new(
                ErrorCode::MixUnsupported,
                "this client no longer supports isolated account enrollment",
            )
        })?;
        let account_capture = adapter.account_capture().ok_or_else(|| {
            Error::new(
                ErrorCode::MixUnsupported,
                "this client no longer supports account capture",
            )
        })?;
        let account = match enrollment.capture_completed(current, &record.temp_dir) {
            Ok(Some(account)) => account,
            Ok(None) => {
                if enrollment_terminal_finished(&record)? {
                    record.state = "failed".into();
                    record.code = Some("MIX_ENROLLMENT_CANCELLED".into());
                    record.error = Some("client sign-in closed without a usable login".into());
                    finish_enrollment(&config.root, &mut record)?;
                }
                return Ok(enrollment_value(&record));
            }
            Err(error) => {
                record.state = "failed".into();
                record.code = Some(error.code.as_str().into());
                record.error = Some(error.to_string());
                finish_enrollment(&config.root, &mut record)?;
                return Ok(enrollment_value(&record));
            }
        };
        if let Err(error) = validate_captured_account(&account) {
            record.state = "failed".into();
            record.code = Some(error.code.as_str().into());
            record.error = Some(error.to_string());
            finish_enrollment(&config.root, &mut record)?;
            return Ok(enrollment_value(&record));
        }
        if let Some(profile_name) = record.repair_profile.as_deref() {
            let target = current
                .profiles
                .get(profile_name)
                .ok_or_else(|| Error::not_found("account", profile_name))?;
            if target.account_fingerprint.as_deref() != Some(&account.fingerprint) {
                record.state = "failed".into();
                record.code = Some("MIX_ACCOUNT_REPAIR_IDENTITY_MISMATCH".into());
                record.error = Some("the isolated login belongs to another account".into());
                finish_enrollment(&config.root, &mut record)?;
                return Ok(enrollment_value(&record));
            }
            self.refresh_captured_profile(&record.app, profile_name, &account)?;
            record.state = "completed".into();
            record.profile = Some(profile_name.into());
            finish_enrollment(&config.root, &mut record)?;
            return Ok(enrollment_value(&record));
        }
        if let Some(existing) = account_capture.matching_profile(current, &account)? {
            self.refresh_captured_profile(&record.app, &existing, &account)?;
            record.state = "completed".into();
            record.profile = Some(existing);
            finish_enrollment(&config.root, &mut record)?;
            return Ok(enrollment_value(&record));
        }
        let name = next_profile_name(
            self.adapters
                .get(current.adapter)
                .descriptor()
                .profile_prefix,
            &current.profiles,
        )?;
        let profile_root = config.root.join("profiles").join(&record.app).join(&name);
        if fs::symlink_metadata(&profile_root).is_ok() {
            return Err(Error::new(
                ErrorCode::MixConflict,
                format!("account storage already exists: {}/{}", record.app, name),
            ));
        }
        private_scoped_dir(&profile_root, &config.root.join("profiles"))?;
        let mut created_references = Vec::new();
        let provision = materialize_captured_profile(
            self,
            current,
            &profile_root,
            &account,
            account.suggested_label.clone(),
            ProfileLabelOrigin::Generated,
            &mut created_references,
        );
        let profile = match provision {
            Ok(profile) => profile,
            Err(error) => {
                let error = self.failed_operation_with_cleanup(
                    error,
                    vec![profile_root],
                    created_references,
                );
                record.state = "failed".into();
                record.code = Some(error.code.as_str().into());
                record.error = Some(error.to_string());
                finish_enrollment(&config.root, &mut record)?;
                return Ok(enrollment_value(&record));
            }
        };
        let add_result = self.store.mutate(|config| {
            let client = config
                .apps
                .get_mut(&record.app)
                .ok_or_else(|| Error::not_found("client", &record.app))?;
            if account_capture
                .matching_profile(client, &account)?
                .is_some()
            {
                return Err(Error::new(
                    ErrorCode::MixConflict,
                    "this account was added by another operation",
                ));
            }
            client.profiles.insert(name.clone(), profile);
            if client.active_profile.is_none() {
                client.active_profile = Some(name.clone());
            }
            Ok(())
        });
        if let Err(error) = add_result {
            return Err(self.failed_operation_with_cleanup(
                error,
                vec![profile_root],
                created_references,
            ));
        }
        record.state = "completed".into();
        record.profile = Some(name);
        finish_enrollment(&config.root, &mut record)?;
        Ok(enrollment_value(&record))
    }

    pub fn repair_active_login(&self, app: &str, profile: &str) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        let config = self.store.load()?;
        self.ensure_no_recovery_for(&config)?;
        let client = config
            .apps
            .get(app)
            .ok_or_else(|| Error::not_found("client", app))?;
        let account_capture = self
            .adapters
            .get(client.adapter)
            .account_capture()
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::MixUnsupported,
                    "this client does not support active-login repair",
                )
            })?;
        let target = client
            .profiles
            .get(profile)
            .ok_or_else(|| Error::not_found("account", profile))?;
        let account = account_capture.capture_current(client)?;
        if target.account_fingerprint.as_deref() != Some(&account.fingerprint) {
            return Err(Error::new(
                ErrorCode::MixAccountLocalRepairUnavailable,
                "the active client login does not match this account",
            ));
        }
        self.refresh_captured_profile(app, profile, &account)?;
        Ok(
            json!({"repaired":true,"status":"repaired","source":"active_login","app":app,"profile":profile}),
        )
    }

    fn refresh_captured_profile(
        &self,
        app: &str,
        profile: &str,
        account: &CapturedAccount,
    ) -> Result<()> {
        let config = self.store.load()?;
        let client = config
            .apps
            .get(app)
            .ok_or_else(|| Error::not_found("client", app))?;
        let account_capture = self
            .adapters
            .get(client.adapter)
            .account_capture()
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::MixUnsupported,
                    "this client does not support account credential refresh",
                )
            })?;
        let target = client
            .profiles
            .get(profile)
            .ok_or_else(|| Error::not_found("account", profile))?;
        if target.account_fingerprint.as_deref() != Some(&account.fingerprint) {
            return Err(Error::new(
                ErrorCode::MixAccountLocalRepairUnavailable,
                "the captured login does not match the saved account",
            ));
        }
        self.store.mutate(|config| {
            let client = config
                .apps
                .get_mut(app)
                .ok_or_else(|| Error::not_found("client", app))?;
            if client
                .profiles
                .get(profile)
                .and_then(|target| target.account_fingerprint.as_deref())
                != Some(account.fingerprint.as_str())
            {
                return Err(Error::new(
                    ErrorCode::MixConflict,
                    "the saved account changed while its credential was refreshed",
                ));
            }
            account_capture.refresh_profile(client, profile, account, self.vault.as_ref())
        })
    }

    pub fn switch(&self, app: &str, profile: &str) -> Result<SwitchOutcome> {
        let _guard = self.lock_mutation()?;
        let config = self.store.load()?;
        let mut outcome = None;
        let mut stopped_controller = None;
        let mut stopped_pids = Vec::new();
        let mut restart_required = false;
        let operation = self.store.mutate(|config| {
            self.ensure_no_recovery_for(config)?;
            let client = config
                .apps
                .get_mut(app)
                .ok_or_else(|| Error::not_found("client", app))?;
            let adapter = self.adapters.get(client.adapter);
            let profile = resolve_profile_selector(adapter, client, profile)?;
            let target = client
                .profiles
                .get(&profile)
                .cloned()
                .ok_or_else(|| Error::not_found("account", &profile))?;
            adapter.validate_profile(client, &target)?;
            let Some(global_account) = adapter.global_account_projection() else {
                let source = client.active_profile.replace(profile.clone());
                outcome = Some(SwitchOutcome {
                    status: if source.as_deref() == Some(profile.as_str()) {
                        "already_selected"
                    } else {
                        "selected"
                    }
                    .into(),
                    app: app.into(),
                    from_profile: source,
                    to_profile: profile,
                    backup: String::new(),
                    stopped_pids: Vec::new(),
                    restarted: false,
                    warning: None,
                });
                return Ok(());
            };
            let source = global_account.switch_source(client)?;
            if source.as_deref() == Some(profile.as_str()) {
                let synchronized =
                    global_account.synchronize_source(client, self.vault.as_ref())?;
                if synchronized.as_deref() != Some(profile.as_str()) {
                    return Err(Error::new(
                        ErrorCode::MixSwitchVerificationFailed,
                        "the active account changed while Mix was synchronizing it",
                    ));
                }
                client.active_profile = Some(profile.clone());
                let controller = self.process_controller(client);
                let restarted = controller.ensure_active()?;
                outcome = Some(SwitchOutcome {
                    status: "already_active".into(),
                    app: app.into(),
                    from_profile: source,
                    to_profile: profile.clone(),
                    backup: String::new(),
                    stopped_pids: Vec::new(),
                    restarted,
                    warning: None,
                });
                return Ok(());
            }
            global_account.prepare_target_credential(client, &target, self.vault.as_ref())?;
            let controller = self.process_controller(client);
            stopped_controller = Some(controller.clone());
            restart_required = controller.restart_configured();
            let stopped = controller.stop()?;
            stopped_pids = stopped.clone();
            let from_name = global_account.synchronize_source(client, self.vault.as_ref())?;
            let from = from_name
                .as_ref()
                .and_then(|name| client.profiles.get(name));
            let projection = global_account.switch_projection(client, &target)?;
            let journal = SwitchJournal::prepare(
                &config.root,
                SwitchPlan {
                    app,
                    live_dir: &client.live_dir,
                    from_name: from_name.as_deref(),
                    from,
                    to_name: &profile,
                    to: &target,
                    file_overrides: projection.files,
                    file_removals: projection.removals,
                    file_rewrites: projection.rewrites,
                    restart_required,
                },
                self.vault.as_ref(),
            )?;
            journal.apply(self.vault.as_ref())?;
            global_account.verify_projection(client, &target)?;
            let restarted = if restart_required {
                controller.start()?
            } else {
                false
            };
            if restarted {
                global_account.verify_stable_projection(client, &target)?;
            }
            global_account.synchronize_target(client, &target, self.vault.as_ref())?;
            client.active_profile = Some(profile.clone());
            outcome = Some(SwitchOutcome {
                status: "switched".into(),
                app: app.into(),
                from_profile: from_name,
                to_profile: profile,
                backup: journal.backup_dir(&config.root).display().to_string(),
                stopped_pids: stopped,
                restarted,
                warning: None,
            });
            Ok(())
        });
        if let Err(error) = operation {
            if let Some(journal) = SwitchJournal::load(&config.root)? {
                ensure_journal_live_dir_matches_client(&config, app, &journal)?;
                if let Some(controller) = stopped_controller.as_ref() {
                    stop_before_recovery(controller)?;
                }
                let restart_required = journal.restart_required();
                let from_profile = journal.from_profile.clone();
                let to_profile = journal.to_profile.clone();
                let old_client = config.apps.get(app);
                journal.restore(&config.root, self.vault.as_ref())?;
                self.store
                    .mutate(|config| {
                        let client = config
                            .apps
                            .get_mut(app)
                            .ok_or_else(|| Error::not_found("client", app))?;
                        client.active_profile = from_profile.clone();
                        Ok(())
                    })
                    .map_err(|restore_error| {
                        Error::new(
                            ErrorCode::MixSwitchRecoveryFailed,
                            format!(
                                "the live account was restored, but Mix state could not be restored; the recovery journal was preserved: {restore_error}"
                            ),
                        )
                    })?;
                let cleanup = journal
                    .cleanup_after_restore(&config.root, self.vault.as_ref())
                    .err();
                if restart_required {
                    if let Some(client) = old_client {
                        if let Err(restart) = self.process_controller(client).start() {
                            return Err(Error::new(
                                ErrorCode::MixSwitchRecoveryFailed,
                                format!("account switch was restored but the client could not restart: {restart}"),
                            ));
                        }
                    }
                }
                return Err(rolled_back_error(
                    error,
                    cleanup,
                    from_profile.as_deref(),
                    &to_profile,
                ));
            }
            if restart_required {
                if let Some(controller) = stopped_controller {
                    if let Err(restart) = controller.ensure_active() {
                        return Err(Error::new(
                            ErrorCode::MixSwitchRecoveryFailed,
                            format!("account switch did not start and the client could not restart: {restart}"),
                        ));
                    }
                }
            }
            return Err(error);
        }
        if let Some(journal) = SwitchJournal::load(&config.root)? {
            ensure_journal_belongs_to_client(&config, app, &journal)?;
            if let Err(error) = journal.commit(&config.root, self.vault.as_ref()) {
                let pending = SwitchJournal::load(&config.root)?;
                let finalized = error.details.get("applied").and_then(Value::as_bool) == Some(true)
                    || pending.as_ref().is_some_and(SwitchJournal::is_committed);
                if finalized {
                    if let Some(outcome) = outcome.as_mut() {
                        outcome.status = "switched_cleanup_pending".into();
                        outcome.warning = Some(error.to_string());
                    }
                } else {
                    // The commit marker could not be made durable, so this
                    // is still a recoverable prepared transaction. Restore
                    // the old state before reporting the switch as failed.
                    let pending = pending.ok_or_else(|| {
                        Error::new(
                            ErrorCode::MixSwitchRecoveryFailed,
                            format!(
                                "account switch commit failed without a recovery journal: {error}"
                            ),
                        )
                    })?;
                    if let Some(controller) = stopped_controller.as_ref() {
                        stop_before_recovery(controller)?;
                    }
                    let restart_required = pending.restart_required();
                    let from_profile = pending.from_profile.clone();
                    let to_profile = pending.to_profile.clone();
                    pending.restore(&config.root, self.vault.as_ref())?;
                    self.store.mutate(|config| {
                        let client = config
                            .apps
                            .get_mut(app)
                            .ok_or_else(|| Error::not_found("client", app))?;
                        client.active_profile = from_profile.clone();
                        Ok(())
                    })?;
                    let cleanup = pending
                        .cleanup_after_restore(&config.root, self.vault.as_ref())
                        .err();
                    if restart_required {
                        if let Some(client) = config.apps.get(app) {
                            self.process_controller(client).start()?;
                        }
                    }
                    return Err(rolled_back_error(
                        error,
                        cleanup,
                        from_profile.as_deref(),
                        &to_profile,
                    ));
                }
            }
        }
        let mut outcome = outcome.ok_or_else(|| {
            Error::new(
                ErrorCode::MixInternalError,
                "account switch completed without a result",
            )
        })?;
        if matches!(
            outcome.status.as_str(),
            "already_active" | "already_selected"
        ) {
            return Ok(outcome);
        }
        let activity = if outcome.status == "selected" {
            "environment_selected"
        } else {
            "switch"
        };
        if let Err(error) = self.record_activity(activity, serde_json::to_value(&outcome)?) {
            outcome.warning = Some(match outcome.warning.take() {
                Some(existing) => format!("{existing}; activity log: {error}"),
                None => format!("activity log: {error}"),
            });
        }
        Ok(outcome)
    }

    pub fn recover_interrupted_switch(&self) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        let config = self.store.load()?;
        let journal = SwitchJournal::load(&config.root)?.ok_or_else(|| {
            Error::new(
                ErrorCode::MixNotFound,
                "there is no interrupted switch to recover",
            )
        })?;
        let app = journal.app.clone();
        let from = journal.from_profile.clone();
        ensure_journal_belongs_to_client(&config, &app, &journal)?;
        if journal.is_cleanup_pending() {
            journal.finish_recovery(&config.root, self.vault.as_ref())?;
            return Ok(json!({"status":"cleanup_completed","app":app}));
        }
        let restart_required = journal.restart_required();
        let client = config
            .apps
            .get(&app)
            .ok_or_else(|| Error::not_found("client", &app))?
            .clone();
        ensure_journal_live_dir_matches_client(&config, &app, &journal)?;
        let controller = self.process_controller(&client);
        controller.stop()?;
        journal.restore(&config.root, self.vault.as_ref())?;
        self.store.mutate(|config| {
            let client = config
                .apps
                .get_mut(&app)
                .ok_or_else(|| Error::not_found("client", &app))?;
            client.active_profile = from.clone();
            Ok(())
        })?;
        journal.cleanup_after_restore(&config.root, self.vault.as_ref())?;
        if restart_required {
            if let Err(error) = controller.start() {
                return Ok(json!({
                    "status":"recovered",
                    "app":app,
                    "profile":from,
                    "restart_warning":error.to_string(),
                }));
            }
        }
        Ok(json!({"status":"recovered","app":app,"profile":from}))
    }

    pub fn sync_active_account(&self, app: &str) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        let mut synchronized_profile = None;
        self.mutate_config(|config| {
            let client = config
                .apps
                .get_mut(app)
                .ok_or_else(|| Error::not_found("client", app))?;
            let projection = self
                .adapters
                .get(client.adapter)
                .global_account_projection()
                .ok_or_else(|| {
                    Error::new(
                        ErrorCode::MixUnsupported,
                        "this client does not expose a global account to synchronize",
                    )
                })?;
            let profile = projection
                .synchronize_source(client, self.vault.as_ref())?
                .ok_or_else(|| {
                    Error::new(
                        ErrorCode::MixValidationError,
                        "the client is not signed in to a managed account",
                    )
                })?;
            synchronized_profile = Some(profile);
            client.active_profile = synchronized_profile.clone();
            Ok(())
        })?;
        let mut result = json!({"status":"synchronized","app":app,"profile":synchronized_profile});
        let activity = self.record_activity("account_synchronized", result.clone());
        attach_activity_warning(&mut result, activity);
        Ok(result)
    }

    pub fn sessions(
        &self,
        app: Option<&str>,
        query: Option<&str>,
        adapter: Option<AdapterKind>,
        recovery: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Session>> {
        if !(1..=500).contains(&limit) {
            return Err(Error::new(
                ErrorCode::MixRequestInvalid,
                "session limit must be between 1 and 500",
            ));
        }
        validate_session_query(query)?;
        let config = self.store.load()?;
        if let Some(selected) = app.filter(|selected| *selected != "all") {
            if !config.apps.contains_key(selected) {
                return Err(Error::not_found("client", selected));
            }
        }
        let mut rows = self.session_catalog(&config, app, query, adapter, recovery)?;
        annotate_project_session_counts(&mut rows);
        Ok(rows.into_iter().skip(offset).take(limit).collect())
    }

    pub fn project_sessions(&self) -> Result<Vec<Session>> {
        let config = self.store.load()?;
        let rows = self.session_catalog(&config, None, None, None, None)?;
        let mut projects: BTreeMap<String, (Session, usize)> = BTreeMap::new();
        for session in rows {
            let Some(id) = session
                .project_id
                .as_ref()
                .or(session.project_path.as_ref())
                .or(session.cwd.as_ref())
                .cloned()
            else {
                continue;
            };
            projects
                .entry(id)
                .and_modify(|(_, count)| *count += 1)
                .or_insert((session, 1));
        }
        let mut rows = projects
            .into_values()
            .map(|(mut session, count)| {
                session.project_session_count = Some(count);
                session
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(rows)
    }

    fn session_catalog(
        &self,
        config: &crate::Config,
        app: Option<&str>,
        query: Option<&str>,
        adapter: Option<AdapterKind>,
        recovery: Option<&str>,
    ) -> Result<Vec<Session>> {
        let mut rows = Vec::new();
        for (name, client) in &config.apps {
            if app.is_some_and(|selected| selected != "all" && selected != name) {
                continue;
            }
            if adapter.is_some_and(|selected| selected != client.adapter) {
                continue;
            }
            rows.extend(
                self.adapters
                    .get(client.adapter)
                    .sessions(name, client, query)?,
            );
            for runtime in runtime_directories(&config.root, name)? {
                let profile = client.profiles.get(&runtime.profile);
                let mut runtime_client = client.clone();
                runtime_client.live_dir = runtime.path.clone();
                runtime_client.active_profile = Some(runtime.profile.clone());
                let adapter = self.adapters.get(client.adapter);
                let mut runtime_rows = adapter.sessions(name, &runtime_client, query)?;
                for session in &mut runtime_rows {
                    session.catalog_source = SessionCatalogSource::Runtime;
                    session.project_source = Some(SessionProjectSource::Runtime);
                    session.profile = Some(runtime.profile.clone());
                    let transcript_is_in_runtime = session
                        .transcript
                        .as_deref()
                        .is_some_and(|path| Path::new(path).starts_with(&runtime_client.live_dir));
                    let identity_matches = profile.is_some_and(|profile| {
                        runtime_identity_matches(
                            adapter,
                            profile,
                            &runtime.path,
                            runtime.profile_id.as_deref(),
                            runtime.account_fingerprint.as_deref(),
                        )
                    });
                    if profile.is_none() {
                        session.recoverability = Recoverability::B;
                        session.recovery_reason = Some(SessionRecoveryReason::ProfileMissing);
                    } else if discover_executable(client).is_none() {
                        session.recoverability = Recoverability::B;
                        session.recovery_reason = Some(SessionRecoveryReason::UnsupportedCommand);
                    } else if runtime.path.join(RUNTIME_REVOKE_MARKER).exists() {
                        session.recoverability = Recoverability::B;
                        session.recovery_reason = Some(SessionRecoveryReason::RuntimeChanged);
                    } else if session.cwd != runtime.cwd {
                        session.recoverability = Recoverability::B;
                        session.recovery_reason =
                            Some(SessionRecoveryReason::WorkingDirectoryChanged);
                    } else if session
                        .cwd
                        .as_deref()
                        .is_none_or(|path| !Path::new(path).is_dir())
                    {
                        session.recoverability = Recoverability::B;
                        session.recovery_reason =
                            Some(SessionRecoveryReason::WorkingDirectoryMissing);
                    } else if !transcript_is_in_runtime {
                        session.recoverability = Recoverability::B;
                        session.recovery_reason =
                            Some(SessionRecoveryReason::TranscriptOutsideRuntime);
                    } else if !identity_matches {
                        session.recoverability = Recoverability::B;
                        session.recovery_reason = Some(SessionRecoveryReason::RuntimeChanged);
                    } else if session
                        .cwd
                        .as_deref()
                        .is_some_and(|path| Path::new(path).is_dir())
                        && transcript_is_in_runtime
                    {
                        session.recoverability = Recoverability::A;
                        session.recovery_reason = Some(SessionRecoveryReason::Verified);
                    } else {
                        session.recoverability = Recoverability::B;
                        session.recovery_reason = Some(SessionRecoveryReason::RuntimeChanged);
                    }
                }
                rows.extend(runtime_rows);
            }
        }
        let workspaces = workspaces_from(config);
        for session in &mut rows {
            let Some(workspace) = registered_project_for(&workspaces, session.cwd.as_deref())
            else {
                continue;
            };
            session.workspace = Some(workspace.id.clone());
            session.project_id = Some(workspace.id.clone());
            session.project_name = Some(workspace.name.clone());
            session.project_path = Some(workspace.path.clone());
            session.project_source = Some(SessionProjectSource::Registered);
            session.project_registered = Some(true);
            session.project_exists = Some(workspace.exists);
        }
        if let Some(recovery) = recovery {
            let recovery = match recovery {
                "A" => Recoverability::A,
                "B" => Recoverability::B,
                _ => {
                    return Err(Error::new(
                        ErrorCode::MixRequestInvalid,
                        "session recovery must be A or B",
                    ));
                }
            };
            rows.retain(|session| session.recoverability == recovery);
        }
        rows.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(rows)
    }

    pub fn resume_session(&self, app: &str, locator: &str) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        let config = self.store.load()?;
        self.ensure_no_recovery_for(&config)?;
        let client = config
            .apps
            .get(app)
            .ok_or_else(|| Error::not_found("client", app))?;
        let session = self
            .session_catalog(&config, Some(app), None, Some(client.adapter), None)?
            .into_iter()
            .find(|session| session.resume_id == locator)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::MixSessionUnavailable,
                    "the session changed or no longer exists",
                )
            })?;
        if session.recoverability != Recoverability::A {
            return Err(Error::new(
                ErrorCode::MixSessionUnavailable,
                "the session did not pass safe continuation checks",
            ));
        }
        let mut native_environment_profile = None;
        if session.profile.is_none() {
            let workspace = session.workspace.as_deref().and_then(|id| {
                workspaces_from(&config)
                    .into_iter()
                    .find(|workspace| workspace.id == id)
            });
            if let Some(conflicts) = workspace
                .as_ref()
                .and_then(|workspace| workspace.binding_conflicts.get(app))
            {
                return Err(Error::new(
                    ErrorCode::MixConflict,
                    "resolve the conflicting project account bindings before continuing the session",
                )
                .details(json!({"profiles":conflicts})));
            }
            let expected = workspace
                .as_ref()
                .and_then(|workspace| workspace.bindings.get(app));
            if let Some(expected) = expected {
                let current = self.adapters.get(client.adapter).observed_profile(client);
                if current.as_deref() != Some(expected.as_str()) {
                    return Err(Error::new(
                        ErrorCode::MixConflict,
                        "switch to the account or environment bound to this project before continuing the session",
                    )
                    .details(json!({"expected_profile":expected,"current_profile":current})));
                }
            }
            native_environment_profile = self
                .adapters
                .get(client.adapter)
                .native_session_environment_profile(client, expected.map(String::as_str));
        }
        let mut resume_client = client.clone();
        let mut environment = BTreeMap::new();
        let mut runtime_secret_files = ProjectedSecretFiles::new();
        if let Some(profile_name) = session.profile.as_deref() {
            let profile = client
                .profiles
                .get(profile_name)
                .ok_or_else(|| Error::not_found("account", profile_name))?;
            let adapter = self.adapters.get(client.adapter);
            adapter.validate_profile(client, profile)?;
            validate_profile_runtime(profile)?;
            if let Some(projection) = adapter.global_account_projection() {
                projection.validate_target_credential(client, profile, self.vault.as_ref())?;
            }
            let runtime = Path::new(&session.state_dir);
            validate_scoped_path(runtime, &config.root.join("runtime"))?;
            for (relative, payload) in materialized_profile_files(adapter, client, profile)? {
                let destination = crate::fs::safe_child(runtime, &relative)?;
                atomic_write(&destination, &payload)?;
            }
            // Codex's resume provider override must come from the isolated
            // runtime after its target account overlay has been materialized.
            resume_client.live_dir = runtime.to_path_buf();
            let runtime_secret_paths = profile
                .secret_files
                .keys()
                .map(|relative| crate::fs::safe_child(runtime, relative))
                .collect::<Result<Vec<_>>>()?;
            if !runtime_secret_paths.is_empty() {
                runtime_secret_files = ProjectedSecretFiles::for_runtime(
                    runtime,
                    &config.root.join("runtime"),
                    runtime_secret_paths,
                )?;
            }
            for (relative, reference) in &profile.secret_files {
                let destination = crate::fs::safe_child(runtime, relative)?;
                atomic_write(&destination, &self.vault.get(reference)?)?;
            }
            environment = profile_environment(profile, self.vault.as_ref())?;
        } else if let Some(profile_name) = native_environment_profile.as_deref() {
            let profile = client
                .profiles
                .get(profile_name)
                .ok_or_else(|| Error::not_found("environment", profile_name))?;
            validate_profile_runtime(profile)?;
            environment = profile_environment(profile, self.vault.as_ref())?;
        }
        let argv = self
            .adapters
            .get(client.adapter)
            .resume_argv(&resume_client, &session.id)?;
        launch_terminal(
            &config.root,
            TerminalLaunch {
                argv: &argv,
                variable: self.adapters.get(client.adapter).runtime_variable(),
                state_dir: session.state_dir.clone(),
                cwd: session.cwd.as_deref(),
                environment: &environment,
                completion_path: None,
            },
            runtime_secret_files,
        )?;
        Ok(json!({"status":"launched","app":app,"session":session.id}))
    }

    pub fn open_native(&self, app: &str, cwd: Option<&str>) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        let config = self.store.load()?;
        self.ensure_no_recovery_for(&config)?;
        let client = config
            .apps
            .get(app)
            .ok_or_else(|| Error::not_found("client", app))?;
        let cwd = cwd
            .ok_or_else(|| Error::new(ErrorCode::MixRequestInvalid, "project path is required"))?;
        let path = canonical_workspace(Path::new(cwd))?;
        if !path.is_dir() {
            return Err(Error::new(
                ErrorCode::MixNotFound,
                "project directory does not exist",
            ));
        }
        let argv = self
            .adapters
            .get(client.adapter)
            .open_project_argv(client, &path)?;
        let (executable, arguments) = argv
            .split_first()
            .ok_or_else(|| Error::new(ErrorCode::MixConfigInvalid, "open command is empty"))?;
        let child = Command::new(executable)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| Error::io("cannot open client project", error))?;
        reap_child(child);
        Ok(json!({"status":"opened","cwd":path}))
    }

    pub fn bind_workspace(
        &self,
        path: PathBuf,
        bindings: BTreeMap<String, String>,
        name: Option<String>,
    ) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        let path = canonical_workspace(&path)?;
        if !path.is_dir() {
            return Err(Error::new(
                ErrorCode::MixNotFound,
                "project directory does not exist",
            ));
        }
        let key = path.display().to_string();
        let name = name
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_owned());
        self.mutate_config(|config| {
            for (app, profile) in &bindings {
                let client = config
                    .apps
                    .get(app)
                    .ok_or_else(|| Error::not_found("client", app))?;
                if !client.profiles.contains_key(profile) {
                    return Err(Error::not_found("account", profile));
                }
            }
            for alias in workspace_aliases(config, &path) {
                config.workspace_bindings.remove(&alias);
                config.workspace_meta.remove(&alias);
            }
            config
                .workspace_bindings
                .insert(key.clone(), bindings.clone());
            config
                .workspace_meta
                .insert(key.clone(), WorkspaceMeta { name: name.clone() });
            Ok(())
        })?;
        Ok(json!({"status":"bound","workspace":key}))
    }

    pub fn add_profile(
        &self,
        app: &str,
        requested_name: Option<&str>,
        input: crate::ProfileInput,
    ) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        let mut added_name = None;
        self.mutate_config(|config| {
            let client = config
                .apps
                .get_mut(app)
                .ok_or_else(|| Error::not_found("client", app))?;
            let name = requested_name
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or(next_profile_name(
                    self.adapters
                        .get(client.adapter)
                        .descriptor()
                        .profile_prefix,
                    &client.profiles,
                )?);
            validate_identifier(&name)?;
            if client.profiles.contains_key(&name) {
                return Err(Error::new(
                    ErrorCode::MixConflict,
                    format!("account already exists: {app}/{name}"),
                ));
            }
            let supplied_label = input
                .label
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.trim().to_owned());
            let profile = Profile {
                label: supplied_label.clone().unwrap_or_else(|| name.clone()),
                label_origin: if supplied_label.is_some() {
                    ProfileLabelOrigin::User
                } else {
                    ProfileLabelOrigin::Generated
                },
                files: input.files,
                secret_files: input.secret_files,
                env: input.env,
                secrets: input.secrets,
                auth_strategy: input.auth_strategy,
                ..Profile::default()
            };
            self.adapters
                .get(client.adapter)
                .validate_profile(client, &profile)?;
            validate_profile_runtime(&profile)?;
            for reference in profile
                .secret_files
                .values()
                .chain(profile.secrets.values())
            {
                self.vault.get(reference)?;
            }
            client.profiles.insert(name.clone(), profile);
            if client.active_profile.is_none() {
                client.active_profile = Some(name.clone());
            }
            added_name = Some(name);
            Ok(())
        })?;
        let name = added_name.ok_or_else(|| {
            Error::new(
                ErrorCode::MixInternalError,
                "profile creation completed without a profile name",
            )
        })?;
        let mut result = json!({"status":"added","app":app,"profile":name});
        let activity = self.record_activity("profile_added", result.clone());
        attach_activity_warning(&mut result, activity);
        Ok(result)
    }

    pub fn capture_profile(
        &self,
        app: &str,
        requested_name: Option<&str>,
        files: &[String],
        label: Option<&str>,
    ) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        let mut captured_name = None;
        let mut created_root = None;
        let operation = self.mutate_config(|config| {
            let client = config
                .apps
                .get_mut(app)
                .ok_or_else(|| Error::not_found("client", app))?;
            let name = requested_name
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or(next_profile_name(
                    self.adapters
                        .get(client.adapter)
                        .descriptor()
                        .profile_prefix,
                    &client.profiles,
                )?);
            validate_identifier(&name)?;
            if client.profiles.contains_key(&name) {
                return Err(Error::new(
                    ErrorCode::MixConflict,
                    format!("account already exists: {app}/{name}"),
                ));
            }
            let profile_root = config.root.join("profiles").join(app).join(&name);
            if fs::symlink_metadata(&profile_root).is_ok() {
                return Err(Error::new(
                    ErrorCode::MixConflict,
                    format!("account storage already exists: {app}/{name}"),
                ));
            }
            private_scoped_dir(&profile_root, &config.root.join("profiles"))?;
            created_root = Some(profile_root.clone());
            let mut sources = BTreeMap::new();
            for relative in files {
                let payload = self
                    .adapters
                    .get(client.adapter)
                    .capture_payload(client, relative)?;
                let destination = crate::fs::safe_child(&profile_root, relative)?;
                atomic_write(&destination, &payload)?;
                sources.insert(relative.clone(), destination);
            }
            if sources.is_empty() {
                return Err(Error::invalid("at least one client file is required"));
            }
            let supplied_label = label
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.trim().to_owned());
            let profile = Profile {
                label: supplied_label.clone().unwrap_or_else(|| name.clone()),
                label_origin: if supplied_label.is_some() {
                    ProfileLabelOrigin::User
                } else {
                    ProfileLabelOrigin::Generated
                },
                files: sources,
                ..Profile::default()
            };
            self.adapters
                .get(client.adapter)
                .validate_profile(client, &profile)?;
            validate_profile_runtime(&profile)?;
            client.profiles.insert(name.clone(), profile);
            if client.active_profile.is_none() {
                client.active_profile = Some(name.clone());
            }
            captured_name = Some(name);
            Ok(())
        });
        if let Err(error) = operation {
            return Err(self.failed_operation_with_cleanup(
                error,
                created_root.into_iter().collect(),
                Vec::new(),
            ));
        }
        let name = captured_name.ok_or_else(|| {
            Error::new(
                ErrorCode::MixInternalError,
                "profile capture completed without a profile name",
            )
        })?;
        Ok(json!({"status":"captured","app":app,"profile":name}))
    }

    pub fn run(&self, app: &str, profile: &str, cwd: Option<&str>) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        let config = self.store.load()?;
        self.ensure_no_recovery_for(&config)?;
        let cwd = cwd
            .map(|value| canonical_workspace(Path::new(value)))
            .transpose()?
            .map(|path| path.display().to_string());
        if cwd.as_deref().is_some_and(|path| !Path::new(path).is_dir()) {
            return Err(Error::new(
                ErrorCode::MixNotFound,
                "working directory does not exist",
            ));
        }
        let client = config
            .apps
            .get(app)
            .ok_or_else(|| Error::not_found("client", app))?;
        let adapter = self.adapters.get(client.adapter);
        let profile = resolve_profile_selector(adapter, client, profile)?;
        let selected = client
            .profiles
            .get(&profile)
            .ok_or_else(|| Error::not_found("account", &profile))?;
        adapter.validate_profile(client, selected)?;
        validate_profile_runtime(selected)?;
        if let Some(projection) = adapter.global_account_projection() {
            projection.validate_target_credential(client, selected, self.vault.as_ref())?;
        }
        let executable = discover_executable(client).ok_or_else(|| {
            Error::new(ErrorCode::MixNotFound, "client executable is unavailable")
        })?;
        let environment = profile_environment(selected, self.vault.as_ref())?;
        let runtime = persistent_runtime_path(&config.root, app, &selected.id, cwd.as_deref());
        private_scoped_dir(&runtime, &config.root.join("runtime"))?;
        let revoke_marker = runtime.join(RUNTIME_REVOKE_MARKER);
        if fs::symlink_metadata(&revoke_marker).is_ok() {
            remove_durable(&revoke_marker)?;
        }
        for (relative, payload) in materialized_profile_files(adapter, client, selected)? {
            let destination = crate::fs::safe_child(&runtime, &relative)?;
            atomic_write(&destination, &payload)?;
        }
        let runtime_secret_paths = selected
            .secret_files
            .keys()
            .map(|relative| crate::fs::safe_child(&runtime, relative))
            .collect::<Result<Vec<_>>>()?;
        let runtime_secret_files = if runtime_secret_paths.is_empty() {
            ProjectedSecretFiles::new()
        } else {
            ProjectedSecretFiles::for_runtime(
                &runtime,
                &config.root.join("runtime"),
                runtime_secret_paths,
            )?
        };
        for (relative, reference) in &selected.secret_files {
            let destination = crate::fs::safe_child(&runtime, relative)?;
            let payload = self.vault.get(reference)?;
            atomic_write(&destination, &payload)?;
        }
        crate::fs::atomic_json(
            &runtime.join(".mix-runtime.json"),
            &json!({
                "product":"mix",
                "app":app,
                "profile":&profile,
                "profile_id":selected.id,
                "account_fingerprint":selected.account_fingerprint,
                "cwd":cwd,
                "runtime":runtime,
            }),
        )?;
        let mut argv = client.run.command.clone();
        if argv.is_empty() {
            argv.push(executable.display().to_string());
        } else {
            argv[0] = executable.display().to_string();
        }
        launch_terminal(
            &config.root,
            TerminalLaunch {
                argv: &argv,
                variable: self.adapters.get(client.adapter).runtime_variable(),
                state_dir: runtime.display().to_string(),
                cwd: cwd.as_deref(),
                environment: &environment,
                completion_path: None,
            },
            runtime_secret_files,
        )?;
        let mut result =
            json!({"status":"launched","app":app,"profile":profile,"runtime_dir":runtime});
        let activity = self.record_activity("run", result.clone());
        attach_activity_warning(&mut result, activity);
        Ok(result)
    }

    pub fn switch_back_from_activity(&self, activity_id: &str) -> Result<Value> {
        let activity = self
            .activities(500)?
            .into_iter()
            .find(|activity| activity.id == activity_id)
            .ok_or_else(|| Error::not_found("activity", activity_id))?;
        let app = activity
            .data
            .get("app")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::invalid("activity has no client"))?;
        let profile = activity
            .data
            .get("from_profile")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::invalid("activity has no previous account"))?;
        serde_json::to_value(self.switch(app, profile)?).map_err(Error::from)
    }

    pub fn delete_workspace(&self, workspace: &str) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        let canonical = canonical_workspace(Path::new(workspace))?
            .display()
            .to_string();
        self.mutate_config(|config| {
            let aliases = workspace_aliases(config, Path::new(&canonical));
            let removed = !aliases.is_empty();
            for alias in aliases {
                config.workspace_bindings.remove(&alias);
                config.workspace_meta.remove(&alias);
            }
            if !removed {
                return Err(Error::not_found("workspace", workspace));
            }
            Ok(())
        })?;
        Ok(json!({"status":"deleted","workspace":canonical}))
    }

    pub fn edit_profile(&self, app: &str, profile: &str, label: Option<&str>) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        self.mutate_config(|config| {
            let client = config
                .apps
                .get_mut(app)
                .ok_or_else(|| Error::not_found("client", app))?;
            let value = client
                .profiles
                .get_mut(profile)
                .ok_or_else(|| Error::not_found("account", profile))?;
            if let Some(label) = label.filter(|value| !value.trim().is_empty()) {
                value.label = label.trim().into();
                value.label_origin = ProfileLabelOrigin::User;
            }
            Ok(())
        })?;
        Ok(json!({"status":"updated","app":app,"profile":profile}))
    }

    pub fn delete_profile(
        &self,
        app: &str,
        profile: &str,
        detach_workspaces: bool,
    ) -> Result<Value> {
        let _guard = self.lock_mutation()?;
        self.mutate_config(|config| {
            let client = config
                .apps
                .get_mut(app)
                .ok_or_else(|| Error::not_found("client", app))?;
            let removing_active = self
                .adapters
                .get(client.adapter)
                .observed_profile(client)
                .as_deref()
                == Some(profile);
            if removing_active && client.profiles.len() > 1 {
                return Err(Error::new(
                    ErrorCode::MixConflict,
                    "switch to another account or environment before removing the active one",
                ));
            }
            let bound = config
                .workspace_bindings
                .iter()
                .filter(|(_, bindings)| bindings.get(app).is_some_and(|value| value == profile))
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            if !bound.is_empty() && !detach_workspaces {
                return Err(Error::new(
                    ErrorCode::MixConflict,
                    "account is still used by projects",
                )
                .details(json!({"workspaces":bound})));
            }
            for path in bound {
                if let Some(bindings) = config.workspace_bindings.get_mut(&path) {
                    bindings.remove(app);
                }
            }
            let selected = client
                .profiles
                .get(profile)
                .ok_or_else(|| Error::not_found("account", profile))?;
            let managed_files = selected
                .files
                .keys()
                .chain(selected.secret_files.keys())
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let removed = client
                .profiles
                .remove(profile)
                .ok_or_else(|| Error::not_found("account", profile))?;
            config
                .pending_cleanup
                .profile_directories
                .push(config.root.join("profiles").join(app).join(profile));
            config
                .pending_cleanup
                .credentials
                .extend(removed.secret_files.values().cloned());
            config
                .pending_cleanup
                .credentials
                .extend(removed.secrets.values().cloned());
            config
                .pending_cleanup
                .runtime_revocations
                .push(PendingRuntimeRevocation {
                    app: app.into(),
                    profile: profile.into(),
                    profile_id: removed.id,
                    files: managed_files,
                });
            config.pending_cleanup.profile_directories.sort();
            config.pending_cleanup.profile_directories.dedup();
            config.pending_cleanup.credentials.sort();
            config.pending_cleanup.credentials.dedup();
            config.pending_cleanup.runtime_revocations.sort();
            config.pending_cleanup.runtime_revocations.dedup();
            if client.active_profile.as_deref() == Some(profile) {
                client.active_profile = None;
            }
            Ok(())
        })?;
        let warnings = self.finish_pending_cleanup()?;
        let mut result = json!({"status":"deleted","app":app,"profile":profile});
        if !warnings.is_empty() {
            result["warnings"] = json!(warnings);
        }
        Ok(result)
    }

    pub fn diagnostics(&self) -> Result<Value> {
        let config = self.store.load()?;
        let state = self.state()?;
        let clients = config
            .apps
            .values()
            .enumerate()
            .map(|(index, client)| {
                let current = state.apps.iter().find(|app| {
                    app.adapter == client.adapter && app.live_dir == client.live_dir
                });
                let account = current.map(|app| &app.account_switch);
                let credential_refs = client
                    .profiles
                    .values()
                    .flat_map(|profile| {
                        profile
                            .secret_files
                            .values()
                            .chain(profile.secrets.values())
                    });
                let mut missing_credentials = 0usize;
                let mut credential_checks_failed = 0usize;
                for reference in credential_refs {
                    match self.vault.contains(reference) {
                        Ok(true) => {}
                        Ok(false) => missing_credentials += 1,
                        Err(_) => credential_checks_failed += 1,
                    }
                }
                let process = match self.process_controller(client).matching_pids() {
                    Ok(pids) => json!({
                        "status": "known",
                        "running": !pids.is_empty(),
                        "managed_process_count": pids.len(),
                    }),
                    Err(error) => json!({
                        "status": "unknown",
                        "error_code": error.code.as_str(),
                    }),
                };
                let route = if client.adapter == AdapterKind::Codex {
                    json!({
                        "provider": account.and_then(|value| value.provider.clone()),
                        "endpoint_host": CodexAdapter::provider_endpoint_host(client).ok().flatten(),
                    })
                } else {
                    json!({
                        "provider": account.and_then(|value| value.provider.clone()),
                    })
                };
                json!({
                    "id": format!("client_{}", index + 1),
                    "adapter": client.adapter,
                    "status": current.map(|app| app.status),
                    "profile_count": client.profiles.len(),
                    "issue_count": current.map_or(0, |app| app.issues.len()),
                    "command_found": current.is_some_and(|app| app.command_found),
                    "managed_profile_count": client.profiles.values().filter(|profile| profile.account_fingerprint.is_some()).count(),
                    "active_account_managed": account.and_then(|value| value.current_profile.as_ref()).is_some(),
                    "account_switch_status": account.map(|value| value.status),
                    "native_files": {
                        "config": diagnostic_file_state(&client.live_dir.join("config.toml")),
                        "auth": diagnostic_file_state(&client.live_dir.join("auth.json")),
                        "auth_kind": (client.adapter == AdapterKind::Codex).then(|| codex_auth_kind(client)),
                    },
                    "route": route,
                    "credentials": {
                        "missing_saved": missing_credentials,
                        "checks_failed": credential_checks_failed,
                    },
                    "process": process,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "product":{
                "name":"mix",
                "version":env!("CARGO_PKG_VERSION"),
            },
            "platform":{
                "os":std::env::consts::OS,
                "arch":std::env::consts::ARCH,
            },
            "config_schema": crate::CONFIG_VERSION,
            "client_count": state.apps.len(),
            "workspace_count": state.workspaces.len(),
            "health": state.health.status,
            "credential_store": state.security.credential_store,
            "recovery_required": state.recovery.interrupted_switch.required,
            "clients": clients,
        }))
    }

    pub fn activities(&self, limit: usize) -> Result<Vec<Activity>> {
        let config = self.store.load()?;
        let lock = open_lock(&config.root.join(".activity.lock"))?;
        lock.lock_shared()
            .map_err(|error| Error::io("cannot lock Mix activity", error))?;
        let mut rows = Vec::new();
        for name in ["activity.jsonl.1", "activity.jsonl"] {
            let path = config.root.join(name);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    return Err(Error::new(
                        ErrorCode::MixLocalFailure,
                        "Mix activity path is not a regular file",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(Error::io("cannot inspect Mix activity", error)),
            }
            let payload = read_bounded(&path, 5 * 1024 * 1024, "cannot read Mix activity")?;
            rows.extend(
                payload
                    .split(|byte| *byte == b'\n')
                    .filter(|line| !line.is_empty())
                    .filter_map(|line| serde_json::from_slice::<Activity>(line).ok()),
            );
        }
        rows.reverse();
        rows.truncate(limit);
        Ok(rows)
    }

    fn record_activity(&self, kind: &str, value: Value) -> Result<()> {
        let config = self.store.load()?;
        private_dir(&config.root)?;
        let lock = open_lock(&config.root.join(".activity.lock"))?;
        lock.lock_exclusive()
            .map_err(|error| Error::io("cannot lock Mix activity", error))?;
        let path = config.root.join("activity.jsonl");
        if path
            .metadata()
            .is_ok_and(|metadata| metadata.len() >= 5 * 1024 * 1024)
        {
            let archive = config.root.join("activity.jsonl.1");
            let _ = fs::remove_file(&archive);
            fs::rename(&path, archive)
                .map_err(|error| Error::io("cannot rotate Mix activity", error))?;
            crate::fs::sync_directory(&config.root)?;
        }
        let mut data = match value {
            Value::Object(data) => data,
            _ => Default::default(),
        };
        data.remove("details");
        let activity = Activity {
            id: Uuid::new_v4().to_string(),
            kind: kind.into(),
            at: chrono::Utc::now().to_rfc3339(),
            data: data.into_iter().collect(),
        };
        let mut payload = serde_json::to_vec(&activity)?;
        payload.push(b'\n');
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(Error::new(
                    ErrorCode::MixLocalFailure,
                    "Mix activity path is not a regular file",
                ));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(path)
            .map_err(|error| Error::io("cannot open Mix activity", error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| Error::io("cannot protect Mix activity", error))?;
        }
        file.write_all(&payload)
            .map_err(|error| Error::io("cannot append Mix activity", error))?;
        file.sync_data()
            .map_err(|error| Error::io("cannot flush Mix activity", error))
    }

    fn mutate_config<T>(
        &self,
        operation: impl FnOnce(&mut crate::Config) -> Result<T>,
    ) -> Result<T> {
        self.store.mutate(|config| {
            self.ensure_no_recovery_for(config)?;
            operation(config)
        })
    }

    fn finish_pending_cleanup(&self) -> Result<Vec<String>> {
        let config = self.store.load()?;
        if config.pending_cleanup.is_empty() {
            return Ok(Vec::new());
        }
        let used_credentials = config
            .apps
            .values()
            .flat_map(|client| client.profiles.values())
            .flat_map(|profile| {
                profile
                    .secret_files
                    .values()
                    .chain(profile.secrets.values())
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        let used_files = config
            .apps
            .values()
            .flat_map(|client| client.profiles.values())
            .flat_map(|profile| profile.files.values())
            .collect::<Vec<_>>();
        let mut remaining_directories = Vec::new();
        let mut remaining_credentials = Vec::new();
        let mut remaining_revocations = Vec::new();
        let mut warnings = Vec::new();

        for revocation in &config.pending_cleanup.runtime_revocations {
            if config.apps.get(&revocation.app).is_some_and(|client| {
                client
                    .profiles
                    .get(&revocation.profile)
                    .is_some_and(|profile| profile.id == revocation.profile_id)
            }) {
                continue;
            }
            if let Err(error) = revoke_runtime_profile_files(
                &config.root.join("runtime").join(&revocation.app),
                &revocation.profile,
                Some(&revocation.profile_id),
                &revocation.files,
            ) {
                remaining_revocations.push(revocation.clone());
                warnings.push(error.to_string());
            }
        }
        for path in &config.pending_cleanup.profile_directories {
            if used_files.iter().any(|source| source.starts_with(path)) {
                continue;
            }
            if let Err(error) = remove_mix_directory(path, &config.root) {
                remaining_directories.push(path.clone());
                warnings.push(error.to_string());
            }
        }
        for reference in &config.pending_cleanup.credentials {
            if used_credentials.contains(reference) {
                continue;
            }
            if let Err(error) = self.vault.delete(reference) {
                remaining_credentials.push(reference.clone());
                warnings.push(error.to_string());
            }
        }

        if let Err(error) = self.store.mutate(|config| {
            config.pending_cleanup.profile_directories = remaining_directories.clone();
            config.pending_cleanup.credentials = remaining_credentials.clone();
            config.pending_cleanup.runtime_revocations = remaining_revocations.clone();
            Ok(())
        }) {
            warnings.push(format!("cannot persist pending cleanup state: {error}"));
        }
        Ok(warnings)
    }

    fn failed_operation_with_cleanup(
        &self,
        error: Error,
        directories: Vec<PathBuf>,
        credentials: Vec<SecretRef>,
    ) -> Error {
        if directories.is_empty() && credentials.is_empty() {
            return error;
        }
        let scheduled = self.store.mutate(|config| {
            config
                .pending_cleanup
                .profile_directories
                .extend(directories);
            config.pending_cleanup.credentials.extend(credentials);
            config.pending_cleanup.profile_directories.sort();
            config.pending_cleanup.profile_directories.dedup();
            config.pending_cleanup.credentials.sort();
            config.pending_cleanup.credentials.dedup();
            Ok(())
        });
        match scheduled {
            Ok(()) => match self.finish_pending_cleanup() {
                Ok(warnings) if warnings.is_empty() => error,
                Ok(warnings) => error.details(json!({
                    "cleanup_pending": true,
                    "cleanup_errors": warnings,
                })),
                Err(cleanup) => Error::new(
                    error.code,
                    format!("{error}; cleanup could not be completed: {cleanup}"),
                ),
            },
            Err(cleanup) => Error::new(
                error.code,
                format!("{error}; cleanup state could not be persisted: {cleanup}"),
            ),
        }
    }

    fn lock_mutation(&self) -> Result<MutationGuard<'_>> {
        let local = self.mutation.lock();
        let root = self.store.root()?;
        private_dir(root)?;
        let cross_process = open_lock(&root.join(".mix-resource.lock"))?;
        cross_process
            .lock_exclusive()
            .map_err(|error| Error::io("cannot lock Mix resources", error))?;
        Ok(MutationGuard {
            _local: local,
            _cross_process: cross_process,
        })
    }

    fn ensure_no_recovery_for(&self, config: &crate::Config) -> Result<()> {
        if SwitchJournal::path(&config.root).exists() {
            Err(Error::new(
                ErrorCode::MixSwitchRecoveryRequired,
                "an interrupted switch must be recovered first",
            ))
        } else {
            Ok(())
        }
    }
}

fn validate_captured_account(account: &CapturedAccount) -> Result<()> {
    if account.fingerprint.is_empty()
        || account.fingerprint.len() > 512
        || account.fingerprint.chars().any(char::is_control)
    {
        return Err(Error::new(
            ErrorCode::MixValidationError,
            "the captured account has no valid stable identity",
        ));
    }
    if account
        .files
        .keys()
        .any(|relative| account.secret_files.contains_key(relative))
    {
        return Err(Error::new(
            ErrorCode::MixValidationError,
            "captured account files and credentials must use distinct client paths",
        ));
    }
    Ok(())
}

fn materialize_captured_profile(
    service: &MixService,
    client: &ClientConfig,
    profile_root: &Path,
    account: &CapturedAccount,
    label: String,
    label_origin: ProfileLabelOrigin,
    created_references: &mut Vec<SecretRef>,
) -> Result<Profile> {
    validate_captured_account(account)?;
    let mut files = BTreeMap::new();
    for (relative, payload) in &account.files {
        let destination = safe_child(profile_root, relative)?;
        atomic_write(&destination, payload)?;
        files.insert(relative.clone(), destination);
    }
    let mut secret_files = BTreeMap::new();
    for (relative, payload) in &account.secret_files {
        safe_child(profile_root, relative)?;
        let reference = SecretRef {
            service: "com.mix.client-auth".into(),
            account: Uuid::new_v4().to_string(),
        };
        service.vault.set(&reference, payload)?;
        created_references.push(reference.clone());
        secret_files.insert(relative.clone(), reference);
    }
    let profile = Profile {
        label,
        label_origin,
        files,
        secret_files,
        auth_strategy: Some(crate::AuthStrategy::LocalFile),
        account_fingerprint: Some(account.fingerprint.clone()),
        account_identity: Some(account.identity.clone()),
        ..Profile::default()
    };
    service
        .adapters
        .get(client.adapter)
        .validate_profile(client, &profile)?;
    Ok(profile)
}

fn validate_identifier(value: &str) -> Result<()> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(Error::invalid("identifier is empty"));
    };
    if value.len() > 64
        || !first.is_ascii_alphanumeric()
        || chars.any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')))
    {
        Err(Error::invalid(format!("unsupported identifier: {value}")))
    } else {
        Ok(())
    }
}

fn validate_session_query(query: Option<&str>) -> Result<()> {
    if query.is_some_and(|value| value.len() > 512 || value.chars().any(char::is_control)) {
        return Err(Error::new(
            ErrorCode::MixRequestInvalid,
            "session query is invalid",
        ));
    }
    Ok(())
}

fn attach_activity_warning(result: &mut Value, activity: Result<()>) {
    if let Err(error) = activity {
        result["activity_warning"] = Value::String(error.to_string());
    }
}

fn diagnostic_file_state(path: &Path) -> &'static str {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => "symlink",
        Ok(metadata) if metadata.is_file() => "regular",
        Ok(_) => "other",
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing",
        Err(_) => "unreadable",
    }
}

fn codex_auth_kind(client: &ClientConfig) -> &'static str {
    if diagnostic_file_state(&client.live_dir.join("auth.json")) != "regular" {
        return "missing_or_invalid_file";
    }
    let Ok(document) = CodexAdapter::auth_document(client) else {
        return "invalid_json";
    };
    if document.get("auth_mode").and_then(Value::as_str) == Some("chatgpt")
        && document
            .get("tokens")
            .and_then(Value::as_object)
            .is_some_and(|tokens| {
                tokens
                    .get("account_id")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            })
    {
        return "chatgpt";
    }
    if document
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
    {
        return "api_key";
    }
    "unknown"
}

fn stop_before_recovery(controller: &ProcessController) -> Result<()> {
    controller.stop().map(|_| ()).map_err(|error| {
        Error::new(
            ErrorCode::MixSwitchRecoveryFailed,
            format!(
                "the client could not be stopped safely; the recovery journal was preserved: {error}"
            ),
        )
    })
}

fn ensure_journal_belongs_to_client(
    config: &crate::Config,
    app: &str,
    journal: &SwitchJournal,
) -> Result<()> {
    if journal.app != app {
        return Err(Error::new(
            ErrorCode::MixSwitchRecoveryFailed,
            "the interrupted switch does not belong to the requested client",
        ));
    }
    if !config.apps.contains_key(app) {
        return Err(Error::not_found("client", app));
    }
    Ok(())
}

fn ensure_journal_live_dir_matches_client(
    config: &crate::Config,
    app: &str,
    journal: &SwitchJournal,
) -> Result<()> {
    ensure_journal_belongs_to_client(config, app, journal)?;
    let client = config
        .apps
        .get(app)
        .ok_or_else(|| Error::not_found("client", app))?;
    if journal.live_dir() != client.live_dir {
        return Err(Error::new(
            ErrorCode::MixSwitchRecoveryFailed,
            "the interrupted switch no longer matches the configured client directory",
        ));
    }
    Ok(())
}

fn rolled_back_error(
    cause: Error,
    cleanup: Option<Error>,
    from_profile: Option<&str>,
    to_profile: &str,
) -> Error {
    let cause_code = cause.code.as_str();
    let cleanup_code = cleanup.as_ref().map(|error| error.code.as_str());
    Error::new(
        ErrorCode::MixSwitchRolledBack,
        format!("account switch failed and was rolled back: {cause}"),
    )
    .details(json!({
        "cause_code": cause_code,
        "cleanup_pending": cleanup.is_some(),
        "cleanup_code": cleanup_code,
        "from_profile": from_profile,
        "to_profile": to_profile,
    }))
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(Error::invalid("path must be absolute"))
    }
}

fn next_profile_name(prefix: &str, profiles: &BTreeMap<String, Profile>) -> Result<String> {
    for index in 1..=u64::MAX {
        let name = format!("{prefix}-{index}");
        if !profiles.contains_key(&name) {
            return Ok(name);
        }
    }
    Err(Error::new(
        ErrorCode::MixConflict,
        "no account identifier is available",
    ))
}

fn persistent_runtime_path(root: &Path, app: &str, profile_id: &str, cwd: Option<&str>) -> PathBuf {
    let scope = cwd.unwrap_or("default");
    let digest = Sha256::digest(scope.as_bytes());
    root.join("runtime")
        .join(app)
        .join(profile_id)
        .join(format!("{:x}", digest))
}

fn runtime_identity_matches(
    adapter: &dyn Adapter,
    profile: &Profile,
    runtime: &Path,
    marker_profile_id: Option<&str>,
    marker_fingerprint: Option<&str>,
) -> bool {
    if marker_profile_id != Some(profile.id.as_str()) {
        return false;
    }
    adapter.runtime_identity_matches(profile, runtime, marker_fingerprint)
}

fn runtime_directories(root: &Path, app: &str) -> Result<Vec<RuntimeDirectory>> {
    let app_root = root.join("runtime").join(app);
    validate_scoped_path(&app_root, &root.join("runtime"))?;
    match fs::symlink_metadata(&app_root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(Error::new(
                ErrorCode::MixLocalFailure,
                "Mix runtime storage is not a regular directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::io("cannot inspect Mix runtime directory", error)),
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&app_root)
        .map_err(|error| Error::io("cannot read Mix runtime directory", error))?
        .filter_map(std::result::Result::ok)
    {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        if path.join(".mix-runtime.json").is_file() {
            candidates.push(path);
            continue;
        }
        for nested in fs::read_dir(path)
            .map_err(|error| Error::io("cannot read Mix runtime directory", error))?
            .filter_map(std::result::Result::ok)
        {
            let nested_path = nested.path();
            let Ok(metadata) = fs::symlink_metadata(&nested_path) else {
                continue;
            };
            if !metadata.file_type().is_symlink()
                && metadata.is_dir()
                && fs::symlink_metadata(nested_path.join(".mix-runtime.json"))
                    .is_ok_and(|marker| marker.is_file() && !marker.file_type().is_symlink())
            {
                candidates.push(nested_path);
            }
        }
    }
    let mut runtimes = Vec::new();
    for path in candidates {
        let payload = match read_bounded(
            &path.join(".mix-runtime.json"),
            64 * 1024,
            "cannot read Mix runtime marker",
        ) {
            Ok(payload) => payload,
            Err(_) => continue,
        };
        let marker: Value = match serde_json::from_slice(&payload) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if marker.get("product").and_then(Value::as_str) != Some("mix")
            || marker.get("app").and_then(Value::as_str) != Some(app)
            || marker.get("runtime").and_then(Value::as_str)
                != Some(path.to_string_lossy().as_ref())
        {
            continue;
        }
        let Some(profile) = marker.get("profile").and_then(Value::as_str) else {
            continue;
        };
        runtimes.push(RuntimeDirectory {
            path,
            profile: profile.to_owned(),
            profile_id: marker
                .get("profile_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            cwd: marker.get("cwd").and_then(Value::as_str).map(str::to_owned),
            account_fingerprint: marker
                .get("account_fingerprint")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    Ok(runtimes)
}

fn remove_mix_directory(path: &Path, root: &Path) -> Result<()> {
    let profiles_root = root.join("profiles");
    remove_owned_directory(path, &profiles_root, "Mix profile storage")
}

fn revoke_runtime_profile_files(
    app_root: &Path,
    profile: &str,
    profile_id: Option<&str>,
    relative_paths: &[String],
) -> Result<()> {
    let markers = runtime_marker_paths(app_root, profile, profile_id)?;
    for marker in markers {
        let Some(runtime) = marker.parent() else {
            continue;
        };
        crate::fs::atomic_json(
            &runtime.join(RUNTIME_REVOKE_MARKER),
            &json!({"files":relative_paths}),
        )?;
        for relative in relative_paths {
            remove_durable(&crate::fs::safe_child(runtime, relative)?)?;
        }
    }
    Ok(())
}

fn prune_revoked_runtime_files(config: &crate::Config) -> Result<()> {
    for app in config.apps.keys() {
        for runtime in runtime_directories(&config.root, app)? {
            let marker = runtime.path.join(RUNTIME_REVOKE_MARKER);
            let metadata = match fs::symlink_metadata(&marker) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(Error::io("cannot inspect revoked runtime files", error));
                }
            };
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(Error::new(
                    ErrorCode::MixLocalFailure,
                    "runtime revocation marker is not a regular file",
                ));
            }
            let payload =
                read_bounded(&marker, 64 * 1024, "cannot read runtime revocation marker")?;
            let value: Value = serde_json::from_slice(&payload)?;
            let files = value
                .get("files")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::invalid("invalid runtime revocation marker"))?;
            for relative in files {
                let relative = relative
                    .as_str()
                    .ok_or_else(|| Error::invalid("invalid runtime revocation path"))?;
                remove_durable(&crate::fs::safe_child(&runtime.path, relative)?)?;
            }
        }
    }
    Ok(())
}

fn prune_inactive_runtime_credentials(config: &crate::Config) -> Result<()> {
    for (app, client) in &config.apps {
        for runtime in runtime_directories(&config.root, app)? {
            let Some(profile) = client.profiles.get(&runtime.profile) else {
                continue;
            };
            let directory = runtime.path.join(RUNTIME_LEASE_DIRECTORY);
            let metadata = match fs::symlink_metadata(&directory) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(Error::io("cannot inspect runtime credential leases", error))
                }
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::new(
                    ErrorCode::MixLocalFailure,
                    "runtime credential lease storage is not a regular directory",
                ));
            }
            let lock = open_lock(&runtime.path.join(RUNTIME_CREDENTIAL_LOCK))?;
            lock.lock_exclusive()
                .map_err(|error| Error::io("cannot lock runtime credentials", error))?;
            prune_runtime_credential_leases(&directory)?;
            if runtime_lease_directory_is_empty(&directory)? {
                for relative in profile.secret_files.keys() {
                    remove_durable(&crate::fs::safe_child(&runtime.path, relative)?)?;
                }
            }
        }
    }
    Ok(())
}

fn remove_owned_directory(path: &Path, allowed_root: &Path, label: &'static str) -> Result<()> {
    if !path.is_absolute()
        || !allowed_root.is_absolute()
        || !path.starts_with(allowed_root)
        || path == allowed_root
    {
        return Err(Error::new(
            ErrorCode::MixValidationError,
            format!("refusing to remove a path outside {label}"),
        ));
    }
    for ancestor in path.ancestors() {
        if !ancestor.starts_with(allowed_root) {
            break;
        }
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(Error::new(
                    ErrorCode::MixLocalFailure,
                    format!("{label} traverses a non-directory path"),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io("cannot inspect Mix storage", error)),
        }
        if ancestor == allowed_root {
            break;
        }
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(Error::new(
            ErrorCode::MixLocalFailure,
            format!("{label} is not a regular directory"),
        )),
        Ok(_) => {
            fs::remove_dir_all(path)
                .map_err(|error| Error::io("cannot remove Mix storage", error))?;
            crate::fs::sync_directory(path.parent().unwrap_or(allowed_root))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io("cannot inspect Mix storage", error)),
    }
}

fn runtime_marker_paths(
    app_root: &Path,
    profile: &str,
    profile_id: Option<&str>,
) -> Result<Vec<PathBuf>> {
    let runtime_root = app_root
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| Error::invalid("Mix runtime path has no controlled root"))?;
    validate_scoped_path(app_root, runtime_root)?;
    let metadata = match fs::symlink_metadata(app_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::io("cannot inspect Mix runtime storage", error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::new(
            ErrorCode::MixLocalFailure,
            "Mix runtime storage is not a regular directory",
        ));
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(app_root)
        .map_err(|error| Error::io("cannot read Mix runtime storage", error))?
        .filter_map(std::result::Result::ok)
    {
        let profile_root = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&profile_root) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let mut candidates = vec![profile_root.join(".mix-runtime.json")];
        for nested in fs::read_dir(&profile_root)
            .map_err(|error| Error::io("cannot read Mix runtime storage", error))?
            .filter_map(std::result::Result::ok)
        {
            let nested = nested.path();
            let Ok(metadata) = fs::symlink_metadata(&nested) else {
                continue;
            };
            if !metadata.file_type().is_symlink() && metadata.is_dir() {
                candidates.push(nested.join(".mix-runtime.json"));
            }
        }
        for marker in candidates {
            let Ok(payload) = read_bounded(&marker, 64 * 1024, "cannot read Mix runtime marker")
            else {
                continue;
            };
            let Ok(value) = serde_json::from_slice::<Value>(&payload) else {
                continue;
            };
            let Some(parent) = marker.parent() else {
                continue;
            };
            validate_scoped_path(parent, runtime_root)?;
            if value.get("product").and_then(Value::as_str) == Some("mix")
                && value.get("profile").and_then(Value::as_str) == Some(profile)
                && profile_id.is_none_or(|expected| {
                    value.get("profile_id").and_then(Value::as_str) == Some(expected)
                })
                && value.get("runtime").and_then(Value::as_str)
                    == Some(parent.to_string_lossy().as_ref())
            {
                paths.push(marker);
            }
        }
    }
    Ok(paths)
}

fn resolve_profile_selector(
    adapter: &dyn Adapter,
    client: &ClientConfig,
    selector: &str,
) -> Result<String> {
    if client.profiles.contains_key(selector) {
        return Ok(selector.to_owned());
    }
    let matching = client
        .profiles
        .iter()
        .filter_map(|(name, profile)| {
            let provider = adapter.profile_provider(profile).ok().flatten();
            (adapter.profile_display_label(
                profile,
                profile.account_identity.as_ref(),
                provider.as_ref(),
            ) == selector)
                .then_some(name.as_str())
        })
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [name] => Ok((*name).to_owned()),
        [] => Err(Error::not_found("account or environment", selector)),
        names => Err(Error::new(
            ErrorCode::MixConflict,
            format!(
                "more than one account or environment is named {selector:?}; use one of these ids: {}",
                names.join(", ")
            ),
        )),
    }
}

fn interrupted(root: &Path) -> InterruptedSwitch {
    match SwitchJournal::load(root) {
        Ok(Some(journal)) => InterruptedSwitch {
            required: true,
            status: Some(if journal.is_cleanup_pending() {
                "cleanup_pending".into()
            } else {
                "pending".into()
            }),
            id: Some(journal.id),
            app: Some(journal.app),
            from_profile: journal.from_profile,
            to_profile: Some(journal.to_profile),
            created_at: Some(journal.created_at),
            error: None,
        },
        Ok(None) => InterruptedSwitch::default(),
        Err(error) => InterruptedSwitch {
            required: true,
            status: Some("invalid".into()),
            error: Some(error.to_string()),
            ..InterruptedSwitch::default()
        },
    }
}

fn enrollment_directory(root: &Path) -> PathBuf {
    root.join("transactions/enrollments")
}

fn enrollment_path(root: &Path, id: &str) -> Result<PathBuf> {
    validate_identifier(id)?;
    Ok(enrollment_directory(root).join(format!("{id}.json")))
}

fn write_enrollment(root: &Path, record: &EnrollmentRecord) -> Result<()> {
    validate_enrollment_record(root, record, &record.id)?;
    let directory = enrollment_directory(root);
    private_scoped_dir(&directory, &root.join("transactions"))?;
    let result = atomic_write(
        &enrollment_path(root, &record.id)?,
        &serde_json::to_vec_pretty(record)?,
    );
    match result {
        Ok(()) => Ok(()),
        Err(_) if read_enrollment(root, &record.id).ok().as_ref() == Some(record) => Ok(()),
        Err(error) => Err(error),
    }
}

fn read_enrollment(root: &Path, id: &str) -> Result<EnrollmentRecord> {
    let path = enrollment_path(root, id)?;
    let payload = read_bounded(&path, 64 * 1024, "cannot read Codex enrollment")?;
    let record: EnrollmentRecord = serde_json::from_slice(&payload).map_err(|_| {
        Error::new(
            ErrorCode::MixLocalFailure,
            "the Codex enrollment record is invalid",
        )
    })?;
    validate_enrollment_record(root, &record, id)?;
    Ok(record)
}

fn validate_enrollment_record(root: &Path, record: &EnrollmentRecord, id: &str) -> Result<()> {
    let invalid = || {
        Error::new(
            ErrorCode::MixLocalFailure,
            "the Codex enrollment record is invalid",
        )
    };
    validate_identifier(id).map_err(|_| invalid())?;
    validate_identifier(&record.id).map_err(|_| invalid())?;
    validate_identifier(&record.app).map_err(|_| invalid())?;
    if let Some(profile) = record.profile.as_deref() {
        validate_identifier(profile).map_err(|_| invalid())?;
    }
    if let Some(profile) = record.repair_profile.as_deref() {
        validate_identifier(profile).map_err(|_| invalid())?;
    }
    let mode_is_valid = match record.mode.as_str() {
        "add" => record.repair_profile.is_none(),
        "repair" => record.repair_profile.is_some(),
        _ => false,
    };
    let state_is_valid = match record.state.as_str() {
        "pending" => record.profile.is_none(),
        "completed" => record.profile.is_some(),
        "failed" => true,
        _ => false,
    };
    let timestamp_is_valid = chrono::DateTime::parse_from_rfc3339(&record.created_at).is_ok();
    if record.version != ENROLLMENT_VERSION
        || record.id != id
        || record.temp_dir != enrollment_directory(root).join(id)
        || !mode_is_valid
        || !state_is_valid
        || !timestamp_is_valid
    {
        return Err(invalid());
    }
    Ok(())
}

fn pending_enrollment(root: &Path, app: &str) -> Result<Option<EnrollmentRecord>> {
    prune_enrollments(root)?;
    let directory = enrollment_directory(root);
    if !directory.is_dir() {
        return Ok(None);
    }
    for entry in fs::read_dir(directory)
        .map_err(|error| Error::io("cannot inspect Codex enrollments", error))?
        .filter_map(std::result::Result::ok)
    {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let Ok(record) = read_enrollment(root, id) else {
            continue;
        };
        if record.app == app && record.state == "pending" && !enrollment_expired(&record) {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

fn enrollment_terminal_finished(record: &EnrollmentRecord) -> Result<bool> {
    let path = record.temp_dir.join(".mix-login-exit");
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(Error::new(
                ErrorCode::MixLocalFailure,
                "Codex sign-in completion marker is invalid",
            ))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::io(
            "cannot inspect Codex sign-in completion marker",
            error,
        )),
    }
}

fn prune_enrollments(root: &Path) -> Result<()> {
    let directory = enrollment_directory(root);
    validate_scoped_path(&directory, &root.join("transactions"))?;
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::io("cannot inspect Codex enrollments", error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::new(
            ErrorCode::MixLocalFailure,
            "Codex enrollment storage is not a regular directory",
        ));
    }
    for entry in fs::read_dir(&directory)
        .map_err(|error| Error::io("cannot inspect Codex enrollments", error))?
    {
        let entry = entry.map_err(|error| Error::io("cannot inspect Codex enrollment", error))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let Ok(record) = read_enrollment(root, id) else {
            continue;
        };
        let expired_pending = record.state == "pending" && enrollment_expired(&record);
        let expired_record = record.state != "pending"
            && enrollment_older_than(&record, ENROLLMENT_RECORD_RETENTION);
        if expired_pending || expired_record {
            remove_enrollment(root, &record)?;
            remove_durable(&path)?;
        }
    }
    Ok(())
}

fn enrollment_expired(record: &EnrollmentRecord) -> bool {
    enrollment_older_than(record, ENROLLMENT_TIMEOUT)
}

fn enrollment_older_than(record: &EnrollmentRecord, age: Duration) -> bool {
    chrono::DateTime::parse_from_rfc3339(&record.created_at)
        .map(|created| {
            chrono::Utc::now().signed_duration_since(created.with_timezone(&chrono::Utc))
                > chrono::Duration::from_std(age).unwrap_or_default()
        })
        .unwrap_or(true)
}

fn enrollment_value(record: &EnrollmentRecord) -> Value {
    json!({
        "enrollment_id": record.id,
        "state": record.state,
        "mode": record.mode,
        "profile": record.profile,
        "repair_profile": record.repair_profile,
        "error": record.error,
        "code": record.code,
    })
}

fn finish_enrollment(root: &Path, record: &mut EnrollmentRecord) -> Result<()> {
    write_enrollment(root, record)?;
    if record.state != "pending" {
        if let Err(error) = cleanup_enrollment_record(root, record) {
            record.error = Some(format!("cleanup pending: {error}"));
            // The enrollment result is already durable. Keep its completed
            // status queryable and expose cleanup as a warning instead of
            // turning a successful account addition into a false failure.
            write_enrollment(root, record)?;
        }
    }
    Ok(())
}

fn remove_enrollment(root: &Path, record: &EnrollmentRecord) -> Result<()> {
    remove_enrollment_dir(root, &record.temp_dir)
}

fn remove_enrollment_dir(root: &Path, path: &Path) -> Result<()> {
    remove_owned_directory(
        path,
        &enrollment_directory(root),
        "temporary Codex enrollment",
    )
}

fn cleanup_enrollment_record(root: &Path, record: &EnrollmentRecord) -> Result<()> {
    remove_enrollment(root, record)
}

type WorkspaceRows<'a> = Vec<(&'a String, &'a BTreeMap<String, String>)>;

fn workspaces_from(config: &crate::Config) -> Vec<WorkspaceSnapshot> {
    let mut grouped: BTreeMap<String, WorkspaceRows<'_>> = BTreeMap::new();
    for (raw, bindings) in &config.workspace_bindings {
        let canonical = canonical_workspace(Path::new(raw)).unwrap_or_else(|_| PathBuf::from(raw));
        grouped
            .entry(canonical.display().to_string())
            .or_default()
            .push((raw, bindings));
    }
    grouped
        .into_iter()
        .map(|(canonical, rows)| {
            let mut by_client: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for (_, bindings) in &rows {
                for (app, profile) in *bindings {
                    by_client
                        .entry(app.clone())
                        .or_default()
                        .insert(profile.clone());
                }
            }
            let bindings = by_client
                .iter()
                .filter(|(_, profiles)| profiles.len() == 1)
                .filter_map(|(app, profiles)| {
                    profiles
                        .first()
                        .map(|profile| (app.clone(), profile.clone()))
                })
                .collect();
            let binding_conflicts = by_client
                .into_iter()
                .filter(|(_, profiles)| profiles.len() > 1)
                .map(|(app, profiles)| (app, profiles.into_iter().collect()))
                .collect();
            let metadata = config.workspace_meta.get(&canonical).or_else(|| {
                rows.iter()
                    .find_map(|(raw, _)| config.workspace_meta.get(*raw))
            });
            let path = PathBuf::from(&canonical);
            WorkspaceSnapshot {
                id: canonical.clone(),
                name: metadata
                    .and_then(|value| value.name.clone())
                    .or_else(|| {
                        path.file_name()
                            .and_then(|value| value.to_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| canonical.clone()),
                path: canonical.clone(),
                bindings,
                binding_conflicts,
                exists: path.is_dir(),
            }
        })
        .collect()
}

fn workspace_aliases(config: &crate::Config, canonical: &Path) -> Vec<String> {
    config
        .workspace_bindings
        .keys()
        .filter(|raw| {
            canonical_workspace(Path::new(raw.as_str())).is_ok_and(|path| path == canonical)
        })
        .cloned()
        .collect()
}

fn annotate_project_session_counts(rows: &mut [Session]) {
    let mut counts = BTreeMap::<String, usize>::new();
    for session in rows.iter() {
        let Some(project) = session
            .project_id
            .as_ref()
            .or(session.project_path.as_ref())
            .or(session.cwd.as_ref())
        else {
            continue;
        };
        *counts.entry(project.clone()).or_default() += 1;
    }
    for session in rows {
        let Some(project) = session
            .project_id
            .as_ref()
            .or(session.project_path.as_ref())
            .or(session.cwd.as_ref())
        else {
            continue;
        };
        session.project_session_count = counts.get(project).copied();
    }
}

fn registered_project_for<'a>(
    workspaces: &'a [WorkspaceSnapshot],
    cwd: Option<&str>,
) -> Option<&'a WorkspaceSnapshot> {
    let raw = Path::new(cwd?);
    let canonical = canonical_workspace(raw).unwrap_or_else(|_| raw.to_path_buf());
    workspaces
        .iter()
        .filter(|workspace| canonical.starts_with(Path::new(&workspace.path)))
        .max_by_key(|workspace| Path::new(&workspace.path).components().count())
}

fn runtime_lease_directory_is_empty(directory: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| Error::io("cannot inspect runtime credential leases", error))?;
    Ok(entries.next().is_none())
}

fn prune_runtime_credential_leases(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)
        .map_err(|error| Error::io("cannot inspect runtime credential leases", error))?
    {
        let entry =
            entry.map_err(|error| Error::io("cannot inspect runtime credential lease", error))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !Uuid::parse_str(&name).is_ok_and(|value| value.to_string() == name) {
            return Err(Error::new(
                ErrorCode::MixLocalFailure,
                "runtime credential lease has an invalid identity",
            ));
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| Error::io("cannot inspect runtime credential lease", error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(Error::new(
                ErrorCode::MixLocalFailure,
                "runtime credential lease is not a regular file",
            ));
        }
        let payload = read_bounded(&path, 64, "cannot read runtime credential lease")?;
        let value = std::str::from_utf8(&payload)
            .map(str::trim)
            .map_err(|_| Error::invalid("runtime credential lease is not UTF-8"))?;
        let active = lease_process_active(value);
        let pending = value == "pending";
        if active.is_none() && !pending {
            return Err(Error::new(
                ErrorCode::MixLocalFailure,
                "runtime credential lease has an invalid state",
            ));
        }
        let active = active.unwrap_or(false);
        let pending_is_recent = pending
            && metadata
                .modified()
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_none_or(|age| age < PENDING_RUNTIME_LEASE_RETENTION);
        if !active && !pending_is_recent {
            remove_durable(&path)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_alive(_: u32) -> bool {
    false
}

fn lease_process_active(value: &str) -> Option<bool> {
    let mut lines = value.lines();
    let first = lines.next()?;
    if first == "v2" {
        let pid = lines.next()?.parse::<u32>().ok()?;
        let start = lines.next()?.trim();
        if start.is_empty() || lines.next().is_some() {
            return None;
        }
        return Some(process_identity_matches(pid, start));
    }
    if lines.next().is_some() {
        return None;
    }
    // Keep leases written by the previous release alive until their process
    // exits; deleting them during an upgrade could scrub a live session.
    Some(first.parse::<u32>().ok().is_some_and(process_is_alive))
}

fn process_identity_matches(pid: u32, expected_start: &str) -> bool {
    process_is_alive(pid)
        && process_start_identity(pid).is_some_and(|start| start == expected_start)
}

fn process_start_identity(pid: u32) -> Option<String> {
    let output = Command::new("/bin/ps")
        .env("LC_ALL", "C")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn launch_terminal(
    mix_root: &Path,
    launch: TerminalLaunch<'_>,
    mut cleanup: ProjectedSecretFiles,
) -> Result<()> {
    if launch.argv.is_empty() {
        return Err(Error::invalid("terminal command is empty"));
    }
    if let Some(path) = launch.completion_path.as_deref() {
        let parent = path
            .parent()
            .ok_or_else(|| Error::invalid("terminal completion path has no parent"))?;
        validate_scoped_path(parent, mix_root)?;
    }
    #[cfg(target_os = "macos")]
    {
        let root = mix_root.join("terminal");
        private_scoped_dir(&root, mix_root)?;
        prune_terminal_handoffs(mix_root)?;
        let path = root.join(format!("launch-{}.command", Uuid::new_v4()));
        let environment_path = if launch.environment.is_empty() {
            None
        } else {
            let path = root.join(format!("env-{}", Uuid::new_v4()));
            let mut contents = String::new();
            for (name, value) in launch.environment {
                contents.push_str("export ");
                contents.push_str(name);
                contents.push('=');
                contents.push_str(&shell_quote(value));
                contents.push('\n');
            }
            atomic_write(&path, contents.as_bytes())?;
            Some(path)
        };
        let command = terminal_handoff_script(
            launch.argv,
            launch.variable,
            &launch.state_dir,
            launch.cwd,
            environment_path.as_deref(),
            launch.completion_path.as_deref(),
            &cleanup,
        );
        let launch = (|| {
            atomic_write(&path, command.as_bytes())?;
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .map_err(|error| Error::io("cannot make terminal handoff executable", error))?;
            let child = Command::new("/usr/bin/open")
                .arg(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| Error::io("cannot open Terminal", error))?;
            reap_child(child);
            Ok::<_, Error>(())
        })();
        if let Err(error) = launch {
            let _ = remove_durable(&path);
            if let Some(environment_path) = &environment_path {
                let _ = remove_durable(environment_path);
            }
            return Err(error);
        }
        cleanup.disarm();
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (mix_root, launch);
        let _ = &mut cleanup;
        Err(Error::new(
            ErrorCode::MixUnsupported,
            "terminal handoff is unsupported on this platform",
        ))
    }
}

fn terminal_handoff_script(
    argv: &[String],
    variable: Option<&str>,
    state_dir: &str,
    cwd: Option<&str>,
    environment_path: Option<&Path>,
    completion_path: Option<&Path>,
    cleanup: &ProjectedSecretFiles,
) -> String {
    let mut command = String::from("#!/bin/sh\nset -eu\numask 077\nself=$0\n");
    if let Some(environment_path) = environment_path {
        command.push_str(&format!(
            "env_file={}\n",
            shell_quote(&environment_path.display().to_string())
        ));
    }
    if let Some(completion_path) = completion_path {
        command.push_str(&format!(
            "completion_file={}\n",
            shell_quote(&completion_path.display().to_string())
        ));
    }
    if let Some(lease) = &cleanup.lease {
        command.push_str(&format!(
            "runtime_lock={}\nruntime_lease={}\nruntime_lease_dir={}\n",
            shell_quote(&lease.lock_path.display().to_string()),
            shell_quote(&lease.path.display().to_string()),
            shell_quote(&lease.directory.display().to_string()),
        ));
    }
    command.push_str("cleanup() {\n  set +e");
    if completion_path.is_some() {
        command.push_str(
            "\n  if [ ! -f \"$completion_file\" ]; then printf '255\\n' > \"$completion_file\"; fi",
        );
    }
    command.push_str("\n  rm -f -- \"$self\"");
    if environment_path.is_some() {
        command.push_str(" \"$env_file\"");
    }
    if cleanup.lease.is_some() {
        command.push_str(
            "\n  /usr/bin/lockf -k \"$runtime_lock\" /bin/sh -c '\nlease=$1\nlease_dir=$2\nshift 2\nrm -f -- \"$lease\"\n[ -d \"$lease_dir\" ] || exit 1\nif ! /usr/bin/find \"$lease_dir\" -mindepth 1 -maxdepth 1 -print -quit | /usr/bin/grep -q .; then\n  rm -f -- \"$@\"\nfi\n' mix-runtime-cleanup \"$runtime_lease\" \"$runtime_lease_dir\"",
        );
        for path in &cleanup.files {
            command.push(' ');
            command.push_str(&shell_quote(&path.display().to_string()));
        }
    } else {
        for path in &cleanup.files {
            command.push(' ');
            command.push_str(&shell_quote(&path.display().to_string()));
        }
    }
    command.push_str(
        "\n}\ntrap cleanup EXIT\ntrap 'exit 129' HUP\ntrap 'exit 130' INT\ntrap 'exit 143' TERM\n",
    );
    if cleanup.lease.is_some() {
        command.push_str(
            "printf 'v2\\n%s\\n%s\\n' \"$$\" \"$(LC_ALL=C /bin/ps -p \"$$\" -o lstart=)\" > \"$runtime_lease\"\n",
        );
    }
    if environment_path.is_some() {
        command.push_str(". \"$env_file\"\nrm -f -- \"$env_file\"\n");
    }
    if let Some(cwd) = cwd {
        command.push_str(&format!("cd -- {}\n", shell_quote(cwd)));
    }
    if let Some(variable) = variable {
        command.push_str(&format!("export {variable}={}\n", shell_quote(state_dir)));
    }
    command.push_str("set +e\n");
    command.push_str(
        &argv
            .iter()
            .map(|value| shell_quote(value))
            .collect::<Vec<_>>()
            .join(" "),
    );
    command.push_str("\nexit_code=$?\n");
    if completion_path.is_some() {
        command.push_str("printf '%s\\n' \"$exit_code\" > \"$completion_file\"\n");
    }
    command.push_str("set -e\nexit \"$exit_code\"\n");
    command
}

fn reap_child(mut child: std::process::Child) {
    let _ = thread::Builder::new()
        .name("mix-launch-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

fn prune_terminal_handoffs(mix_root: &Path) -> Result<()> {
    let directory = mix_root.join("terminal");
    validate_scoped_path(&directory, mix_root)?;
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::io("cannot inspect terminal handoffs", error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::new(
            ErrorCode::MixLocalFailure,
            "Mix terminal handoff storage is not a regular directory",
        ));
    }
    for entry in fs::read_dir(&directory)
        .map_err(|error| Error::io("cannot inspect terminal handoffs", error))?
    {
        let entry = entry.map_err(|error| Error::io("cannot inspect terminal handoff", error))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("launch-") && name.ends_with(".command") || name.starts_with("env-"))
        {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| Error::io("cannot inspect terminal handoff", error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= TERMINAL_HANDOFF_RETENTION);
        if stale {
            remove_durable(&path)?;
        }
    }
    Ok(())
}

fn validate_profile_runtime(profile: &Profile) -> Result<()> {
    for name in profile.env.keys().chain(profile.secrets.keys()) {
        validate_env_name(name)?;
    }
    for (name, value) in &profile.env {
        if sensitive_environment_name(name) {
            return Err(Error::new(
                ErrorCode::MixSensitiveDataRejected,
                format!(
                    "environment variable {name} may contain authentication material; use a local credential reference"
                ),
            ));
        }
        if value.contains('\0') {
            return Err(Error::invalid(format!(
                "environment variable contains a null byte: {name}"
            )));
        }
    }
    for reference in profile
        .secret_files
        .values()
        .chain(profile.secrets.values())
    {
        if reference.service.trim().is_empty() || reference.account.trim().is_empty() {
            return Err(Error::invalid(
                "credential reference must include service and account",
            ));
        }
    }
    Ok(())
}

fn materialized_profile_files(
    adapter: &dyn Adapter,
    client: &ClientConfig,
    profile: &Profile,
) -> Result<BTreeMap<String, Vec<u8>>> {
    if let Some(projection) = adapter.global_account_projection() {
        return projection.materialized_files(client, profile);
    }
    profile
        .files
        .iter()
        .map(|(relative, source)| {
            let payload =
                read_bounded(source, 16 * 1024 * 1024, "cannot materialize profile file")?;
            Ok((relative.clone(), payload))
        })
        .collect()
}

fn profile_environment(
    profile: &Profile,
    vault: &dyn CredentialVault,
) -> Result<BTreeMap<String, String>> {
    let mut environment = profile.env.clone();
    for (name, reference) in &profile.secrets {
        let value = String::from_utf8(vault.get(reference)?).map_err(|_| {
            Error::new(
                ErrorCode::MixValidationError,
                format!("secret environment value is not valid UTF-8: {name}"),
            )
        })?;
        if value.contains('\0') {
            return Err(Error::new(
                ErrorCode::MixValidationError,
                format!("secret environment value contains a null byte: {name}"),
            ));
        }
        environment.insert(name.clone(), value);
    }
    Ok(environment)
}

fn validate_env_name(name: &str) -> Result<()> {
    if !is_environment_variable_name(name) {
        return Err(Error::invalid(format!(
            "invalid environment variable name: {name}"
        )));
    }
    Ok(())
}

fn sensitive_environment_name(name: &str) -> bool {
    let normalized = name.to_ascii_uppercase();
    let words = normalized
        .split('_')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    words.iter().enumerate().any(|(index, word)| match *word {
        "TOKEN" | "SECRET" | "PASSWORD" | "CREDENTIAL" | "CREDENTIALS" | "AUTHORIZATION" => true,
        "KEY" => index == 0 || words[index - 1] != "PUBLIC",
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::MemoryVault;

    struct FixtureGlobalAdapter {
        process: crate::ProcessSpec,
        preflight_failure: bool,
    }

    impl Adapter for FixtureGlobalAdapter {
        fn descriptor(&self) -> crate::AdapterDescriptor {
            crate::AdapterDescriptor {
                kind: AdapterKind::Codex,
                name: "fixture",
                label: "Fixture",
                default_home: ".fixture",
                default_command: &["fixture"],
                session_roots: &[],
                import_files: &["route.txt"],
                profile_category: crate::ProfileCategory::Account,
                profile_prefix: "account",
            }
        }

        fn capabilities(&self, _: &ClientConfig, _: bool) -> BTreeMap<String, crate::Capability> {
            BTreeMap::new()
        }

        fn account_capability(&self, client: &ClientConfig) -> AccountSwitchCapability {
            AccountSwitchCapability {
                available: true,
                status: AccountSwitchStatus::Ready,
                current_profile: client.active_profile.clone(),
                ..Default::default()
            }
        }

        fn capture_payload(&self, _: &ClientConfig, _: &str) -> Result<Vec<u8>> {
            Err(Error::new(ErrorCode::MixUnsupported, "fixture"))
        }

        fn validate_profile(&self, _: &ClientConfig, profile: &Profile) -> Result<()> {
            if profile.files.keys().map(String::as_str).eq(["route.txt"])
                && profile
                    .secret_files
                    .keys()
                    .map(String::as_str)
                    .eq(["credential.bin"])
            {
                Ok(())
            } else {
                Err(Error::invalid("fixture profile is incomplete"))
            }
        }

        fn sessions(&self, _: &str, _: &ClientConfig, _: Option<&str>) -> Result<Vec<Session>> {
            Ok(Vec::new())
        }

        fn resume_argv(&self, _: &ClientConfig, _: &str) -> Result<Vec<String>> {
            Err(Error::new(ErrorCode::MixUnsupported, "fixture"))
        }

        fn runtime_variable(&self) -> Option<&'static str> {
            None
        }

        fn desktop_process(&self, _: &ClientConfig) -> Option<crate::ProcessSpec> {
            Some(self.process.clone())
        }

        fn global_account_projection(&self) -> Option<&dyn GlobalAccountProjection> {
            Some(self)
        }

        fn account_capture(&self) -> Option<&dyn AccountCapture> {
            Some(self)
        }

        fn account_enrollment(&self) -> Option<&dyn AccountEnrollment> {
            Some(self)
        }
    }

    impl AccountCapture for FixtureGlobalAdapter {
        fn capture_current(&self, client: &ClientConfig) -> Result<CapturedAccount> {
            let credential = read_bounded(
                &client.live_dir.join("credential.bin"),
                1024,
                "cannot read fixture credential",
            )?;
            let fingerprint = Sha256::digest(&credential)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            Ok(CapturedAccount {
                fingerprint: fingerprint.clone(),
                identity: crate::AccountIdentity {
                    name: Some("Fixture account".into()),
                    suffix: Some(fingerprint[..6].to_ascii_uppercase()),
                    ..Default::default()
                },
                suggested_label: "Fixture account".into(),
                files: BTreeMap::from([(
                    "route.txt".into(),
                    read_bounded(
                        &client.live_dir.join("route.txt"),
                        1024,
                        "cannot read fixture route",
                    )?,
                )]),
                secret_files: BTreeMap::from([("credential.bin".into(), credential)]),
            })
        }

        fn matching_profile(
            &self,
            client: &ClientConfig,
            captured: &CapturedAccount,
        ) -> Result<Option<String>> {
            Ok(client.profiles.iter().find_map(|(name, profile)| {
                (profile.account_fingerprint.as_deref() == Some(&captured.fingerprint))
                    .then(|| name.clone())
            }))
        }

        fn refresh_profile(
            &self,
            client: &mut ClientConfig,
            profile: &str,
            captured: &CapturedAccount,
            vault: &dyn CredentialVault,
        ) -> Result<()> {
            let target = client
                .profiles
                .get_mut(profile)
                .ok_or_else(|| Error::not_found("account", profile))?;
            let reference = target
                .secret_files
                .get("credential.bin")
                .ok_or_else(|| Error::invalid("fixture credential reference is missing"))?;
            let credential = captured
                .secret_files
                .get("credential.bin")
                .ok_or_else(|| Error::invalid("captured fixture credential is missing"))?;
            let route = captured
                .files
                .get("route.txt")
                .ok_or_else(|| Error::invalid("captured fixture route is missing"))?;
            let route_source = target
                .files
                .get("route.txt")
                .ok_or_else(|| Error::invalid("fixture route source is missing"))?;
            vault.set(reference, credential)?;
            atomic_write(route_source, route)?;
            target.account_identity = Some(captured.identity.clone());
            Ok(())
        }
    }

    impl AccountEnrollment for FixtureGlobalAdapter {
        fn prepare(&self, _: &ClientConfig, executable: &Path) -> Result<AccountEnrollmentPlan> {
            Ok(AccountEnrollmentPlan {
                files: BTreeMap::new(),
                argv: vec![executable.display().to_string()],
                runtime_variable: None,
            })
        }

        fn capture_completed(
            &self,
            client: &ClientConfig,
            state_dir: &Path,
        ) -> Result<Option<CapturedAccount>> {
            let mut isolated = client.clone();
            isolated.live_dir = state_dir.to_path_buf();
            self.capture_current(&isolated).map(Some)
        }
    }

    impl GlobalAccountProjection for FixtureGlobalAdapter {
        fn switch_source(&self, client: &ClientConfig) -> Result<Option<String>> {
            Ok(client.active_profile.clone())
        }

        fn validate_target_credential(
            &self,
            _: &ClientConfig,
            target: &Profile,
            vault: &dyn CredentialVault,
        ) -> Result<()> {
            let reference = target
                .secret_files
                .get("credential.bin")
                .ok_or_else(|| Error::invalid("fixture credential is missing"))?;
            vault.get(reference).map(|_| ())
        }

        fn prepare_target_credential(
            &self,
            client: &ClientConfig,
            target: &Profile,
            vault: &dyn CredentialVault,
        ) -> Result<()> {
            if self.preflight_failure {
                return Err(Error::new(
                    ErrorCode::MixAccountRefreshFailed,
                    "fixture preflight failed",
                ));
            }
            self.validate_target_credential(client, target, vault)
        }

        fn synchronize_source(
            &self,
            client: &mut ClientConfig,
            _: &dyn CredentialVault,
        ) -> Result<Option<String>> {
            Ok(client.active_profile.clone())
        }

        fn materialized_files(
            &self,
            _: &ClientConfig,
            target: &Profile,
        ) -> Result<BTreeMap<String, Vec<u8>>> {
            let source = target
                .files
                .get("route.txt")
                .ok_or_else(|| Error::invalid("fixture route is missing"))?;
            Ok(BTreeMap::from([(
                "route.txt".into(),
                read_bounded(source, 1024, "cannot read fixture route")?,
            )]))
        }

        fn verify_projection(&self, client: &ClientConfig, target: &Profile) -> Result<()> {
            let live = read_bounded(
                &client.live_dir.join("route.txt"),
                1024,
                "cannot read live fixture route",
            )?;
            let source = target
                .files
                .get("route.txt")
                .ok_or_else(|| Error::invalid("fixture route is missing"))?;
            let expected = read_bounded(source, 1024, "cannot read fixture route")?;
            if live == expected {
                Ok(())
            } else {
                Err(Error::new(
                    ErrorCode::MixSwitchVerificationFailed,
                    "fixture route projection mismatch",
                ))
            }
        }

        fn verify_stable_projection(&self, client: &ClientConfig, target: &Profile) -> Result<()> {
            self.verify_projection(client, target)
        }

        fn synchronize_target(
            &self,
            _: &ClientConfig,
            _: &Profile,
            _: &dyn CredentialVault,
        ) -> Result<()> {
            Ok(())
        }
    }

    fn service(root: &Path) -> MixService {
        MixService::with_vault(
            root.join("mix/config.json"),
            Arc::new(MemoryVault::default()),
        )
        .unwrap()
    }

    #[test]
    fn rollback_error_exposes_only_stable_context() {
        let error = rolled_back_error(
            Error::new(ErrorCode::MixSwitchVerificationFailed, "private cause"),
            Some(Error::new(
                ErrorCode::MixLocalFailure,
                "private cleanup path",
            )),
            Some("account-1"),
            "account-2",
        );

        assert_eq!(error.code, ErrorCode::MixSwitchRolledBack);
        assert_eq!(
            error.details,
            json!({
                "cause_code":"MIX_SWITCH_VERIFICATION_FAILED",
                "cleanup_pending":true,
                "cleanup_code":"MIX_LOCAL_FAILURE",
                "from_profile":"account-1",
                "to_profile":"account-2",
            })
        );
        assert!(!error.details.to_string().contains("private"));
    }

    #[test]
    fn generic_account_capture_uses_the_adapter_contract() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("route.txt"), b"route-a").unwrap();
        fs::write(live.join("credential.bin"), b"credential-a").unwrap();

        let vault = Arc::new(MemoryVault::default());
        let mut service =
            MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
        service.adapters.replace(
            AdapterKind::Codex,
            Box::new(FixtureGlobalAdapter {
                process: crate::ProcessSpec::default(),
                preflight_failure: false,
            }),
        );
        service.initialize().unwrap();
        service
            .register_client("fixture", AdapterKind::Codex, live.clone())
            .unwrap();

        let first = service
            .capture_current_account("fixture", None, None)
            .unwrap();
        assert_eq!(first["status"], "added");
        assert_eq!(first["profile"], "account-1");
        let config = service.store.load().unwrap();
        let profile = &config.apps["fixture"].profiles["account-1"];
        assert_eq!(profile.label, "Fixture account");
        assert_eq!(fs::read(&profile.files["route.txt"]).unwrap(), b"route-a");
        assert_eq!(
            vault.get(&profile.secret_files["credential.bin"]).unwrap(),
            b"credential-a"
        );

        fs::write(live.join("route.txt"), b"route-b").unwrap();
        let second = service
            .capture_current_account("fixture", None, None)
            .unwrap();
        assert_eq!(second["status"], "already_added");
        let config = service.store.load().unwrap();
        let client = &config.apps["fixture"];
        assert_eq!(client.profiles.len(), 1);
        assert_eq!(
            fs::read(&client.profiles["account-1"].files["route.txt"]).unwrap(),
            b"route-b"
        );
    }

    #[test]
    fn generic_account_enrollment_uses_the_adapter_contract() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("route.txt"), b"current-route").unwrap();
        fs::write(live.join("credential.bin"), b"current-credential").unwrap();

        let mut service = MixService::with_vault(
            root.path().join("mix/config.json"),
            Arc::new(MemoryVault::default()),
        )
        .unwrap();
        service.adapters.replace(
            AdapterKind::Codex,
            Box::new(FixtureGlobalAdapter {
                process: crate::ProcessSpec::default(),
                preflight_failure: false,
            }),
        );
        service.initialize().unwrap();
        service
            .register_client("fixture", AdapterKind::Codex, live)
            .unwrap();

        let config = service.store.load().unwrap();
        let id = "fixture-enrollment";
        let state_dir = config.root.join("transactions/enrollments").join(id);
        private_scoped_dir(&state_dir, &config.root.join("transactions/enrollments")).unwrap();
        fs::write(state_dir.join("route.txt"), b"enrolled-route").unwrap();
        fs::write(state_dir.join("credential.bin"), b"enrolled-credential").unwrap();
        write_enrollment(
            &config.root,
            &EnrollmentRecord {
                version: ENROLLMENT_VERSION,
                id: id.into(),
                app: "fixture".into(),
                mode: "add".into(),
                repair_profile: None,
                temp_dir: state_dir.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
                state: "pending".into(),
                profile: None,
                error: None,
                code: None,
            },
        )
        .unwrap();

        let status = service.account_enrollment_status(id).unwrap();
        assert_eq!(status["state"], "completed");
        assert_eq!(status["profile"], "account-1");
        let config = service.store.load().unwrap();
        let profile = &config.apps["fixture"].profiles["account-1"];
        assert_eq!(
            fs::read(&profile.files["route.txt"]).unwrap(),
            b"enrolled-route"
        );
        assert!(!state_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn target_preflight_failure_never_stops_or_restarts_the_current_client() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        let profiles = root.path().join("profiles");
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&profiles).unwrap();
        fs::write(live.join("route.txt"), b"route-a").unwrap();
        fs::write(live.join("credential.bin"), b"credential-a").unwrap();
        let source_route = profiles.join("source.txt");
        let target_route = profiles.join("target.txt");
        fs::write(&source_route, b"route-a").unwrap();
        fs::write(&target_route, b"route-b").unwrap();

        let executable = root.path().join("fixture-client");
        fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let process = crate::ProcessSpec {
            executable: Some(executable.clone()),
            grace_seconds: 1,
            ..Default::default()
        };
        let controller = ProcessController::new(Some(process.clone()));
        let mut current = Command::new(&executable)
            .args([
                "--exact",
                "service::tests::generic_global_switch_process_fixture",
                "--nocapture",
            ])
            .env("MIX_GLOBAL_SWITCH_FIXTURE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while controller.matching_pids().unwrap().is_empty() && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(controller.matching_pids().unwrap(), vec![current.id()]);

        let source_secret = SecretRef {
            service: "com.mix.fixture".into(),
            account: "source".into(),
        };
        let target_secret = SecretRef {
            service: "com.mix.fixture".into(),
            account: "target".into(),
        };
        let vault = Arc::new(MemoryVault::default());
        vault.set(&source_secret, b"credential-a").unwrap();
        vault.set(&target_secret, b"credential-b").unwrap();
        let mut service =
            MixService::with_vault(root.path().join("mix/config.json"), vault).unwrap();
        service.adapters.replace(
            AdapterKind::Codex,
            Box::new(FixtureGlobalAdapter {
                process,
                preflight_failure: true,
            }),
        );
        service.initialize().unwrap();
        service
            .register_client("fixture", AdapterKind::Codex, live.clone())
            .unwrap();
        service
            .store
            .mutate(|config| {
                let client = config.apps.get_mut("fixture").unwrap();
                client.active_profile = Some("source".into());
                client.profiles = BTreeMap::from([
                    (
                        "source".into(),
                        Profile {
                            label: "Source".into(),
                            files: BTreeMap::from([("route.txt".into(), source_route)]),
                            secret_files: BTreeMap::from([(
                                "credential.bin".into(),
                                source_secret,
                            )]),
                            ..Profile::default()
                        },
                    ),
                    (
                        "target".into(),
                        Profile {
                            label: "Target".into(),
                            files: BTreeMap::from([("route.txt".into(), target_route)]),
                            secret_files: BTreeMap::from([(
                                "credential.bin".into(),
                                target_secret,
                            )]),
                            ..Profile::default()
                        },
                    ),
                ]);
                Ok(())
            })
            .unwrap();

        let error = service.switch("fixture", "target").unwrap_err();
        let still_running = controller.matching_pids().unwrap();
        let route = fs::read(live.join("route.txt")).unwrap();
        let credential = fs::read(live.join("credential.bin")).unwrap();
        let active = service.store.load().unwrap().apps["fixture"]
            .active_profile
            .clone();
        current.kill().unwrap();
        current.wait().unwrap();

        assert_eq!(error.code, ErrorCode::MixAccountRefreshFailed);
        assert_eq!(still_running, vec![current.id()]);
        assert_eq!(route, b"route-a");
        assert_eq!(credential, b"credential-a");
        assert_eq!(active.as_deref(), Some("source"));
        assert!(!SwitchJournal::path(service.store.root().unwrap()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn generic_global_switch_stops_the_old_process_projects_the_target_and_restarts() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        let profiles = root.path().join("profiles");
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&profiles).unwrap();
        fs::write(live.join("route.txt"), b"route-a").unwrap();
        fs::write(live.join("credential.bin"), b"credential-a").unwrap();
        let source_route = profiles.join("source.txt");
        let target_route = profiles.join("target.txt");
        fs::write(&source_route, b"route-a").unwrap();
        fs::write(&target_route, b"route-b").unwrap();

        let app = root.path().join("Fixture.app/Contents");
        let executable = app.join("MacOS/Fixture");
        let helper = app.join("Resources/fixture-app-server");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::create_dir_all(helper.parent().unwrap()).unwrap();
        let fixture = std::env::current_exe().unwrap();
        fs::copy(&fixture, &executable).unwrap();
        fs::copy(&fixture, &helper).unwrap();
        let observed = root.path().join("observed-target");
        let context_activated = root.path().join("context-activated");
        let process = crate::ProcessSpec {
            executable: Some(executable.clone()),
            ready_executable: Some(helper.clone()),
            managed_executables: vec![helper.clone()],
            grace_seconds: 1,
            launch: vec![
                "/bin/sh".into(),
                "-c".into(),
                "MIX_GLOBAL_SWITCH_FIXTURE=1 MIX_GLOBAL_SWITCH_OBSERVED=\"$3\" MIX_GLOBAL_SWITCH_LIVE=\"$4\" \"$1\" --exact service::tests::generic_global_switch_process_fixture --nocapture & sleep 0.2; exec env MIX_GLOBAL_SWITCH_FIXTURE=1 \"$2\" --exact service::tests::generic_global_switch_process_fixture --nocapture".into(),
                "mix-switch-fixture".into(),
                executable.display().to_string(),
                helper.display().to_string(),
                observed.display().to_string(),
                live.display().to_string(),
            ],
            activate: vec![
                "/usr/bin/touch".into(),
                context_activated.display().to_string(),
            ],
            ready_timeout: 3,
        };
        let controller = ProcessController::new(Some(process.clone()));
        let mut old_process = Command::new(&executable)
            .args([
                "--exact",
                "service::tests::generic_global_switch_process_fixture",
                "--nocapture",
            ])
            .env("MIX_GLOBAL_SWITCH_FIXTURE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut old_helper = Command::new(&helper)
            .args([
                "--exact",
                "service::tests::generic_global_switch_process_fixture",
                "--nocapture",
            ])
            .env("MIX_GLOBAL_SWITCH_FIXTURE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut old_pids = vec![old_process.id(), old_helper.id()];
        old_pids.sort_unstable();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while controller.matching_pids().unwrap().len() < 2 && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(controller.matching_pids().unwrap(), old_pids);

        let source_secret = SecretRef {
            service: "com.mix.fixture".into(),
            account: "source".into(),
        };
        let target_secret = SecretRef {
            service: "com.mix.fixture".into(),
            account: "target".into(),
        };
        let vault = Arc::new(MemoryVault::default());
        vault.set(&source_secret, b"credential-a").unwrap();
        vault.set(&target_secret, b"credential-b").unwrap();
        let mut service =
            MixService::with_vault(root.path().join("mix/config.json"), vault).unwrap();
        service.adapters.replace(
            AdapterKind::Codex,
            Box::new(FixtureGlobalAdapter {
                process,
                preflight_failure: false,
            }),
        );
        service.initialize().unwrap();
        service
            .register_client("fixture", AdapterKind::Codex, live.clone())
            .unwrap();
        service
            .store
            .mutate(|config| {
                let client = config.apps.get_mut("fixture").unwrap();
                client.active_profile = Some("source".into());
                client.profiles = BTreeMap::from([
                    (
                        "source".into(),
                        Profile {
                            label: "Source".into(),
                            files: BTreeMap::from([("route.txt".into(), source_route)]),
                            secret_files: BTreeMap::from([(
                                "credential.bin".into(),
                                source_secret,
                            )]),
                            ..Profile::default()
                        },
                    ),
                    (
                        "target".into(),
                        Profile {
                            label: "Target".into(),
                            files: BTreeMap::from([("route.txt".into(), target_route)]),
                            secret_files: BTreeMap::from([(
                                "credential.bin".into(),
                                target_secret,
                            )]),
                            ..Profile::default()
                        },
                    ),
                ]);
                Ok(())
            })
            .unwrap();

        let switch = service.switch("fixture", "target");
        let _ = old_process.wait();
        let _ = old_helper.wait();
        let observation_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !observed.is_file() && std::time::Instant::now() < observation_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let cleanup = controller.stop();
        let outcome = switch.unwrap();
        cleanup.unwrap();

        assert_eq!(outcome.status, "switched");
        assert_eq!(outcome.stopped_pids, old_pids);
        assert!(outcome.restarted);
        assert_eq!(fs::read(live.join("route.txt")).unwrap(), b"route-b");
        assert_eq!(
            fs::read(live.join("credential.bin")).unwrap(),
            b"credential-b"
        );
        assert_eq!(
            fs::read(observed).unwrap(),
            b"route-b\0credential-b",
            "the restarted client must observe the target route and credential as one pair"
        );
        assert!(!context_activated.is_file());
        assert!(!SwitchJournal::path(&service.store.load().unwrap().root).exists());
    }

    #[test]
    fn generic_global_switch_process_fixture() {
        if std::env::var_os("MIX_GLOBAL_SWITCH_FIXTURE").is_some() {
            if let (Some(observed), Some(live)) = (
                std::env::var_os("MIX_GLOBAL_SWITCH_OBSERVED"),
                std::env::var_os("MIX_GLOBAL_SWITCH_LIVE"),
            ) {
                let live = PathBuf::from(live);
                let mut projection = fs::read(live.join("route.txt")).unwrap();
                projection.push(0);
                projection.extend(fs::read(live.join("credential.bin")).unwrap());
                fs::write(observed, projection).unwrap();
            }
            thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn generated_api_key_label_uses_any_custom_provider_identity() {
        let profile = Profile {
            label: "Codex · A1B2C3".into(),
            ..Profile::default()
        };
        let identity = crate::AccountIdentity {
            suffix: Some("A1B2C3".into()),
            suggested_label: Some("Codex · A1B2C3".into()),
            ..Default::default()
        };
        let provider = ProviderIdentity {
            id: "arbitrary-gateway".into(),
            name: "Company Gateway".into(),
            official: false,
        };
        let adapter = crate::CodexAdapter;

        assert_eq!(
            adapter.profile_display_label(&profile, Some(&identity), Some(&provider)),
            "Company Gateway · ID A1B2C3"
        );
    }

    #[test]
    fn user_alias_wins_over_a_generated_custom_provider_label() {
        let profile = Profile {
            label: "Work".into(),
            label_origin: ProfileLabelOrigin::User,
            ..Profile::default()
        };
        let identity = crate::AccountIdentity {
            email: Some("owner@example.com".into()),
            suffix: Some("A1B2C3".into()),
            suggested_label: Some("owner@example.com".into()),
            ..Default::default()
        };
        let provider = ProviderIdentity {
            id: "arbitrary-gateway".into(),
            name: "Company Gateway".into(),
            official: false,
        };
        let adapter = crate::CodexAdapter;

        assert_eq!(
            adapter.profile_display_label(&profile, Some(&identity), Some(&provider)),
            "Work"
        );
    }

    #[test]
    fn generated_label_tracks_the_latest_detected_identity() {
        let profile = Profile {
            label: "Codex account 1".into(),
            label_origin: ProfileLabelOrigin::Generated,
            ..Profile::default()
        };
        let identity = crate::AccountIdentity {
            email: Some("owner@example.com".into()),
            suggested_label: Some("owner@example.com".into()),
            ..Default::default()
        };
        let adapter = crate::CodexAdapter;

        assert_eq!(
            adapter.profile_display_label(&profile, Some(&identity), None),
            "owner@example.com"
        );
    }

    #[test]
    fn profile_selectors_accept_unique_labels_without_guessing() {
        let profile = |label: &str| Profile {
            label: label.into(),
            label_origin: ProfileLabelOrigin::User,
            ..Profile::default()
        };
        let client = ClientConfig {
            adapter: AdapterKind::Claude,
            live_dir: PathBuf::from("/tmp/claude"),
            active_profile: None,
            profiles: BTreeMap::from([
                ("account-1".into(), profile("Personal")),
                ("account-2".into(), profile("account-1")),
            ]),
            run: RunSpec::default(),
        };
        let adapter = crate::ClaudeAdapter;

        assert_eq!(
            resolve_profile_selector(&adapter, &client, "Personal").unwrap(),
            "account-1"
        );
        assert_eq!(
            resolve_profile_selector(&adapter, &client, "account-1").unwrap(),
            "account-1"
        );
        assert_eq!(
            resolve_profile_selector(&adapter, &client, "Missing")
                .unwrap_err()
                .code,
            ErrorCode::MixNotFound
        );
    }

    #[test]
    fn ambiguous_profile_labels_fail_closed_with_stable_ids() {
        let profile = Profile {
            label: "Work".into(),
            label_origin: ProfileLabelOrigin::User,
            ..Profile::default()
        };
        let client = ClientConfig {
            adapter: AdapterKind::Claude,
            live_dir: PathBuf::from("/tmp/claude"),
            active_profile: None,
            profiles: BTreeMap::from([
                ("environment-2".into(), profile.clone()),
                ("environment-1".into(), profile),
            ]),
            run: RunSpec::default(),
        };
        let adapter = crate::ClaudeAdapter;

        let error = resolve_profile_selector(&adapter, &client, "Work").unwrap_err();
        assert_eq!(error.code, ErrorCode::MixConflict);
        assert!(error.to_string().contains("environment-1, environment-2"));
    }

    #[test]
    fn switch_resolves_a_visible_label_in_core() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("claude", AdapterKind::Claude, root.path().join("claude"))
            .unwrap();
        service
            .store
            .mutate(|config| {
                config.apps.get_mut("claude").unwrap().profiles.insert(
                    "environment-1".into(),
                    Profile {
                        label: "Work".into(),
                        label_origin: ProfileLabelOrigin::User,
                        ..Profile::default()
                    },
                );
                Ok(())
            })
            .unwrap();

        let outcome = service.switch("claude", "Work").unwrap();
        assert_eq!(outcome.status, "selected");
        assert_eq!(outcome.to_profile, "environment-1");
    }

    #[test]
    fn selecting_a_claude_environment_never_projects_it_into_native_home() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("claude");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("settings.json"), br#"{"theme":"native"}"#).unwrap();
        let first = root.path().join("first.json");
        let second = root.path().join("second.json");
        fs::write(&first, br#"{"theme":"light"}"#).unwrap();
        fs::write(&second, br#"{"theme":"dark"}"#).unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("claude", AdapterKind::Claude, live.clone())
            .unwrap();
        service
            .store
            .mutate(|config| {
                let client = config.apps.get_mut("claude").unwrap();
                client.active_profile = Some("environment-1".into());
                client.profiles.insert(
                    "environment-1".into(),
                    Profile {
                        label: "First".into(),
                        files: BTreeMap::from([("settings.json".into(), first)]),
                        ..Profile::default()
                    },
                );
                client.profiles.insert(
                    "environment-2".into(),
                    Profile {
                        label: "Second".into(),
                        files: BTreeMap::from([("settings.json".into(), second)]),
                        ..Profile::default()
                    },
                );
                Ok(())
            })
            .unwrap();

        let outcome = service.switch("claude", "Second").unwrap();

        assert_eq!(outcome.status, "selected");
        assert_eq!(outcome.from_profile.as_deref(), Some("environment-1"));
        assert_eq!(outcome.to_profile, "environment-2");
        assert!(outcome.backup.is_empty());
        assert!(outcome.stopped_pids.is_empty());
        assert!(!outcome.restarted);
        assert_eq!(
            fs::read(live.join("settings.json")).unwrap(),
            br#"{"theme":"native"}"#
        );
        assert!(!SwitchJournal::path(service.store.root().unwrap()).exists());
    }

    #[test]
    fn initializes_with_json_state_and_no_external_runtime() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        let snapshot = service.initialize().unwrap();
        assert!(snapshot.apps.is_empty());
        assert!(root.path().join("mix/config.json").is_file());
    }

    #[test]
    fn an_unconfigured_client_is_onboarding_not_a_health_failure() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, root.path().join("codex"))
            .unwrap();
        service
            .store
            .mutate(|config| {
                config.apps.get_mut("codex").unwrap().run.command.clear();
                Ok(())
            })
            .unwrap();

        let snapshot = service.state().unwrap();
        assert!(snapshot.needs_setup);
        assert!(matches!(snapshot.health.status, HealthStatus::Healthy));
        assert!(snapshot.health.issues.is_empty());
        assert!(snapshot.apps[0]
            .issues
            .iter()
            .any(|issue| matches!(issue.code, ClientIssueCode::NoProfiles)));
        assert!(snapshot.apps[0]
            .issues
            .iter()
            .any(|issue| matches!(issue.code, ClientIssueCode::RunCommandMissing)));
    }

    #[test]
    fn diagnostics_include_build_identity_without_private_client_values() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("private-native-directory");
        fs::create_dir_all(&live).unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("private-client-name", AdapterKind::Codex, live.clone())
            .unwrap();
        service
            .store
            .mutate(|config| {
                config
                    .apps
                    .get_mut("private-client-name")
                    .unwrap()
                    .profiles
                    .insert(
                        "private-profile-id".into(),
                        Profile {
                            label: "Confidential profile label".into(),
                            ..Profile::default()
                        },
                    );
                Ok(())
            })
            .unwrap();

        let report = service.diagnostics().unwrap();
        assert_eq!(report["product"]["name"], "mix");
        assert_eq!(report["product"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(report["platform"]["os"], std::env::consts::OS);
        assert_eq!(report["platform"]["arch"], std::env::consts::ARCH);
        assert_eq!(report["client_count"], 1);
        assert_eq!(report["clients"][0]["profile_count"], 1);
        assert_eq!(
            report["clients"][0]["native_files"]["auth_kind"],
            "missing_or_invalid_file"
        );
        assert_eq!(report["clients"][0]["process"]["status"], "known");

        let serialized = report.to_string();
        assert!(!serialized.contains("private-client-name"));
        assert!(!serialized.contains("private-profile-id"));
        assert!(!serialized.contains("Confidential profile label"));
        assert!(!serialized.contains(live.to_string_lossy().as_ref()));
    }

    #[test]
    fn custom_codex_directory_never_controls_the_native_desktop_app() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client(
                "codex",
                AdapterKind::Codex,
                root.path().join("isolated-codex"),
            )
            .unwrap();

        let client = &service.store.load().unwrap().apps["codex"];
        assert!(service
            .adapters
            .get(client.adapter)
            .desktop_process(client)
            .is_none());
    }

    #[test]
    fn product_snapshot_orders_codex_before_claude() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("claude", AdapterKind::Claude, root.path().join("claude"))
            .unwrap();
        service
            .register_client("codex", AdapterKind::Codex, root.path().join("codex"))
            .unwrap();

        assert_eq!(
            service
                .state()
                .unwrap()
                .apps
                .iter()
                .map(|client| client.name.as_str())
                .collect::<Vec<_>>(),
            ["codex", "claude"]
        );
    }

    #[test]
    fn activities_keep_recent_rows_across_log_rotation() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        let activity_root = root.path().join("mix");
        let row = |id: &str, at: &str| {
            serde_json::json!({
                "id": id,
                "kind": "switch",
                "at": at,
                "app": "codex",
            })
            .to_string()
        };
        atomic_write(
            &activity_root.join("activity.jsonl.1"),
            format!(
                "{}\n{}\n",
                row("old", "2026-09-03T00:01:00Z"),
                row("middle", "2026-09-03T00:02:00Z")
            )
            .as_bytes(),
        )
        .unwrap();
        atomic_write(
            &activity_root.join("activity.jsonl"),
            format!(
                "{}\n{}\n",
                row("new", "2026-09-03T00:03:00Z"),
                row("newest", "2026-09-03T00:04:00Z")
            )
            .as_bytes(),
        )
        .unwrap();

        let rows = service.activities(3).unwrap();
        assert_eq!(
            rows.iter()
                .map(|activity| activity.id.as_str())
                .collect::<Vec<_>>(),
            vec!["newest", "new", "middle"]
        );
    }

    #[test]
    fn one_client_adapter_cannot_be_connected_twice() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, root.path().join("codex-a"))
            .unwrap();

        let error = service
            .register_client(
                "codex-secondary",
                AdapterKind::Codex,
                root.path().join("codex-b"),
            )
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::MixConflict);
    }

    #[test]
    fn projected_secrets_are_removed_until_terminal_owns_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let removed = root.path().join("removed-secret");
        fs::write(&removed, b"secret").unwrap();
        {
            let _cleanup = ProjectedSecretFiles {
                files: vec![removed.clone()],
                lease: None,
                armed: true,
            };
        }
        assert!(!removed.exists());

        let handed_off = root.path().join("handed-off-secret");
        fs::write(&handed_off, b"secret").unwrap();
        {
            let mut cleanup = ProjectedSecretFiles {
                files: vec![handed_off.clone()],
                lease: None,
                armed: true,
            };
            cleanup.disarm();
        }
        assert!(handed_off.exists());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn runtime_lease_identity_rejects_a_reused_pid() {
        let pid = std::process::id();
        let start = process_start_identity(pid).expect("current process start identity");
        assert!(lease_process_active(&format!("v2\n{pid}\n{start}\n")).unwrap_or(false));
        assert!(!lease_process_active(&format!("v2\n{pid}\n{start}-reused\n")).unwrap_or(true));
        assert!(lease_process_active(&pid.to_string()).unwrap_or(false));
    }

    #[test]
    fn runtime_credentials_are_kept_until_the_last_lease_ends() {
        let root = tempfile::tempdir().unwrap();
        let runtime_root = root.path().join("runtime");
        let runtime = runtime_root.join("codex/account-1/project");
        private_scoped_dir(&runtime, &runtime_root).unwrap();
        let leases = runtime.join(RUNTIME_LEASE_DIRECTORY);
        private_scoped_dir(&leases, &runtime_root).unwrap();
        let active_lease = leases.join(Uuid::new_v4().to_string());
        fs::write(&active_lease, format!("{}\n", std::process::id())).unwrap();
        let secret = runtime.join("auth.json");
        fs::write(&secret, b"secret").unwrap();

        {
            let _second =
                ProjectedSecretFiles::for_runtime(&runtime, &runtime_root, vec![secret.clone()])
                    .unwrap();
        }
        assert!(secret.exists());

        remove_durable(&active_lease).unwrap();
        {
            let _last =
                ProjectedSecretFiles::for_runtime(&runtime, &runtime_root, vec![secret.clone()])
                    .unwrap();
            fs::write(&secret, b"new-secret").unwrap();
        }
        assert!(!secret.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn terminal_handoff_is_valid_and_releases_its_runtime_lease() {
        let root = tempfile::tempdir().unwrap();
        let runtime_root = root.path().join("runtime");
        let runtime = runtime_root.join("codex/account-1/project");
        private_scoped_dir(&runtime, &runtime_root).unwrap();
        let secret = runtime.join("auth.json");
        fs::write(&secret, b"secret").unwrap();
        let mut cleanup =
            ProjectedSecretFiles::for_runtime(&runtime, &runtime_root, vec![secret.clone()])
                .unwrap();
        let script = root.path().join("handoff.command");
        let completion = root.path().join("completion");
        let contents = terminal_handoff_script(
            &["/usr/bin/true".into()],
            Some("CODEX_HOME"),
            runtime.to_string_lossy().as_ref(),
            Some(root.path().to_string_lossy().as_ref()),
            None,
            Some(&completion),
            &cleanup,
        );
        atomic_write(&script, contents.as_bytes()).unwrap();
        assert!(Command::new("/bin/sh")
            .arg("-n")
            .arg(&script)
            .status()
            .unwrap()
            .success());

        cleanup.disarm();
        drop(cleanup);
        assert!(Command::new("/bin/sh")
            .arg(&script)
            .status()
            .unwrap()
            .success());
        assert!(!script.exists());
        assert!(!secret.exists());
        assert_eq!(fs::read_to_string(completion).unwrap(), "0\n");
        assert!(runtime_lease_directory_is_empty(&runtime.join(RUNTIME_LEASE_DIRECTORY)).unwrap());
    }

    #[test]
    fn current_codex_account_is_added_without_a_required_name() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("config.toml"), "model = \"gpt\"\n").unwrap();
        fs::write(
            home.join("auth.json"),
            r#"{"tokens":{"account_id":"account-a","access_token":"token"}}"#,
        )
        .unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, home.clone())
            .unwrap();
        let result = service
            .capture_current_account("codex", None, None)
            .unwrap();
        assert_eq!(result["profile"], "account-1");
        let state = service.state().unwrap();
        assert_eq!(state.apps[0].profiles.len(), 1);

        service
            .store
            .mutate(|config| {
                config.apps.get_mut("codex").unwrap().active_profile = None;
                Ok(())
            })
            .unwrap();
        let known = service.state().unwrap();
        assert_eq!(known.apps[0].active.as_deref(), Some("account-1"));
        assert!(known.apps[0]
            .issues
            .iter()
            .all(|issue| !matches!(issue.code, ClientIssueCode::UnmanagedAccount)));

        fs::write(
            home.join("auth.json"),
            r#"{"tokens":{"account_id":"account-b","access_token":"token"}}"#,
        )
        .unwrap();
        let unmanaged = service.state().unwrap();
        assert_eq!(unmanaged.apps[0].active, None);
        assert!(unmanaged.apps[0]
            .issues
            .iter()
            .any(|issue| matches!(issue.code, ClientIssueCode::UnmanagedAccount)));
    }

    #[test]
    fn switch_adopts_an_externally_selected_account_without_restarting() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        fs::write(
            live.join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();
        let first = json!({
            "last_refresh":"2026-09-04T00:00:00Z",
            "tokens":{"account_id":"account-a","access_token":"first"}
        });
        let second = json!({
            "last_refresh":"2026-09-04T00:00:00Z",
            "tokens":{"account_id":"account-b","access_token":"old"}
        });
        fs::write(live.join("auth.json"), serde_json::to_vec(&first).unwrap()).unwrap();
        let vault = Arc::new(MemoryVault::default());
        let service =
            MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live.clone())
            .unwrap();
        service
            .capture_current_account("codex", None, Some("A"))
            .unwrap();
        fs::write(live.join("auth.json"), serde_json::to_vec(&second).unwrap()).unwrap();
        service
            .capture_current_account("codex", None, Some("B"))
            .unwrap();
        let newer_second = json!({
            "last_refresh":"2026-09-06T00:00:00Z",
            "tokens":{"account_id":"account-b","access_token":"new"}
        });
        fs::write(
            live.join("auth.json"),
            serde_json::to_vec(&newer_second).unwrap(),
        )
        .unwrap();
        service
            .store
            .mutate(|config| {
                config.apps.get_mut("codex").unwrap().active_profile = Some("account-1".into());
                Ok(())
            })
            .unwrap();

        let outcome = service.switch("codex", "account-2").unwrap();

        assert_eq!(outcome.status, "already_active");
        assert_eq!(outcome.from_profile.as_deref(), Some("account-2"));
        assert!(outcome.stopped_pids.is_empty());
        assert!(!outcome.restarted);
        let config = service.store.load().unwrap();
        assert_eq!(
            config.apps["codex"].active_profile.as_deref(),
            Some("account-2")
        );
        let reference = &config.apps["codex"].profiles["account-2"].secret_files["auth.json"];
        assert_eq!(
            serde_json::from_slice::<Value>(&vault.get(reference).unwrap()).unwrap(),
            newer_second
        );
        assert!(service
            .activities(20)
            .unwrap()
            .iter()
            .all(|activity| activity.kind != "switch"));
    }

    #[test]
    fn switch_uses_the_observed_account_and_route_when_saved_active_state_is_stale() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        let gateway_account = json!({"OPENAI_API_KEY":"gateway-account-key"});
        let official_a = json!({
            "tokens":{"account_id":"official-a","access_token":"official-a-token"}
        });
        let official_b = json!({
            "tokens":{"account_id":"official-b","access_token":"official-b-token"}
        });
        let vault = Arc::new(MemoryVault::default());
        let service =
            MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live.clone())
            .unwrap();

        fs::write(
            live.join("config.toml"),
            "model_provider = \"custom\"\n[model_providers.custom]\nname = \"Partner Gateway\"\nbase_url = \"https://gateway.example/v1\"\nwire_api = \"responses\"\n",
        )
        .unwrap();
        fs::write(
            live.join("auth.json"),
            serde_json::to_vec(&gateway_account).unwrap(),
        )
        .unwrap();
        service
            .capture_current_account("codex", None, Some("Partner Gateway"))
            .unwrap();

        fs::write(live.join("config.toml"), "model = \"official-a\"\n").unwrap();
        fs::write(
            live.join("auth.json"),
            serde_json::to_vec(&official_a).unwrap(),
        )
        .unwrap();
        service
            .capture_current_account("codex", None, Some("Official A"))
            .unwrap();

        fs::write(live.join("config.toml"), "model = \"official-b\"\n").unwrap();
        fs::write(
            live.join("auth.json"),
            serde_json::to_vec(&official_b).unwrap(),
        )
        .unwrap();
        service
            .capture_current_account("codex", None, Some("Official B"))
            .unwrap();

        let official_a_reference = service.store.load().unwrap().apps["codex"].profiles
            ["account-2"]
            .secret_files["auth.json"]
            .clone();
        let official_a_before = vault.get(&official_a_reference).unwrap();
        fs::write(
            live.join("config.toml"),
            "model_provider = \"custom\"\n[model_providers.custom]\nname = \"Partner Gateway\"\nbase_url = \"https://gateway.example/v1\"\nwire_api = \"responses\"\n",
        )
        .unwrap();
        fs::write(
            live.join("auth.json"),
            serde_json::to_vec(&gateway_account).unwrap(),
        )
        .unwrap();
        service
            .store
            .mutate(|config| {
                config.apps.get_mut("codex").unwrap().active_profile = Some("account-2".into());
                Ok(())
            })
            .unwrap();

        let outcome = service.switch("codex", "account-3").unwrap();

        assert_eq!(outcome.status, "switched");
        assert_eq!(outcome.from_profile.as_deref(), Some("account-1"));
        assert_eq!(outcome.to_profile, "account-3");
        assert_eq!(
            crate::CodexAdapter::parse_auth(&live.join("auth.json")).unwrap(),
            official_b
        );
        assert_eq!(
            crate::CodexAdapter::provider(&service.store.load().unwrap().apps["codex"])
                .unwrap()
                .id,
            "openai"
        );
        assert_eq!(vault.get(&official_a_reference).unwrap(), official_a_before);
        assert_eq!(
            service.store.load().unwrap().apps["codex"]
                .active_profile
                .as_deref(),
            Some("account-3")
        );
    }

    #[test]
    fn local_file_credentials_switch_accounts_without_system_authorization() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(live.join("sessions")).unwrap();
        fs::write(
            live.join("config.toml"),
            "cli_auth_credentials_store = \"file\"\nmodel = \"gpt\"\n",
        )
        .unwrap();
        let session = live.join("sessions/native.jsonl");
        fs::write(&session, b"native-history\n").unwrap();
        let account_a = json!({"tokens":{"account_id":"account-a","access_token":"a"}});
        let account_b = json!({"tokens":{"account_id":"account-b","access_token":"b"}});
        fs::write(
            live.join("auth.json"),
            serde_json::to_vec(&account_a).unwrap(),
        )
        .unwrap();

        let service = MixService::new(root.path().join("mix/config.json")).unwrap();
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live.clone())
            .unwrap();
        service
            .capture_current_account("codex", None, Some("A"))
            .unwrap();
        fs::write(
            live.join("auth.json"),
            serde_json::to_vec(&account_b).unwrap(),
        )
        .unwrap();
        service
            .capture_current_account("codex", None, Some("B"))
            .unwrap();

        assert_eq!(
            service.switch("codex", "account-1").unwrap().status,
            "switched"
        );
        assert_eq!(
            crate::CodexAdapter::parse_auth(&live.join("auth.json")).unwrap(),
            account_a
        );
        assert_eq!(
            service.switch("codex", "account-2").unwrap().status,
            "switched"
        );
        assert_eq!(
            crate::CodexAdapter::parse_auth(&live.join("auth.json")).unwrap(),
            account_b
        );
        assert_eq!(fs::read(session).unwrap(), b"native-history\n");

        let credential_files = fs::read_dir(root.path().join("mix/credentials"))
            .unwrap()
            .count();
        assert_eq!(credential_files, 2);
    }

    #[test]
    fn one_credential_can_back_multiple_provider_routes() {
        for credential in [
            json!({"OPENAI_API_KEY":"shared-key"}),
            json!({"tokens":{"account_id":"shared-account","access_token":"token"}}),
        ] {
            let root = tempfile::tempdir().unwrap();
            let live = root.path().join("codex");
            fs::create_dir_all(&live).unwrap();
            fs::write(
                live.join("auth.json"),
                serde_json::to_vec(&credential).unwrap(),
            )
            .unwrap();
            fs::write(
                live.join("config.toml"),
                "cli_auth_credentials_store = \"file\"\nmodel_provider = \"first\"\n[model_providers.first]\nbase_url = \"https://first.example\"\n",
            )
            .unwrap();
            let service = service(root.path());
            service.initialize().unwrap();
            service
                .register_client("codex", AdapterKind::Codex, live.clone())
                .unwrap();
            service
                .capture_current_account("codex", None, None)
                .unwrap();

            fs::write(
                live.join("config.toml"),
                "cli_auth_credentials_store = \"file\"\nmodel_provider = \"second\"\n[model_providers.second]\nbase_url = \"https://second.example\"\n",
            )
            .unwrap();
            service
                .capture_current_account("codex", None, None)
                .unwrap();

            let config = service.store.load().unwrap();
            let client = &config.apps["codex"];
            let adapter = crate::CodexAdapter;
            assert_eq!(client.profiles.len(), 2);
            assert_eq!(client.active_profile.as_deref(), Some("account-2"));
            assert_eq!(
                adapter
                    .profile_provider(&client.profiles["account-1"])
                    .unwrap()
                    .unwrap()
                    .id,
                "first"
            );
            assert_eq!(
                adapter
                    .profile_provider(&client.profiles["account-2"])
                    .unwrap()
                    .unwrap()
                    .id,
                "second"
            );
            drop(config);

            service.switch("codex", "account-1").unwrap();
            assert_eq!(
                provider_from_toml(&read_codex_config(&live.join("config.toml")).unwrap()).id,
                "first"
            );
            service.switch("codex", "account-2").unwrap();
            assert_eq!(
                provider_from_toml(&read_codex_config(&live.join("config.toml")).unwrap()).id,
                "second"
            );
        }
    }

    #[test]
    fn adding_an_existing_current_account_refreshes_its_saved_credential() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("config.toml"), "model = \"old-model\"\n").unwrap();
        fs::write(
            live.join("auth.json"),
            r#"{"tokens":{"account_id":"account-a","access_token":"old"}}"#,
        )
        .unwrap();
        let vault = Arc::new(MemoryVault::default());
        let service =
            MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live.clone())
            .unwrap();
        service
            .capture_current_account("codex", None, None)
            .unwrap();
        let reference = service.store.load().unwrap().apps["codex"].profiles["account-1"]
            .secret_files["auth.json"]
            .clone();

        let fresh = json!({"tokens":{"account_id":"account-a","access_token":"fresh"}});
        fs::write(live.join("auth.json"), serde_json::to_vec(&fresh).unwrap()).unwrap();
        fs::write(live.join("config.toml"), "model = \"fresh-model\"\n").unwrap();
        let result = service
            .capture_current_account("codex", None, None)
            .unwrap();

        assert_eq!(result["status"], "already_added");
        assert_eq!(
            serde_json::from_slice::<Value>(&vault.get(&reference).unwrap()).unwrap(),
            fresh
        );
        let profile = service.store.load().unwrap().apps["codex"].profiles["account-1"].files
            ["config.toml"]
            .clone();
        assert_eq!(
            read_codex_config(&profile).unwrap().get("model"),
            Some(&toml::Value::String("fresh-model".into()))
        );

        let newest_saved = json!({
            "auth_mode":"chatgpt",
            "last_refresh":"2026-09-05T12:00:00Z",
            "tokens":{"account_id":"account-a","access_token":"newest-saved"}
        });
        let stale_live = json!({
            "auth_mode":"chatgpt",
            "last_refresh":"2026-09-05T11:00:00Z",
            "tokens":{"account_id":"account-a","access_token":"stale-live"}
        });
        vault
            .set(&reference, &serde_json::to_vec(&newest_saved).unwrap())
            .unwrap();
        fs::write(
            live.join("auth.json"),
            serde_json::to_vec(&stale_live).unwrap(),
        )
        .unwrap();

        service
            .capture_current_account("codex", None, None)
            .unwrap();

        assert_eq!(
            serde_json::from_slice::<Value>(&vault.get(&reference).unwrap()).unwrap(),
            newest_saved
        );
    }

    #[test]
    fn codex_credential_merge_preserves_the_newest_rotation_for_any_account() {
        let vault = MemoryVault::default();
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "account".into(),
        };
        let saved = json!({
            "auth_mode":"chatgpt",
            "last_refresh":"2026-09-05T12:00:00Z",
            "tokens":{"account_id":"account-a","access_token":"saved"}
        });
        let older_live = json!({
            "auth_mode":"chatgpt",
            "last_refresh":"2026-09-05T11:00:00Z",
            "tokens":{"account_id":"account-a","access_token":"older-live"}
        });
        let newer_live = json!({
            "auth_mode":"chatgpt",
            "last_refresh":"2026-09-05T13:00:00+00:00",
            "tokens":{"account_id":"account-a","access_token":"newer-live"}
        });
        let ambiguous_live = json!({
            "auth_mode":"chatgpt",
            "last_refresh":"2026-09-05T12:00:00Z",
            "tokens":{"account_id":"account-a","access_token":"ambiguous-live"}
        });
        let fingerprint = crate::CodexAdapter::fingerprint(&saved).unwrap();
        vault
            .set(&reference, &serde_json::to_vec(&saved).unwrap())
            .unwrap();

        merge_codex_credential(&vault, &reference, &fingerprint, &older_live).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&vault.get(&reference).unwrap()).unwrap(),
            saved
        );

        merge_codex_credential(&vault, &reference, &fingerprint, &ambiguous_live).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&vault.get(&reference).unwrap()).unwrap(),
            saved
        );

        merge_codex_credential(&vault, &reference, &fingerprint, &newer_live).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&vault.get(&reference).unwrap()).unwrap(),
            newer_live
        );
    }

    #[test]
    fn codex_credential_merge_rejects_a_cross_account_vault_reference() {
        let vault = MemoryVault::default();
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "account".into(),
        };
        let live = json!({"tokens":{"account_id":"account-a","access_token":"live"}});
        let wrong_saved = json!({"tokens":{"account_id":"account-b","access_token":"saved"}});
        vault
            .set(&reference, &serde_json::to_vec(&wrong_saved).unwrap())
            .unwrap();

        let error = merge_codex_credential(
            &vault,
            &reference,
            &crate::CodexAdapter::fingerprint(&live).unwrap(),
            &live,
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::MixAccountReauthRequired);
        assert_eq!(
            serde_json::from_slice::<Value>(&vault.get(&reference).unwrap()).unwrap(),
            wrong_saved
        );
    }

    #[test]
    fn explicit_codex_sync_refreshes_the_account_overlay_too() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("config.toml"), "model = \"before-sync\"\n").unwrap();
        fs::write(
            live.join("auth.json"),
            r#"{"tokens":{"account_id":"account-a","access_token":"before"}}"#,
        )
        .unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live.clone())
            .unwrap();
        service
            .capture_current_account("codex", None, None)
            .unwrap();
        fs::write(live.join("config.toml"), "model = \"after-sync\"\n").unwrap();
        fs::write(
            live.join("auth.json"),
            r#"{"tokens":{"account_id":"account-a","access_token":"after"}}"#,
        )
        .unwrap();

        service.sync_active_account("codex").unwrap();

        let stored = service.store.load().unwrap();
        let source = &stored.apps["codex"].profiles["account-1"].files["config.toml"];
        assert_eq!(
            read_codex_config(source).unwrap()["model"].as_str(),
            Some("after-sync")
        );
    }

    #[test]
    fn explicit_codex_sync_cannot_replace_a_newer_saved_rotation() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("config.toml"), "model = \"gpt\"\n").unwrap();
        let older_live = json!({
            "auth_mode":"chatgpt",
            "last_refresh":"2026-09-05T11:00:00Z",
            "tokens":{"account_id":"account-a","access_token":"older-live"}
        });
        fs::write(
            live.join("auth.json"),
            serde_json::to_vec(&older_live).unwrap(),
        )
        .unwrap();
        let vault = Arc::new(MemoryVault::default());
        let service =
            MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live)
            .unwrap();
        service
            .capture_current_account("codex", None, None)
            .unwrap();
        let reference = service.store.load().unwrap().apps["codex"].profiles["account-1"]
            .secret_files["auth.json"]
            .clone();
        let newer_saved = json!({
            "auth_mode":"chatgpt",
            "last_refresh":"2026-09-05T12:00:00Z",
            "tokens":{"account_id":"account-a","access_token":"newer-saved"}
        });
        vault
            .set(&reference, &serde_json::to_vec(&newer_saved).unwrap())
            .unwrap();

        service.sync_active_account("codex").unwrap();

        assert_eq!(
            serde_json::from_slice::<Value>(&vault.get(&reference).unwrap()).unwrap(),
            newer_saved
        );
    }

    #[test]
    fn generic_codex_profiles_are_rejected_without_leaving_storage() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        fs::write(
            live.join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live)
            .unwrap();

        assert_eq!(
            service
                .add_profile("codex", Some("manual"), crate::ProfileInput::default())
                .unwrap_err()
                .code,
            ErrorCode::MixCodexFileAuthRequired,
        );
        assert_eq!(
            service
                .capture_profile("codex", Some("imported"), &["config.toml".into()], None)
                .unwrap_err()
                .code,
            ErrorCode::MixCodexFileAuthRequired,
        );
        assert!(!root.path().join("mix/profiles/codex/imported").exists());
    }

    #[test]
    fn codex_capture_rejects_literal_credentials_without_leaving_storage() {
        let unsafe_configs = [
            "[model_providers.gateway]\nexperimental_bearer_token = \"secret-value\"\n",
            "[model_providers.gateway.http_headers]\nAuthorization = \"secret-value\"\n",
            "[model_providers.gateway.query_params]\napi_key = \"secret-value\"\n",
            "[mcp_servers.company.env]\nCOMPANY_TOKEN = \"secret-value\"\n",
            "[shell_environment_policy.set]\nCOMPANY_TOKEN = \"secret-value\"\n",
        ];

        for config in unsafe_configs {
            let root = tempfile::tempdir().unwrap();
            let live = root.path().join("codex");
            fs::create_dir_all(&live).unwrap();
            fs::write(live.join("config.toml"), config).unwrap();
            fs::write(
                live.join("auth.json"),
                r#"{"tokens":{"account_id":"account-a","access_token":"auth-secret"}}"#,
            )
            .unwrap();
            let vault = Arc::new(MemoryVault::default());
            let service =
                MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
            service.initialize().unwrap();
            service
                .register_client("codex", AdapterKind::Codex, live)
                .unwrap();

            let error = service
                .capture_current_account("codex", None, None)
                .expect_err("literal credential surfaces must fail closed");

            assert_eq!(error.code, ErrorCode::MixSensitiveDataRejected);
            assert!(!error.to_string().contains("secret-value"));
            assert!(!error.to_string().contains("auth-secret"));
            assert!(service.store.load().unwrap().apps["codex"]
                .profiles
                .is_empty());
            assert!(!root.path().join("mix/profiles/codex/account-1").exists());
            assert!(vault.is_empty());
        }
    }

    #[test]
    fn codex_capture_preserves_safe_environment_references() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        fs::write(
            live.join("config.toml"),
            "model_provider = \"gateway\"\n[model_providers.gateway]\nname = \"Acme Gateway\"\nenv_key = \"GATEWAY_API_KEY\"\n[model_providers.gateway.env_http_headers]\nX-Token = \"GATEWAY_HEADER\"\n[mcp_servers.company]\nbearer_token_env_var = \"MCP_TOKEN\"\nenv_vars = [\"MCP_EXTRA\"]\n",
        )
        .unwrap();
        fs::write(
            live.join("auth.json"),
            r#"{"tokens":{"account_id":"account-a","access_token":"auth-secret"}}"#,
        )
        .unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live)
            .unwrap();

        service
            .capture_current_account("codex", None, None)
            .expect("environment variable names are safe to persist");

        let stored = service.store.load().unwrap();
        let client = &stored.apps["codex"];
        let profile = &client.profiles["account-1"];
        let overlay = fs::read_to_string(&profile.files["config.toml"]).unwrap();
        assert!(overlay.contains("GATEWAY_API_KEY"));
        assert!(overlay.contains("GATEWAY_HEADER"));
        assert!(!overlay.contains("MCP_TOKEN"));
        assert!(!overlay.contains("MCP_EXTRA"));
        assert!(!overlay.contains("cli_auth_credentials_store"));
        assert!(!overlay.contains("auth-secret"));

        let effective =
            String::from_utf8(codex_materialized_config(client, profile).unwrap()).unwrap();
        assert!(effective.contains("GATEWAY_API_KEY"));
        assert!(effective.contains("GATEWAY_HEADER"));
        assert!(effective.contains("MCP_TOKEN"));
        assert!(effective.contains("MCP_EXTRA"));
        assert!(effective.contains("cli_auth_credentials_store = \"file\""));
        assert_eq!(
            service.state().unwrap().apps[0].profiles[0]
                .provider
                .as_ref()
                .map(|provider| provider.name.as_str()),
            Some("Acme Gateway")
        );
    }

    #[test]
    fn codex_materialized_config_keeps_shared_settings_and_applies_target_provider() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        let stored = root.path().join("target.toml");
        fs::create_dir_all(&live).unwrap();
        fs::write(
            live.join("config.toml"),
            r#"
model = "current-model"
model_provider = "current"
notify = ["current-notifier"]

[model_providers.current]
name = "Current"
base_url = "https://current.example"

[model_providers.unused]
name = "Reusable definition"
base_url = "https://unused.example"

[mcp_servers.company]
command = "company-mcp"

[features]
multi_agent = true

[plugins.latest]
enabled = true

[projects."/work/latest"]
trust_level = "trusted"
"#,
        )
        .unwrap();
        fs::write(
            &stored,
            r#"
model = "target-model"
model_provider = "target"
notify = ["stale-notifier"]

[model_providers.target]
name = "Target"
base_url = "https://target.example"
env_key = "TARGET_API_KEY"

[mcp_servers.stale]
command = "stale-mcp"

[features]
multi_agent = false

[plugins.stale]
enabled = true
"#,
        )
        .unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: live,
            active_profile: None,
            profiles: BTreeMap::new(),
            run: RunSpec::default(),
        };
        let profile = Profile {
            files: BTreeMap::from([("config.toml".into(), stored)]),
            ..Profile::default()
        };

        let effective = codex_materialized_config(&client, &profile).unwrap();
        let document =
            toml::from_str::<toml::Table>(std::str::from_utf8(&effective).unwrap()).unwrap();

        assert_eq!(document["model"].as_str(), Some("target-model"));
        assert_eq!(document["model_provider"].as_str(), Some("target"));
        assert_eq!(
            document["cli_auth_credentials_store"].as_str(),
            Some("file")
        );
        assert_eq!(document["notify"][0].as_str(), Some("current-notifier"));
        assert!(document["mcp_servers"].get("company").is_some());
        assert!(document["mcp_servers"].get("stale").is_none());
        assert_eq!(document["features"]["multi_agent"].as_bool(), Some(true));
        assert!(document["plugins"].get("latest").is_some());
        assert!(document["plugins"].get("stale").is_none());
        assert!(document["projects"].get("/work/latest").is_some());
        assert!(document["model_providers"].get("current").is_some());
        assert!(document["model_providers"].get("unused").is_some());
        assert_eq!(
            document["model_providers"]["target"]["env_key"].as_str(),
            Some("TARGET_API_KEY")
        );
    }

    #[test]
    fn codex_official_account_overlay_clears_the_active_custom_provider() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        let stored = root.path().join("official.toml");
        fs::create_dir_all(&live).unwrap();
        fs::write(
            live.join("config.toml"),
            "model_provider = \"gateway\"\n[model_providers.gateway]\nname = \"Gateway\"\nbase_url = \"https://gateway.example\"\n[mcp_servers.company]\ncommand = \"company-mcp\"\n",
        )
        .unwrap();
        fs::write(&stored, "model = \"official-model\"\n").unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: live,
            active_profile: None,
            profiles: BTreeMap::new(),
            run: RunSpec::default(),
        };
        let profile = Profile {
            files: BTreeMap::from([("config.toml".into(), stored)]),
            ..Profile::default()
        };

        let effective = codex_materialized_config(&client, &profile).unwrap();
        let document =
            toml::from_str::<toml::Table>(std::str::from_utf8(&effective).unwrap()).unwrap();

        assert!(document.get("model_provider").is_none());
        assert!(document["model_providers"].get("gateway").is_some());
        assert!(document["mcp_servers"].get("company").is_some());
        assert_eq!(document["model"].as_str(), Some("official-model"));
    }

    #[test]
    fn codex_official_account_removes_a_custom_definition_named_openai() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        let stored = root.path().join("official.toml");
        fs::create_dir_all(&live).unwrap();
        fs::write(
            live.join("config.toml"),
            "model_provider = \"openai\"\n[model_providers.openai]\nname = \"Gateway\"\nbase_url = \"https://gateway.example/v1\"\n",
        )
        .unwrap();
        fs::write(&stored, "model = \"official-model\"\n").unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: live,
            active_profile: None,
            profiles: BTreeMap::new(),
            run: RunSpec::default(),
        };
        let profile = Profile {
            files: BTreeMap::from([("config.toml".into(), stored)]),
            ..Profile::default()
        };

        let effective = codex_materialized_config(&client, &profile).unwrap();
        let document =
            toml::from_str::<toml::Table>(std::str::from_utf8(&effective).unwrap()).unwrap();

        assert!(document.get("model_provider").is_none());
        assert!(document
            .get("model_providers")
            .and_then(toml::Value::as_table)
            .is_none_or(|providers| !providers.contains_key("openai")));
        assert_eq!(provider_from_toml(&document).id, "openai");
        assert!(provider_from_toml(&document).official);
    }

    #[test]
    fn codex_projection_verification_covers_every_provider_transition() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        let stored = root.path().join("target.toml");
        fs::create_dir_all(&live).unwrap();
        let auth = json!({"tokens":{"account_id":"target-account","access_token":"token"}});
        fs::write(live.join("auth.json"), serde_json::to_vec(&auth).unwrap()).unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: live.clone(),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: RunSpec::default(),
        };
        let mut profile = Profile {
            account_fingerprint: crate::CodexAdapter::fingerprint(&auth),
            files: BTreeMap::from([("config.toml".into(), stored.clone())]),
            ..Profile::default()
        };
        let cases = [
            (
                "model = \"official-model\"\n",
                "model_provider = \"gateway-a\"\n[model_providers.gateway-a]\nbase_url = \"https://a.example\"\n",
            ),
            (
                "model_provider = \"gateway-a\"\n[model_providers.gateway-a]\nbase_url = \"https://a.example\"\n",
                "model = \"official-model\"\n",
            ),
            (
                "model_provider = \"gateway-b\"\n[model_providers.gateway-b]\nbase_url = \"https://b.example\"\n",
                "model_provider = \"gateway-a\"\n[model_providers.gateway-a]\nbase_url = \"https://a.example\"\n",
            ),
            (
                "model_provider = \"gateway-a\"\n[model_providers.gateway-a]\nbase_url = \"https://new.example\"\n",
                "model_provider = \"gateway-a\"\n[model_providers.gateway-a]\nbase_url = \"https://stale.example\"\n",
            ),
        ];

        for (target, wrong_live) in cases {
            fs::write(&stored, target).unwrap();
            fs::write(live.join("config.toml"), wrong_live).unwrap();
            let error = verify_live_projection(&client, &profile).unwrap_err();
            assert_eq!(error.code, ErrorCode::MixSwitchVerificationFailed);

            fs::write(live.join("config.toml"), target).unwrap();
            verify_live_projection(&client, &profile).unwrap();
        }

        profile.account_fingerprint = Some("wrong-account".into());
        let error = verify_live_projection(&client, &profile).unwrap_err();
        assert_eq!(error.code, ErrorCode::MixSwitchVerificationFailed);
    }

    #[test]
    fn isolated_official_login_never_inherits_the_current_custom_provider() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        fs::write(
            &config,
            r#"
model = "gateway-model"
model_provider = "gateway"
disable_response_storage = true

[model_providers.gateway]
name = "Gateway"
base_url = "https://gateway.example"

[mcp_servers.company]
command = "company-mcp"

[features]
multi_agent = true
"#,
        )
        .unwrap();

        let isolated =
            toml::from_str::<toml::Table>(&codex_login_config(&config).unwrap()).unwrap();

        assert!(isolated.get("model").is_none());
        assert!(isolated.get("model_provider").is_none());
        assert!(isolated.get("disable_response_storage").is_none());
        assert!(isolated["model_providers"].get("gateway").is_some());
        assert!(isolated["mcp_servers"].get("company").is_some());
        assert_eq!(isolated["features"]["multi_agent"].as_bool(), Some(true));
        assert_eq!(
            isolated["cli_auth_credentials_store"].as_str(),
            Some("file")
        );
    }

    #[test]
    fn claude_import_rejects_auth_material_without_leaving_profile_storage() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("claude");
        fs::create_dir_all(&live).unwrap();
        fs::write(
            live.join("settings.json"),
            r#"{"env":{"ANTHROPIC_API_KEY":"sk-secret"}}"#,
        )
        .unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("claude", AdapterKind::Claude, live)
            .unwrap();

        let error = service
            .capture_profile("claude", None, &["settings.json".into()], None)
            .expect_err("Claude auth material must fail closed");
        assert_eq!(error.code, ErrorCode::MixSensitiveDataRejected);
        assert!(!root
            .path()
            .join("mix/profiles/claude/environment-1")
            .exists());
    }

    #[test]
    fn claude_environment_names_are_generated_by_core() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("claude");
        fs::create_dir_all(&live).unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("claude", AdapterKind::Claude, live)
            .unwrap();

        let result = service
            .add_profile(
                "claude",
                None,
                crate::ProfileInput {
                    label: Some("Work".into()),
                    auth_strategy: Some(crate::AuthStrategy::Interactive),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result["profile"], "environment-1");
    }

    #[test]
    fn enrolling_an_existing_account_refreshes_its_vault_credential() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        let vault = Arc::new(MemoryVault::default());
        let service =
            MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live)
            .unwrap();
        let document = json!({
            "email":"fresh@example.com",
            "tokens":{"account_id":"account-a","access_token":"fresh"}
        });
        let fingerprint = crate::CodexAdapter::fingerprint(&document).unwrap();
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "account-a".into(),
        };
        let stored_config = root.path().join("stored-account.toml");
        fs::write(&stored_config, "").unwrap();
        vault.set(&reference, b"stale").unwrap();
        service
            .store
            .mutate(|config| {
                config.apps.get_mut("codex").unwrap().profiles.insert(
                    "account-1".into(),
                    Profile {
                        label: "Account".into(),
                        files: BTreeMap::from([("config.toml".into(), stored_config.clone())]),
                        secret_files: BTreeMap::from([("auth.json".into(), reference.clone())]),
                        account_fingerprint: Some(fingerprint),
                        ..Profile::default()
                    },
                );
                Ok(())
            })
            .unwrap();
        let mix_root = service.store.root().unwrap();
        let id = Uuid::new_v4().to_string();
        let temp_dir = enrollment_directory(mix_root).join(&id);
        private_scoped_dir(&temp_dir, &enrollment_directory(mix_root)).unwrap();
        fs::write(
            temp_dir.join("auth.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        let record = EnrollmentRecord {
            version: ENROLLMENT_VERSION,
            id: id.clone(),
            app: "codex".into(),
            mode: "add".into(),
            repair_profile: None,
            temp_dir,
            created_at: chrono::Utc::now().to_rfc3339(),
            state: "pending".into(),
            profile: None,
            error: None,
            code: None,
        };
        write_enrollment(mix_root, &record).unwrap();

        let status = service.account_enrollment_status(&id).unwrap();
        assert_eq!(status["state"], "completed");
        assert_eq!(
            serde_json::from_slice::<Value>(&vault.get(&reference).unwrap()).unwrap(),
            document
        );
        assert_eq!(
            service.store.load().unwrap().apps["codex"].profiles["account-1"]
                .account_identity
                .as_ref()
                .and_then(|identity| identity.email.as_deref()),
            Some("fresh@example.com")
        );
    }

    #[test]
    fn active_login_repair_refreshes_credential_identity_and_route_overlay() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        let stored = root.path().join("stored.toml");
        fs::create_dir_all(&live).unwrap();
        fs::write(
            live.join("config.toml"),
            r#"cli_auth_credentials_store = "file"
model = "current-model"
model_provider = "company-b"

[model_providers.company-b]
base_url = "https://company-b.example"
"#,
        )
        .unwrap();
        fs::write(&stored, "model = \"stale-model\"\n").unwrap();
        let document = json!({
            "email":"fresh@example.com",
            "tokens":{"account_id":"account-a","access_token":"fresh"}
        });
        fs::write(
            live.join("auth.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        let fingerprint = crate::CodexAdapter::fingerprint(&document).unwrap();
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "account-a".into(),
        };
        let vault = Arc::new(MemoryVault::default());
        vault.set(&reference, b"stale").unwrap();
        let service =
            MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live)
            .unwrap();
        service
            .store
            .mutate(|config| {
                config.apps.get_mut("codex").unwrap().profiles.insert(
                    "account-1".into(),
                    Profile {
                        label: "Work".into(),
                        label_origin: ProfileLabelOrigin::User,
                        files: BTreeMap::from([("config.toml".into(), stored.clone())]),
                        secret_files: BTreeMap::from([("auth.json".into(), reference.clone())]),
                        account_fingerprint: Some(fingerprint),
                        ..Profile::default()
                    },
                );
                Ok(())
            })
            .unwrap();

        service.repair_active_login("codex", "account-1").unwrap();

        assert_eq!(
            serde_json::from_slice::<Value>(&vault.get(&reference).unwrap()).unwrap(),
            document
        );
        let config = service.store.load().unwrap();
        let profile = &config.apps["codex"].profiles["account-1"];
        assert_eq!(
            profile
                .account_identity
                .as_ref()
                .and_then(|identity| identity.email.as_deref()),
            Some("fresh@example.com")
        );
        assert_eq!(profile.label, "Work");
        assert_eq!(profile.label_origin, ProfileLabelOrigin::User);
        let overlay = read_codex_config(&stored).unwrap();
        assert_eq!(overlay["model_provider"].as_str(), Some("company-b"));
        assert_eq!(
            overlay["model_providers"]["company-b"]["base_url"].as_str(),
            Some("https://company-b.example")
        );
    }

    #[test]
    fn enrollment_revalidates_isolated_config_before_persisting_it() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        let vault = Arc::new(MemoryVault::default());
        let service =
            MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live)
            .unwrap();
        let mix_root = service.store.root().unwrap();
        let id = Uuid::new_v4().to_string();
        let temp_dir = enrollment_directory(mix_root).join(&id);
        private_scoped_dir(&temp_dir, &enrollment_directory(mix_root)).unwrap();
        fs::write(
            temp_dir.join("auth.json"),
            r#"{"tokens":{"account_id":"account-a","access_token":"auth-secret"}}"#,
        )
        .unwrap();
        fs::write(
            temp_dir.join("config.toml"),
            "[model_providers.gateway]\nexperimental_bearer_token = \"config-secret\"\n",
        )
        .unwrap();
        let record = EnrollmentRecord {
            version: ENROLLMENT_VERSION,
            id: id.clone(),
            app: "codex".into(),
            mode: "add".into(),
            repair_profile: None,
            temp_dir: temp_dir.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            state: "pending".into(),
            profile: None,
            error: None,
            code: None,
        };
        write_enrollment(mix_root, &record).unwrap();

        let status = service.account_enrollment_status(&id).unwrap();

        assert_eq!(status["state"], "failed");
        assert_eq!(status["code"], ErrorCode::MixSensitiveDataRejected.as_str());
        assert!(!status["error"].as_str().unwrap().contains("config-secret"));
        assert!(!status["error"].as_str().unwrap().contains("auth-secret"));
        assert!(service.store.load().unwrap().apps["codex"]
            .profiles
            .is_empty());
        assert!(!root.path().join("mix/profiles/codex/account-1").exists());
        assert!(!temp_dir.exists());
        assert!(vault.is_empty());
    }

    #[test]
    fn claude_run_revalidates_saved_settings_before_creating_a_runtime() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("claude");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("claude", AdapterKind::Claude, live)
            .unwrap();
        service
            .capture_profile("claude", None, &["settings.json".into()], Some("Team"))
            .unwrap();
        let stored = service.store.load().unwrap();
        let settings =
            stored.apps["claude"].profiles["environment-1"].files["settings.json"].clone();
        fs::write(
            settings,
            r#"{"env":{"ANTHROPIC_API_KEY":"must-not-project"}}"#,
        )
        .unwrap();

        let error = service
            .run("claude", "environment-1", None)
            .expect_err("tampered settings must fail before runtime creation");

        assert_eq!(error.code, ErrorCode::MixSensitiveDataRejected);
        assert!(!error.to_string().contains("must-not-project"));
        assert!(!root.path().join("mix/runtime/claude").exists());
    }

    #[test]
    fn run_checks_the_executable_before_creating_a_runtime() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("claude");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("claude", AdapterKind::Claude, live)
            .unwrap();
        service
            .capture_profile("claude", None, &["settings.json".into()], Some("Team"))
            .unwrap();
        service
            .store
            .mutate(|config| {
                config.apps.get_mut("claude").unwrap().run.command =
                    vec![root.path().join("missing/claude").display().to_string()];
                Ok(())
            })
            .unwrap();

        let error = service
            .run("claude", "environment-1", None)
            .expect_err("missing executable must fail before runtime setup");

        assert_eq!(error.code, ErrorCode::MixNotFound);
        assert!(!root.path().join("mix/runtime").exists());
    }

    #[test]
    fn closed_enrollment_fails_immediately_and_can_be_rediscovered() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live)
            .unwrap();
        let mix_root = service.store.root().unwrap();
        let id = Uuid::new_v4().to_string();
        let temp_dir = enrollment_directory(mix_root).join(&id);
        private_scoped_dir(&temp_dir, &enrollment_directory(mix_root)).unwrap();
        fs::write(temp_dir.join(".mix-login-exit"), b"1\n").unwrap();
        let record = EnrollmentRecord {
            version: ENROLLMENT_VERSION,
            id: id.clone(),
            app: "codex".into(),
            mode: "add".into(),
            repair_profile: None,
            temp_dir: temp_dir.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            state: "pending".into(),
            profile: None,
            error: None,
            code: None,
        };
        write_enrollment(mix_root, &record).unwrap();
        assert_eq!(
            pending_enrollment(mix_root, "codex").unwrap().unwrap().id,
            id
        );

        let status = service.account_enrollment_status(&id).unwrap();
        assert_eq!(status["state"], "failed");
        assert_eq!(status["code"], "MIX_ENROLLMENT_CANCELLED");
        assert!(!temp_dir.exists());
    }

    #[test]
    fn expired_pending_enrollment_is_pruned_before_it_can_block_login() {
        let root = tempfile::tempdir().unwrap();
        let mix_root = root.path().join("mix");
        let id = Uuid::new_v4().to_string();
        let directory = enrollment_directory(&mix_root);
        let temp_dir = directory.join(&id);
        private_scoped_dir(&temp_dir, &directory).unwrap();
        fs::write(temp_dir.join("config.toml"), b"stale\n").unwrap();
        let record = EnrollmentRecord {
            version: ENROLLMENT_VERSION,
            id: id.clone(),
            app: "codex".into(),
            mode: "repair".into(),
            repair_profile: Some("account-1".into()),
            temp_dir: temp_dir.clone(),
            created_at: (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339(),
            state: "pending".into(),
            profile: None,
            error: None,
            code: None,
        };
        write_enrollment(&mix_root, &record).unwrap();

        assert!(pending_enrollment(&mix_root, "codex").unwrap().is_none());
        assert!(!temp_dir.exists());
        assert!(!directory.join(format!("{id}.json")).exists());
    }

    #[test]
    fn enrollment_record_cannot_reference_another_temporary_directory() {
        let root = tempfile::tempdir().unwrap();
        let mix_root = root.path().join("mix");
        let id = Uuid::new_v4().to_string();
        let directory = enrollment_directory(&mix_root);
        fs::create_dir_all(&directory).unwrap();
        let record = EnrollmentRecord {
            version: ENROLLMENT_VERSION,
            id: id.clone(),
            app: "codex".into(),
            mode: "add".into(),
            repair_profile: None,
            temp_dir: directory.join("another-enrollment"),
            created_at: chrono::Utc::now().to_rfc3339(),
            state: "pending".into(),
            profile: None,
            error: None,
            code: None,
        };
        fs::write(
            directory.join(format!("{id}.json")),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();

        let error = read_enrollment(&mix_root, &id).unwrap_err();
        assert_eq!(error.code, ErrorCode::MixLocalFailure);
    }

    #[test]
    fn enrollment_record_rejects_unknown_fields() {
        let root = tempfile::tempdir().unwrap();
        let mix_root = root.path().join("mix");
        let id = Uuid::new_v4().to_string();
        let directory = enrollment_directory(&mix_root);
        let temp_dir = directory.join(&id);
        fs::create_dir_all(&temp_dir).unwrap();
        let record = EnrollmentRecord {
            version: ENROLLMENT_VERSION,
            id: id.clone(),
            app: "codex".into(),
            mode: "add".into(),
            repair_profile: None,
            temp_dir,
            created_at: chrono::Utc::now().to_rfc3339(),
            state: "pending".into(),
            profile: None,
            error: None,
            code: None,
        };
        let mut document = serde_json::to_value(record).unwrap();
        document["unexpected"] = Value::Bool(true);
        fs::write(
            directory.join(format!("{id}.json")),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();

        let error = read_enrollment(&mix_root, &id).unwrap_err();

        assert_eq!(error.code, ErrorCode::MixLocalFailure);
    }

    #[test]
    fn the_only_active_environment_can_be_removed_without_touching_live_state() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("client");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("native-history.jsonl"), b"history").unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("client", AdapterKind::Claude, live.clone())
            .unwrap();
        fs::write(
            root.path().join("stored-settings.json"),
            br#"{"theme":"dark"}"#,
        )
        .unwrap();
        service
            .add_profile(
                "client",
                Some("account-1"),
                crate::ProfileInput {
                    files: BTreeMap::from([(
                        "settings.json".into(),
                        root.path().join("stored-settings.json"),
                    )]),
                    ..crate::ProfileInput::default()
                },
            )
            .unwrap();
        let profile_id = service.store.load().unwrap().apps["client"].profiles["account-1"]
            .id
            .clone();
        let runtime = service
            .store
            .root()
            .unwrap()
            .join(format!("runtime/client/{profile_id}/project"));
        fs::create_dir_all(runtime.join("projects")).unwrap();
        fs::write(runtime.join("settings.json"), b"projected").unwrap();
        fs::write(runtime.join("projects/session.jsonl"), b"history").unwrap();
        fs::write(
            runtime.join(".mix-runtime.json"),
            serde_json::to_vec(&json!({
                "product":"mix",
                "app":"client",
                "profile":"account-1",
                "profile_id":profile_id,
                "runtime":runtime,
            }))
            .unwrap(),
        )
        .unwrap();
        service.delete_profile("client", "account-1", true).unwrap();
        assert!(service.state().unwrap().apps[0].profiles.is_empty());
        assert!(!runtime.join("settings.json").exists());
        assert!(runtime.join("projects/session.jsonl").exists());
        assert!(service.store.load().unwrap().pending_cleanup.is_empty());
        assert_eq!(
            fs::read(live.join("native-history.jsonl")).unwrap(),
            b"history"
        );
    }

    #[test]
    fn deleting_a_codex_account_uses_the_observed_login_not_stale_saved_state() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        fs::write(
            live.join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();
        let first = json!({"tokens":{"account_id":"account-a","access_token":"a"}});
        let second = json!({"tokens":{"account_id":"account-b","access_token":"b"}});
        fs::write(live.join("auth.json"), serde_json::to_vec(&first).unwrap()).unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live.clone())
            .unwrap();
        service
            .capture_current_account("codex", None, Some("First"))
            .unwrap();
        fs::write(live.join("auth.json"), serde_json::to_vec(&second).unwrap()).unwrap();
        service
            .capture_current_account("codex", None, Some("Second"))
            .unwrap();
        service
            .store
            .mutate(|config| {
                config.apps.get_mut("codex").unwrap().active_profile = Some("account-1".into());
                Ok(())
            })
            .unwrap();

        let error = service
            .delete_profile("codex", "account-2", true)
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::MixConflict);
        assert_eq!(
            service.store.load().unwrap().apps["codex"].profiles.len(),
            2
        );
        assert_eq!(
            crate::CodexAdapter::parse_auth(&live.join("auth.json")).unwrap(),
            second
        );

        service.delete_profile("codex", "account-1", true).unwrap();
        let config = service.store.load().unwrap();
        assert_eq!(config.apps["codex"].profiles.len(), 1);
        assert!(config.apps["codex"].profiles.contains_key("account-2"));
        assert_eq!(config.apps["codex"].active_profile, None);
        assert_eq!(
            crate::CodexAdapter::parse_auth(&live.join("auth.json")).unwrap(),
            second
        );
    }

    #[test]
    fn failed_credential_deletion_is_persisted_and_retried() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("claude");
        fs::create_dir_all(&live).unwrap();
        let vault = Arc::new(MemoryVault::default());
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "retired-environment".into(),
        };
        vault.set(&reference, b"secret").unwrap();
        let service =
            MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
        service.initialize().unwrap();
        service
            .register_client("claude", AdapterKind::Claude, live)
            .unwrap();
        service
            .add_profile(
                "claude",
                Some("retired"),
                crate::ProfileInput {
                    secrets: BTreeMap::from([("ANTHROPIC_API_KEY".into(), reference.clone())]),
                    ..crate::ProfileInput::default()
                },
            )
            .unwrap();

        vault.set_delete_failure(true);
        let result = service.delete_profile("claude", "retired", true).unwrap();
        assert!(result["warnings"].is_array());
        assert_eq!(
            service
                .store
                .load()
                .unwrap()
                .pending_cleanup
                .credentials
                .len(),
            1
        );

        vault.set_delete_failure(false);
        service.initialize().unwrap();
        assert!(service.store.load().unwrap().pending_cleanup.is_empty());
        assert_eq!(
            vault.get(&reference).unwrap_err().code,
            ErrorCode::MixCredentialNotFound
        );
    }

    #[test]
    fn deleting_one_profile_never_removes_a_shared_credential() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("claude");
        fs::create_dir_all(&live).unwrap();
        let vault = Arc::new(MemoryVault::default());
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "shared".into(),
        };
        vault.set(&reference, b"secret").unwrap();
        let service =
            MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
        service.initialize().unwrap();
        service
            .register_client("claude", AdapterKind::Claude, live)
            .unwrap();
        for name in ["first", "second"] {
            service
                .add_profile(
                    "claude",
                    Some(name),
                    crate::ProfileInput {
                        secrets: BTreeMap::from([("ANTHROPIC_API_KEY".into(), reference.clone())]),
                        ..crate::ProfileInput::default()
                    },
                )
                .unwrap();
        }

        service.delete_profile("claude", "second", true).unwrap();
        assert_eq!(vault.get(&reference).unwrap(), b"secret");
        assert!(service.store.load().unwrap().pending_cleanup.is_empty());
    }

    #[test]
    fn runtime_profile_accepts_vault_backed_environment() {
        let profile = Profile {
            label: "provider".into(),
            env: BTreeMap::from([(String::from("MIX_MODE"), String::from("work"))]),
            secrets: BTreeMap::from([(
                String::from("PROVIDER_API_KEY"),
                SecretRef {
                    service: "com.mix.provider".into(),
                    account: "work".into(),
                },
            )]),
            ..Profile::default()
        };
        validate_profile_runtime(&profile).unwrap();
    }

    #[test]
    fn runtime_profile_rejects_shell_unsafe_environment_names() {
        let profile = Profile {
            label: "provider".into(),
            env: BTreeMap::from([(String::from("PROVIDER-API-KEY"), String::from("x"))]),
            ..Profile::default()
        };
        assert!(validate_profile_runtime(&profile).is_err());
    }

    #[test]
    fn runtime_profile_rejects_plaintext_credential_environment_values() {
        for name in [
            "ANTHROPIC_API_KEY",
            "access_token",
            "CLIENT_SECRET",
            "DATABASE_PASSWORD",
            "AUTHORIZATION",
            "AWS_ACCESS_KEY_ID",
            "SSH_PRIVATE_KEY",
            "OPENAI_KEY",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            let profile = Profile {
                label: "provider".into(),
                env: BTreeMap::from([(name.into(), "must-use-vault".into())]),
                ..Profile::default()
            };
            let error = validate_profile_runtime(&profile)
                .expect_err("credential-like variables must use vault references");
            assert_eq!(error.code, ErrorCode::MixSensitiveDataRejected);
            assert!(!error.to_string().contains("must-use-vault"));
        }
        for name in ["AUTH_MODE", "SSH_AUTH_SOCK", "PUBLIC_KEY"] {
            let profile = Profile {
                label: "provider".into(),
                env: BTreeMap::from([(name.into(), "non-secret".into())]),
                ..Profile::default()
            };
            validate_profile_runtime(&profile).unwrap();
        }
    }

    #[test]
    fn runtime_environment_rejects_null_bytes_from_the_vault() {
        let vault = MemoryVault::default();
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "null-byte".into(),
        };
        vault.set(&reference, b"before\0after").unwrap();
        let profile = Profile {
            label: "provider".into(),
            secrets: BTreeMap::from([("PROVIDER_API_KEY".into(), reference)]),
            ..Profile::default()
        };
        assert!(profile_environment(&profile, &vault).is_err());
    }

    #[test]
    fn native_claude_resume_uses_the_project_or_active_environment() {
        let client = ClientConfig {
            adapter: AdapterKind::Claude,
            live_dir: PathBuf::from("/tmp/claude"),
            active_profile: Some("personal".into()),
            profiles: BTreeMap::new(),
            run: RunSpec::default(),
        };
        let claude = crate::ClaudeAdapter;
        assert_eq!(
            claude.native_session_environment_profile(&client, Some("work")),
            Some("work".into())
        );
        assert_eq!(
            claude.native_session_environment_profile(&client, None),
            Some("personal".into())
        );
        assert_eq!(
            crate::CodexAdapter.native_session_environment_profile(&client, Some("work")),
            None
        );
    }

    #[test]
    fn runtime_identity_rejects_a_reused_profile_name_for_another_account() {
        let root = tempfile::tempdir().unwrap();
        let old = json!({"tokens":{"account_id":"old-account"}});
        let new = json!({"tokens":{"account_id":"new-account"}});
        fs::write(
            root.path().join("auth.json"),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        let profile = Profile {
            label: "Reused name".into(),
            account_fingerprint: crate::CodexAdapter::fingerprint(&new),
            ..Profile::default()
        };
        assert!(!runtime_identity_matches(
            &crate::CodexAdapter,
            &profile,
            root.path(),
            Some(&profile.id),
            None,
        ));
    }

    #[test]
    fn runtime_identity_survives_normal_plaintext_credential_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let document = json!({"tokens":{"account_id":"account-a"}});
        let fingerprint = crate::CodexAdapter::fingerprint(&document).unwrap();
        let profile = Profile {
            label: "Account".into(),
            account_fingerprint: Some(fingerprint.clone()),
            ..Profile::default()
        };
        assert!(runtime_identity_matches(
            &crate::CodexAdapter,
            &profile,
            root.path(),
            Some(&profile.id),
            Some(&fingerprint),
        ));
    }

    #[test]
    fn runtime_identity_rejects_a_recreated_claude_environment() {
        let root = tempfile::tempdir().unwrap();
        let old = Profile {
            label: "Team".into(),
            ..Profile::default()
        };
        let replacement = Profile {
            label: "Team".into(),
            ..Profile::default()
        };
        assert_ne!(old.id, replacement.id);
        assert!(!runtime_identity_matches(
            &crate::ClaudeAdapter,
            &replacement,
            root.path(),
            Some(&old.id),
            None,
        ));
    }

    #[test]
    fn missing_profile_identities_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        service.store.initialize().unwrap();
        let error = service
            .store
            .mutate(|config| {
                config.apps.insert(
                    "claude".into(),
                    ClientConfig {
                        adapter: AdapterKind::Claude,
                        live_dir: root.path().join("claude"),
                        active_profile: Some("team".into()),
                        profiles: BTreeMap::from([(
                            "team".into(),
                            Profile {
                                id: String::new(),
                                label: "Team".into(),
                                ..Profile::default()
                            },
                        )]),
                        run: RunSpec {
                            command: vec!["claude".into()],
                        },
                    },
                );
                Ok(())
            })
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::MixConfigInvalid);
    }

    #[test]
    fn recovery_refuses_to_mutate_live_state_when_process_stop_is_unverifiable() {
        let controller = ProcessController::new(Some(crate::process::ProcessSpec {
            executable: Some(PathBuf::from("relative-client")),
            ..crate::process::ProcessSpec::default()
        }));
        let error = stop_before_recovery(&controller).expect_err("unsafe recovery must fail");
        assert_eq!(error.code, ErrorCode::MixSwitchRecoveryFailed);
        assert!(error.to_string().contains("journal was preserved"));
    }

    #[test]
    fn recovery_refuses_a_journal_for_a_different_live_directory() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live-a");
        let replacement = root.path().join("live-b");
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&replacement).unwrap();
        fs::write(live.join("config.toml"), b"old").unwrap();
        let source = root.path().join("new.toml");
        fs::write(&source, b"new").unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live.clone())
            .unwrap();
        let target = Profile {
            label: "new".into(),
            files: BTreeMap::from([("config.toml".into(), source)]),
            ..Profile::default()
        };
        let mix_root = service.store.root().unwrap();
        let journal = SwitchJournal::prepare(
            mix_root,
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
            service.vault.as_ref(),
        )
        .unwrap();
        journal.apply(service.vault.as_ref()).unwrap();
        service
            .store
            .mutate(|config| {
                config.apps.get_mut("codex").unwrap().live_dir = replacement;
                Ok(())
            })
            .unwrap();

        let error = service.recover_interrupted_switch().unwrap_err();
        assert_eq!(error.code, ErrorCode::MixSwitchRecoveryFailed);
        assert_eq!(fs::read(live.join("config.toml")).unwrap(), b"new");
        assert!(SwitchJournal::path(mix_root).is_file());
    }

    #[test]
    fn discovered_cli_is_never_treated_as_a_process_owned_by_mix() {
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: PathBuf::from("/tmp/codex"),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: RunSpec {
                command: vec![std::env::current_exe().unwrap().display().to_string()],
            },
        };
        let adapters = AdapterRegistry::default();
        assert!(adapters
            .get(client.adapter)
            .desktop_process(&client)
            .is_none());
    }

    #[test]
    fn session_catalog_is_not_truncated_by_the_public_page_limit() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        let sessions = live.join("sessions/2026/09/02");
        fs::create_dir_all(&sessions).unwrap();
        for index in 0..501 {
            fs::write(
                sessions.join(format!("{index:04}.jsonl")),
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"session-{index:04}\",\"cwd\":{}}}}}\n",
                    serde_json::to_string(&root.path().display().to_string()).unwrap()
                ),
            )
            .unwrap();
        }
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live)
            .unwrap();
        service
            .store
            .mutate(|config| {
                let client = config.apps.get_mut("codex").unwrap();
                client.run.command = vec![std::env::current_exe().unwrap().display().to_string()];
                Ok(())
            })
            .unwrap();
        let config = service.store.load().unwrap();
        let catalog = service
            .session_catalog(&config, Some("codex"), None, None, None)
            .unwrap();
        assert_eq!(catalog.len(), 501);
        let first_page = service
            .sessions(Some("codex"), None, None, None, 500, 0)
            .unwrap();
        assert_eq!(first_page.len(), 500);
        assert!(first_page
            .iter()
            .all(|session| session.project_session_count == Some(501)));
        let projects = service.project_sessions().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].project_session_count, Some(501));
    }

    #[test]
    fn session_catalog_rejects_an_unknown_client_instead_of_looking_empty() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        let error = service
            .sessions(Some("missing"), None, None, None, 100, 0)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::MixNotFound);
    }

    #[test]
    fn live_session_resume_requires_the_registered_project_account() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        let project = root.path().join("project");
        let sessions = live.join("sessions/2026/09/02");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            sessions.join("session.jsonl"),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"session-1\",\"cwd\":{}}}}}\n",
                serde_json::to_string(&project.display().to_string()).unwrap()
            ),
        )
        .unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live)
            .unwrap();
        service
            .store
            .mutate(|config| {
                let client = config.apps.get_mut("codex").unwrap();
                client.run.command = vec![std::env::current_exe().unwrap().display().to_string()];
                client.profiles.insert(
                    "personal".into(),
                    Profile {
                        label: "Personal".into(),
                        ..Profile::default()
                    },
                );
                client.profiles.insert(
                    "work".into(),
                    Profile {
                        label: "Work".into(),
                        ..Profile::default()
                    },
                );
                client.active_profile = Some("personal".into());
                config.workspace_bindings.insert(
                    project.display().to_string(),
                    BTreeMap::from([("codex".into(), "work".into())]),
                );
                Ok(())
            })
            .unwrap();
        let session = service
            .sessions(Some("codex"), None, None, None, 10, 0)
            .unwrap()
            .remove(0);

        let error = service
            .resume_session("codex", &session.resume_id)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::MixConflict);
    }

    #[test]
    fn projects_must_exist_before_registration() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        let missing = root.path().join("missing");
        assert_eq!(
            service
                .bind_workspace(missing.clone(), BTreeMap::new(), None)
                .unwrap_err()
                .code,
            ErrorCode::MixNotFound
        );
        fs::create_dir_all(&missing).unwrap();
        service
            .bind_workspace(missing.clone(), BTreeMap::new(), None)
            .unwrap();
        assert_eq!(service.state().unwrap().workspaces.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn editing_or_deleting_a_project_removes_canonical_path_aliases() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let alias = root.path().join("project-alias");
        let live = root.path().join("claude");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&live).unwrap();
        symlink(&project, &alias).unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("claude", AdapterKind::Claude, live)
            .unwrap();
        for profile in ["one", "two"] {
            service
                .add_profile("claude", Some(profile), crate::ProfileInput::default())
                .unwrap();
        }
        service
            .store
            .mutate(|config| {
                config.workspace_bindings.insert(
                    project.display().to_string(),
                    BTreeMap::from([("claude".into(), "one".into())]),
                );
                config.workspace_bindings.insert(
                    alias.display().to_string(),
                    BTreeMap::from([("claude".into(), "two".into())]),
                );
                Ok(())
            })
            .unwrap();
        assert!(!service.state().unwrap().workspaces[0]
            .binding_conflicts
            .is_empty());

        service
            .bind_workspace(
                project.clone(),
                BTreeMap::from([("claude".into(), "one".into())]),
                Some("Project".into()),
            )
            .unwrap();
        let config = service.store.load().unwrap();
        assert_eq!(config.workspace_bindings.len(), 1);
        assert!(service.state().unwrap().workspaces[0]
            .binding_conflicts
            .is_empty());

        service
            .store
            .mutate(|config| {
                config.workspace_bindings.insert(
                    alias.display().to_string(),
                    BTreeMap::from([("claude".into(), "one".into())]),
                );
                Ok(())
            })
            .unwrap();
        service
            .delete_workspace(project.to_string_lossy().as_ref())
            .unwrap();
        assert!(service.store.load().unwrap().workspace_bindings.is_empty());
    }

    #[test]
    fn session_account_uses_the_most_specific_registered_project_binding() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("project");
        let nested = parent.join("packages/web");
        fs::create_dir_all(&nested).unwrap();
        let mut config = crate::Config::empty(root.path().join("mix"));
        config.workspace_bindings.insert(
            parent.display().to_string(),
            BTreeMap::from([("codex".into(), "personal".into())]),
        );
        config.workspace_bindings.insert(
            nested.display().to_string(),
            BTreeMap::from([("codex".into(), "work".into())]),
        );

        let workspaces = workspaces_from(&config);
        let project =
            registered_project_for(&workspaces, Some(&nested.join("src").display().to_string()))
                .unwrap();
        assert_eq!(
            project.path,
            fs::canonicalize(&nested).unwrap().display().to_string()
        );
        assert_eq!(
            project.bindings.get("codex").map(String::as_str),
            Some("work")
        );
    }

    #[test]
    fn missing_managed_account_credential_fails_before_switch_side_effects() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        let stored = root.path().join("stored");
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&stored).unwrap();
        fs::write(
            stored.join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();
        let document = json!({"tokens":{"account_id":"account-a"}});
        let fingerprint = crate::CodexAdapter::fingerprint(&document).unwrap();
        let service = service(root.path());
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live)
            .unwrap();
        service
            .store
            .mutate(|config| {
                let client = config.apps.get_mut("codex").unwrap();
                client.active_profile = Some("account-1".into());
                client.profiles.insert(
                    "account-1".into(),
                    Profile {
                        label: "Account".into(),
                        files: BTreeMap::from([("config.toml".into(), stored.join("config.toml"))]),
                        secret_files: BTreeMap::from([(
                            "auth.json".into(),
                            SecretRef {
                                service: "com.mix.test".into(),
                                account: "missing".into(),
                            },
                        )]),
                        account_fingerprint: Some(fingerprint),
                        ..Profile::default()
                    },
                );
                Ok(())
            })
            .unwrap();
        let error = service.switch("codex", "account-1").unwrap_err();
        assert_eq!(error.code, ErrorCode::MixAccountReauthRequired);
        assert!(!SwitchJournal::path(service.store.root().unwrap()).exists());
    }

    #[test]
    fn a_signed_out_codex_client_can_switch_to_a_saved_account() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        let stored = root.path().join("stored");
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&stored).unwrap();
        fs::write(
            stored.join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();
        let document = json!({"tokens":{"account_id":"account-a","access_token":"token"}});
        let fingerprint = crate::CodexAdapter::fingerprint(&document).unwrap();
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "account-a".into(),
        };
        let vault = Arc::new(MemoryVault::default());
        vault
            .set(&reference, &serde_json::to_vec(&document).unwrap())
            .unwrap();
        let service =
            MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live.clone())
            .unwrap();
        service
            .store
            .mutate(|config| {
                let client = config.apps.get_mut("codex").unwrap();
                client.active_profile = Some("account-1".into());
                client.profiles.insert(
                    "account-1".into(),
                    Profile {
                        label: "Account".into(),
                        files: BTreeMap::from([("config.toml".into(), stored.join("config.toml"))]),
                        secret_files: BTreeMap::from([("auth.json".into(), reference)]),
                        account_fingerprint: Some(fingerprint.clone()),
                        ..Profile::default()
                    },
                );
                Ok(())
            })
            .unwrap();

        let signed_out = service.state().unwrap();
        assert_eq!(signed_out.apps[0].active, None);
        assert_eq!(
            signed_out.apps[0].configured_active.as_deref(),
            Some("account-1")
        );
        let outcome = service.switch("codex", "account-1").unwrap();
        assert!(vault.read_count() > 0);

        assert_eq!(outcome.status, "switched");
        assert_eq!(outcome.from_profile, None);
        assert_eq!(
            crate::CodexAdapter::fingerprint(
                &crate::CodexAdapter::parse_auth(&live.join("auth.json")).unwrap()
            ),
            Some(fingerprint)
        );
    }

    #[test]
    fn codex_switch_updates_only_provider_metadata_and_preserves_session_content() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        let profiles = root.path().join("profiles");
        fs::create_dir_all(live.join("sessions/2026/09/05")).unwrap();
        fs::create_dir_all(&profiles).unwrap();
        let session = live.join("sessions/2026/09/05/rollout.jsonl");
        fs::write(&session, b"native-history\n").unwrap();
        fs::write(
            live.join("config.toml"),
            r#"
cli_auth_credentials_store = "file"
model = "current-model"
model_provider = "current"
notify = ["latest-notifier"]

[model_providers.current]
name = "Current"
base_url = "https://current.example"

[mcp_servers.latest]
command = "latest-mcp"

[features]
multi_agent = true

[plugins.latest]
enabled = true
"#,
        )
        .unwrap();
        let current_auth =
            json!({"tokens":{"account_id":"account-a","access_token":"current-token"}});
        let target_auth =
            json!({"tokens":{"account_id":"account-b","access_token":"target-token"}});
        fs::write(
            live.join("auth.json"),
            serde_json::to_vec(&current_auth).unwrap(),
        )
        .unwrap();
        let current_config = profiles.join("current.toml");
        let target_config = profiles.join("target.toml");
        fs::write(
            &current_config,
            "model = \"stale-current-model\"\nmodel_provider = \"current\"\n[model_providers.current]\nname = \"Current\"\nbase_url = \"https://current.example\"\n[mcp_servers.stale]\ncommand = \"stale-mcp\"\n",
        )
        .unwrap();
        fs::write(
            &target_config,
            "model = \"target-model\"\nmodel_auto_compact_token_limit = 120000\nmodel_auto_compact_token_limit_scope = \"body_after_prefix\"\nmodel_supports_reasoning_summaries = false\nmodel_provider = \"target\"\n[model_providers.target]\nname = \"Target\"\nbase_url = \"https://target.example\"\n[mcp_servers.stale]\ncommand = \"stale-mcp\"\n[features]\nmulti_agent = false\n[plugins.stale]\nenabled = true\n",
        )
        .unwrap();
        let current_reference = SecretRef {
            service: "com.mix.test".into(),
            account: "account-a".into(),
        };
        let target_reference = SecretRef {
            service: "com.mix.test".into(),
            account: "account-b".into(),
        };
        let vault = Arc::new(MemoryVault::default());
        vault
            .set(
                &current_reference,
                &serde_json::to_vec(&current_auth).unwrap(),
            )
            .unwrap();
        vault
            .set(
                &target_reference,
                &serde_json::to_vec(&target_auth).unwrap(),
            )
            .unwrap();
        let service = MixService::with_vault(root.path().join("mix/config.json"), vault).unwrap();
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live.clone())
            .unwrap();
        service
            .store
            .mutate(|config| {
                let client = config.apps.get_mut("codex").unwrap();
                client.active_profile = Some("current".into());
                client.profiles = BTreeMap::from([
                    (
                        "current".into(),
                        Profile {
                            label: "Current".into(),
                            files: BTreeMap::from([("config.toml".into(), current_config.clone())]),
                            secret_files: BTreeMap::from([("auth.json".into(), current_reference)]),
                            account_fingerprint: crate::CodexAdapter::fingerprint(&current_auth),
                            ..Profile::default()
                        },
                    ),
                    (
                        "target".into(),
                        Profile {
                            label: "Target".into(),
                            files: BTreeMap::from([("config.toml".into(), target_config.clone())]),
                            secret_files: BTreeMap::from([("auth.json".into(), target_reference)]),
                            account_fingerprint: crate::CodexAdapter::fingerprint(&target_auth),
                            ..Profile::default()
                        },
                    ),
                ]);
                Ok(())
            })
            .unwrap();

        let outcome = service.switch("codex", "target").unwrap();

        assert_eq!(outcome.status, "switched");
        assert_eq!(outcome.from_profile.as_deref(), Some("current"));
        assert_eq!(fs::read(&session).unwrap(), b"native-history\n");
        assert_eq!(
            crate::CodexAdapter::fingerprint(
                &crate::CodexAdapter::parse_auth(&live.join("auth.json")).unwrap()
            ),
            crate::CodexAdapter::fingerprint(&target_auth)
        );
        let effective = read_codex_config(&live.join("config.toml")).unwrap();
        assert_eq!(effective["model_provider"].as_str(), Some("target"));
        assert_eq!(effective["model"].as_str(), Some("target-model"));
        assert_eq!(
            effective["model_auto_compact_token_limit"].as_integer(),
            Some(120000)
        );
        assert_eq!(
            effective["model_auto_compact_token_limit_scope"].as_str(),
            Some("body_after_prefix")
        );
        assert_eq!(
            effective["model_supports_reasoning_summaries"].as_bool(),
            Some(false)
        );
        assert_eq!(effective["notify"][0].as_str(), Some("latest-notifier"));
        assert!(effective["mcp_servers"].get("latest").is_some());
        assert!(effective["mcp_servers"].get("stale").is_none());
        assert_eq!(effective["features"]["multi_agent"].as_bool(), Some(true));
        assert!(effective["plugins"].get("latest").is_some());
        assert!(effective["plugins"].get("stale").is_none());

        let refreshed_current = read_codex_config(&current_config).unwrap();
        assert_eq!(
            refreshed_current["model_provider"].as_str(),
            Some("current")
        );
        assert_eq!(refreshed_current["model"].as_str(), Some("current-model"));
        assert!(refreshed_current.get("mcp_servers").is_none());
        assert!(refreshed_current.get("plugins").is_none());
        assert!(refreshed_current.get("features").is_none());
    }

    #[test]
    fn codex_switch_keeps_every_credential_and_provider_route_paired() {
        let cases = [
            (
                "official-to-custom",
                "model = \"official\"\n",
                json!({"tokens":{"account_id":"official-a","access_token":"official-token"}}),
                "model_provider = \"gateway\"\n[model_providers.gateway]\nname = \"Gateway\"\nbase_url = \"https://gateway.example/v1\"\nwire_api = \"responses\"\n",
                json!({"OPENAI_API_KEY":"gateway-key"}),
                "gateway",
            ),
            (
                "official-to-required-auth-gateway",
                "model = \"official\"\n",
                json!({"tokens":{"account_id":"official-a","access_token":"official-token"}}),
                "model_provider = \"custom\"\n[model_providers.custom]\nname = \"Partner Gateway\"\nbase_url = \"https://partner-gateway.example/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n",
                json!({"OPENAI_API_KEY":"partner-gateway-key"}),
                "custom",
            ),
            (
                "custom-to-official",
                "model_provider = \"gateway\"\n[model_providers.gateway]\nname = \"Gateway\"\nbase_url = \"https://gateway.example/v1\"\nwire_api = \"responses\"\n",
                json!({"OPENAI_API_KEY":"gateway-key"}),
                "model = \"official\"\n",
                json!({"tokens":{"account_id":"official-b","access_token":"official-token"}}),
                "openai",
            ),
            (
                "custom-to-custom",
                "model_provider = \"gateway-a\"\n[model_providers.gateway-a]\nbase_url = \"https://a.example/v1\"\n",
                json!({"OPENAI_API_KEY":"gateway-a-key"}),
                "model_provider = \"gateway-b\"\n[model_providers.gateway-b]\nbase_url = \"https://b.example/v1\"\n",
                json!({"OPENAI_API_KEY":"gateway-b-key"}),
                "gateway-b",
            ),
            (
                "shared-credential-different-route",
                "model_provider = \"gateway-a\"\n[model_providers.gateway-a]\nbase_url = \"https://a.example/v1\"\n",
                json!({"OPENAI_API_KEY":"shared-key"}),
                "model_provider = \"gateway-b\"\n[model_providers.gateway-b]\nbase_url = \"https://b.example/v1\"\n",
                json!({"OPENAI_API_KEY":"shared-key"}),
                "gateway-b",
            ),
        ];

        for (name, source_config, source_auth, target_config, target_auth, target_provider) in cases
        {
            let root = tempfile::tempdir().unwrap();
            let live = root.path().join("codex");
            let profiles = root.path().join("profiles");
            fs::create_dir_all(&live).unwrap();
            fs::create_dir_all(&profiles).unwrap();
            fs::write(live.join("config.toml"), source_config).unwrap();
            fs::write(
                live.join("auth.json"),
                serde_json::to_vec(&source_auth).unwrap(),
            )
            .unwrap();

            let source_path = profiles.join("source.toml");
            let target_path = profiles.join("target.toml");
            fs::write(&source_path, source_config).unwrap();
            fs::write(&target_path, target_config).unwrap();
            let source_reference = SecretRef {
                service: "com.mix.test".into(),
                account: format!("{name}/source"),
            };
            let target_reference = SecretRef {
                service: "com.mix.test".into(),
                account: format!("{name}/target"),
            };
            let vault = Arc::new(MemoryVault::default());
            vault
                .set(
                    &source_reference,
                    &serde_json::to_vec(&source_auth).unwrap(),
                )
                .unwrap();
            vault
                .set(
                    &target_reference,
                    &serde_json::to_vec(&target_auth).unwrap(),
                )
                .unwrap();

            let service =
                MixService::with_vault(root.path().join("mix/config.json"), vault).unwrap();
            service.initialize().unwrap();
            service
                .register_client("codex", AdapterKind::Codex, live.clone())
                .unwrap();
            service
                .store
                .mutate(|config| {
                    let client = config.apps.get_mut("codex").unwrap();
                    client.active_profile = Some("source".into());
                    client.profiles = BTreeMap::from([
                        (
                            "source".into(),
                            Profile {
                                label: "Source".into(),
                                files: BTreeMap::from([("config.toml".into(), source_path)]),
                                secret_files: BTreeMap::from([(
                                    "auth.json".into(),
                                    source_reference,
                                )]),
                                account_fingerprint: crate::CodexAdapter::fingerprint(&source_auth),
                                ..Profile::default()
                            },
                        ),
                        (
                            "target".into(),
                            Profile {
                                label: "Target".into(),
                                files: BTreeMap::from([("config.toml".into(), target_path)]),
                                secret_files: BTreeMap::from([(
                                    "auth.json".into(),
                                    target_reference,
                                )]),
                                account_fingerprint: crate::CodexAdapter::fingerprint(&target_auth),
                                ..Profile::default()
                            },
                        ),
                    ]);
                    Ok(())
                })
                .unwrap();

            let outcome = service.switch("codex", "target").unwrap();

            assert_eq!(outcome.status, "switched", "case: {name}");
            assert_eq!(
                crate::CodexAdapter::parse_auth(&live.join("auth.json")).unwrap(),
                target_auth,
                "credential mismatch in case: {name}",
            );
            assert_eq!(
                crate::CodexAdapter::provider(&service.store.load().unwrap().apps["codex"])
                    .unwrap()
                    .id,
                target_provider,
                "provider mismatch in case: {name}",
            );
        }
    }

    #[test]
    fn switch_restores_every_live_layer_after_partial_projection_failure() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        let profiles = root.path().join("profiles");
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&profiles).unwrap();
        let current_config =
            "cli_auth_credentials_store = \"file\"\nmodel_provider = \"current\"\n[model_providers.current]\nbase_url = \"https://current.example\"\n";
        let target_config =
            "cli_auth_credentials_store = \"file\"\nmodel_provider = \"target\"\n[model_providers.target]\nbase_url = \"https://target.example\"\n";
        fs::write(live.join("config.toml"), current_config).unwrap();
        let current_auth =
            json!({"tokens":{"account_id":"current-account","access_token":"current"}});
        let target_auth = json!({"tokens":{"account_id":"target-account","access_token":"target"}});
        fs::write(
            live.join("auth.json"),
            serde_json::to_vec(&current_auth).unwrap(),
        )
        .unwrap();
        let current_profile_config = profiles.join("current.toml");
        let target_profile_config = profiles.join("target.toml");
        fs::write(&current_profile_config, current_config).unwrap();
        fs::write(&target_profile_config, target_config).unwrap();
        let current_reference = SecretRef {
            service: "com.mix.test".into(),
            account: "current".into(),
        };
        let target_reference = SecretRef {
            service: "com.mix.test".into(),
            account: "target".into(),
        };
        let vault = Arc::new(MemoryVault::default());
        vault
            .set(
                &current_reference,
                &serde_json::to_vec(&current_auth).unwrap(),
            )
            .unwrap();
        vault
            .set(
                &target_reference,
                &serde_json::to_vec(&target_auth).unwrap(),
            )
            .unwrap();
        let service =
            MixService::with_vault(root.path().join("mix/config.json"), vault.clone()).unwrap();
        service.initialize().unwrap();
        service
            .register_client("codex", AdapterKind::Codex, live.clone())
            .unwrap();
        service
            .store
            .mutate(|config| {
                let client = config.apps.get_mut("codex").unwrap();
                client.active_profile = Some("current".into());
                client.profiles = BTreeMap::from([
                    (
                        "current".into(),
                        Profile {
                            label: "Current".into(),
                            files: BTreeMap::from([("config.toml".into(), current_profile_config)]),
                            secret_files: BTreeMap::from([("auth.json".into(), current_reference)]),
                            account_fingerprint: crate::CodexAdapter::fingerprint(&current_auth),
                            ..Profile::default()
                        },
                    ),
                    (
                        "target".into(),
                        Profile {
                            label: "Target".into(),
                            files: BTreeMap::from([("config.toml".into(), target_profile_config)]),
                            secret_files: BTreeMap::from([("auth.json".into(), target_reference)]),
                            account_fingerprint: crate::CodexAdapter::fingerprint(&target_auth),
                            ..Profile::default()
                        },
                    ),
                ]);
                Ok(())
            })
            .unwrap();

        // Target preflight and current-account synchronization succeed. The
        // third vault read is the target credential projection, after the
        // ordinary configuration file has already changed.
        vault.set_read_failure_on(3);
        let error = service.switch("codex", "target").unwrap_err();

        assert_eq!(error.code, ErrorCode::MixSwitchRolledBack);
        assert_eq!(
            error.details["cause_code"],
            ErrorCode::MixCredentialUnavailable.as_str()
        );
        assert_eq!(error.details["cleanup_pending"], false);
        assert!(error.details["cleanup_code"].is_null());
        assert_eq!(
            read_codex_config(&live.join("config.toml")).unwrap(),
            toml::from_str::<toml::Table>(current_config).unwrap()
        );
        assert_eq!(
            crate::CodexAdapter::parse_auth(&live.join("auth.json")).unwrap(),
            current_auth
        );
        assert_eq!(
            service.store.load().unwrap().apps["codex"]
                .active_profile
                .as_deref(),
            Some("current")
        );
        assert!(!SwitchJournal::path(service.store.root().unwrap()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_scanner_rejects_symlinked_app_storage() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("runtime")).unwrap();
        symlink(outside.path(), root.path().join("runtime/codex")).unwrap();
        assert!(runtime_directories(root.path(), "codex").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_revocation_never_follows_a_symlinked_profile_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let app_root = root.path().join("runtime/codex");
        let outside_runtime = outside.path().join("hash");
        fs::create_dir_all(&app_root).unwrap();
        fs::create_dir_all(&outside_runtime).unwrap();
        fs::write(outside_runtime.join("auth.json"), b"must-stay").unwrap();
        fs::write(
            outside_runtime.join(".mix-runtime.json"),
            serde_json::to_vec(&json!({
                "product":"mix",
                "app":"codex",
                "profile":"account-1",
                "runtime":app_root.join("account-1/hash"),
            }))
            .unwrap(),
        )
        .unwrap();
        symlink(outside.path(), app_root.join("account-1")).unwrap();
        revoke_runtime_profile_files(&app_root, "account-1", None, &["auth.json".into()]).unwrap();
        assert_eq!(
            fs::read(outside_runtime.join("auth.json")).unwrap(),
            b"must-stay"
        );
    }

    #[test]
    fn removing_a_profile_revokes_managed_runtime_files_but_keeps_sessions() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime/codex/account-1/hash");
        fs::create_dir_all(runtime.join("sessions")).unwrap();
        fs::write(runtime.join("config.toml"), b"configuration").unwrap();
        fs::write(runtime.join("auth.json"), b"secret").unwrap();
        fs::write(runtime.join("sessions/session.jsonl"), b"history").unwrap();
        fs::write(
            runtime.join(".mix-runtime.json"),
            serde_json::to_vec(&json!({
                "product": "mix",
                "app": "codex",
                "profile": "account-1",
                "runtime": runtime,
            }))
            .unwrap(),
        )
        .unwrap();
        revoke_runtime_profile_files(
            &root.path().join("runtime/codex"),
            "account-1",
            None,
            &["auth.json".into(), "config.toml".into()],
        )
        .unwrap();
        assert!(!runtime.join("auth.json").exists());
        assert!(!runtime.join("config.toml").exists());
        assert!(runtime.join("sessions/session.jsonl").exists());
        assert!(runtime.join(RUNTIME_REVOKE_MARKER).is_file());

        // A still-running client may rewrite a managed file while exiting.
        // The durable revocation marker makes the next Mix startup scrub all
        // projected files again without deleting native sessions.
        fs::write(runtime.join("auth.json"), b"rewritten-secret").unwrap();
        let mut config = crate::Config::empty(root.path().to_path_buf());
        config.apps.insert(
            "codex".into(),
            ClientConfig {
                adapter: AdapterKind::Codex,
                live_dir: root.path().join("live"),
                active_profile: None,
                profiles: BTreeMap::new(),
                run: RunSpec::default(),
            },
        );
        prune_revoked_runtime_files(&config).unwrap();
        assert!(!runtime.join("auth.json").exists());
        assert!(runtime.join("sessions/session.jsonl").exists());
    }
}
