use crate::fs::{atomic_write, read_bounded, safe_child, shell_quote};
use crate::model::is_environment_variable_name;
#[cfg(target_os = "macos")]
use crate::process::running_executable;
use crate::process::ProcessSpec;
use crate::transaction::FileTokenRewrite;
use crate::{
    AccountIdentity, AccountSwitchCapability, AccountSwitchStatus, AdapterKind, Capability,
    CapabilityAuthority, CapabilitySupport, ClientConfig, Completeness, CredentialVault, Error,
    ErrorCode, Profile, ProfileCategory, ProfileLabelOrigin, ProviderIdentity, Recoverability,
    Result, SecretRef, Session, SessionCatalogSource, SessionProjectSource, SessionRecoveryReason,
};
use base64::Engine;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const SESSION_SCAN_BYTES: u64 = 512 * 1024;
const SESSION_LINE_BYTES: usize = 64 * 1024;
const SESSION_META_BYTES: u64 = 256 * 1024;
const SESSION_TITLE_CHARS: usize = 240;
const SESSION_MAX_FILES: usize = 5_000;
const SESSION_MAX_CANDIDATES: usize = 20_000;
const SESSION_MAX_WALK_ENTRIES: usize = 100_000;
const SESSION_MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const SESSION_MAX_DEPTH: usize = 16;

#[derive(Clone, Copy, Debug)]
pub struct AdapterDescriptor {
    pub kind: AdapterKind,
    pub name: &'static str,
    pub label: &'static str,
    pub default_home: &'static str,
    pub default_command: &'static [&'static str],
    pub session_roots: &'static [&'static str],
    pub import_files: &'static [&'static str],
    pub profile_category: ProfileCategory,
    pub profile_prefix: &'static str,
}

pub trait Adapter: Send + Sync {
    fn descriptor(&self) -> AdapterDescriptor;
    fn capabilities(
        &self,
        client: &ClientConfig,
        command_found: bool,
    ) -> BTreeMap<String, Capability>;
    fn account_capability(&self, client: &ClientConfig) -> AccountSwitchCapability;
    fn capture_payload(&self, client: &ClientConfig, relative: &str) -> Result<Vec<u8>>;
    fn validate_profile(&self, client: &ClientConfig, profile: &Profile) -> Result<()>;
    fn sessions(
        &self,
        app: &str,
        client: &ClientConfig,
        query: Option<&str>,
    ) -> Result<Vec<Session>>;
    fn resume_argv(&self, client: &ClientConfig, native_id: &str) -> Result<Vec<String>>;
    fn open_project_argv(&self, _: &ClientConfig, _: &Path) -> Result<Vec<String>> {
        Err(Error::new(
            ErrorCode::MixUnsupported,
            "native project open is unsupported for this client",
        ))
    }
    fn runtime_variable(&self) -> Option<&'static str>;
    fn desktop_process(&self, _: &ClientConfig) -> Option<ProcessSpec> {
        None
    }
    fn global_account_projection(&self) -> Option<&dyn GlobalAccountProjection> {
        None
    }
    fn account_capture(&self) -> Option<&dyn AccountCapture> {
        None
    }
    fn account_enrollment(&self) -> Option<&dyn AccountEnrollment> {
        None
    }
    fn runtime_identity_matches(
        &self,
        profile: &Profile,
        _: &Path,
        marker_fingerprint: Option<&str>,
    ) -> bool {
        profile
            .account_fingerprint
            .as_deref()
            .is_none_or(|expected| marker_fingerprint == Some(expected))
    }
    fn native_session_environment_profile(
        &self,
        _: &ClientConfig,
        _: Option<&str>,
    ) -> Option<String> {
        None
    }

    fn profile_provider(&self, _: &Profile) -> Result<Option<ProviderIdentity>> {
        Ok(None)
    }

    fn profile_display_label(
        &self,
        profile: &Profile,
        identity: Option<&AccountIdentity>,
        _: Option<&ProviderIdentity>,
    ) -> String {
        if profile.label_origin == ProfileLabelOrigin::User {
            return profile.label.clone();
        }
        identity
            .map(|identity| identity.display_label(&profile.label))
            .unwrap_or_else(|| profile.label.clone())
    }

    fn observed_profile(&self, client: &ClientConfig) -> Option<String> {
        match self.global_account_projection() {
            Some(_) => self.account_capability(client).current_profile,
            None => client.active_profile.clone(),
        }
    }
}

#[derive(Clone)]
pub struct CapturedAccount {
    pub fingerprint: String,
    pub identity: AccountIdentity,
    pub suggested_label: String,
    pub files: BTreeMap<String, Vec<u8>>,
    pub secret_files: BTreeMap<String, Vec<u8>>,
}

/// Adapter-owned account discovery and refresh semantics. Core owns profile
/// naming, private storage, credential references and failure cleanup.
pub trait AccountCapture: Send + Sync {
    fn capture_current(&self, client: &ClientConfig) -> Result<CapturedAccount>;
    fn matching_profile(
        &self,
        client: &ClientConfig,
        captured: &CapturedAccount,
    ) -> Result<Option<String>>;
    fn refresh_profile(
        &self,
        client: &mut ClientConfig,
        profile: &str,
        captured: &CapturedAccount,
        vault: &dyn CredentialVault,
    ) -> Result<()>;
}

pub struct AccountEnrollmentPlan {
    pub files: BTreeMap<String, Vec<u8>>,
    pub argv: Vec<String>,
    pub runtime_variable: Option<&'static str>,
}

#[derive(Default)]
pub struct SwitchProjection {
    pub files: BTreeMap<String, Vec<u8>>,
    pub removals: BTreeSet<String>,
    pub rewrites: Vec<FileTokenRewrite>,
}

/// Adapter-owned isolated native sign-in. Core owns the enrollment journal,
/// timeout, process handoff and cleanup.
pub trait AccountEnrollment: Send + Sync {
    fn prepare(&self, client: &ClientConfig, executable: &Path) -> Result<AccountEnrollmentPlan>;
    fn capture_completed(
        &self,
        client: &ClientConfig,
        state_dir: &Path,
    ) -> Result<Option<CapturedAccount>>;
}

/// Adapter-owned semantics for a client whose account and provider state is
/// global to a running desktop process. Core owns the transaction and process
/// lifecycle; the adapter owns how native files form one coherent projection.
pub trait GlobalAccountProjection: Send + Sync {
    fn switch_source(&self, client: &ClientConfig) -> Result<Option<String>>;
    fn validate_target_credential(
        &self,
        client: &ClientConfig,
        target: &Profile,
        vault: &dyn CredentialVault,
    ) -> Result<()>;
    /// Make a saved credential ready for projection while the current client
    /// is still untouched. Implementations must either complete durably or
    /// return without changing the live client state.
    fn prepare_target_credential(
        &self,
        client: &ClientConfig,
        target: &Profile,
        vault: &dyn CredentialVault,
    ) -> Result<()> {
        self.validate_target_credential(client, target, vault)
    }
    fn synchronize_source(
        &self,
        client: &mut ClientConfig,
        vault: &dyn CredentialVault,
    ) -> Result<Option<String>>;
    fn materialized_files(
        &self,
        client: &ClientConfig,
        target: &Profile,
    ) -> Result<BTreeMap<String, Vec<u8>>>;
    fn switch_projection(
        &self,
        client: &ClientConfig,
        target: &Profile,
    ) -> Result<SwitchProjection> {
        Ok(SwitchProjection {
            files: self.materialized_files(client, target)?,
            ..Default::default()
        })
    }
    fn verify_projection(&self, client: &ClientConfig, target: &Profile) -> Result<()>;
    fn verify_stable_projection(&self, client: &ClientConfig, target: &Profile) -> Result<()>;
    fn synchronize_target(
        &self,
        client: &ClientConfig,
        target: &Profile,
        vault: &dyn CredentialVault,
    ) -> Result<()>;
}

pub struct AdapterRegistry {
    adapters: BTreeMap<AdapterKind, Box<dyn Adapter>>,
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self {
            adapters: BTreeMap::from([
                (
                    AdapterKind::Codex,
                    Box::new(CodexAdapter) as Box<dyn Adapter>,
                ),
                (
                    AdapterKind::Claude,
                    Box::new(ClaudeAdapter) as Box<dyn Adapter>,
                ),
            ]),
        }
    }
}

impl AdapterRegistry {
    pub fn get(&self, kind: AdapterKind) -> &dyn Adapter {
        self.adapters
            .get(&kind)
            .expect("every AdapterKind must have a registered adapter")
            .as_ref()
    }

    pub fn all(&self) -> impl Iterator<Item = &dyn Adapter> {
        self.adapters.values().map(Box::as_ref)
    }

    #[cfg(test)]
    pub(crate) fn replace(&mut self, kind: AdapterKind, adapter: Box<dyn Adapter>) {
        self.adapters.insert(kind, adapter);
    }
}

#[derive(Default)]
pub struct CodexAdapter;

impl CodexAdapter {
    pub fn parse_auth(path: &Path) -> Result<Value> {
        let payload = read_bounded(path, 2 * 1024 * 1024, "cannot read Codex auth")?;
        let value: Value = serde_json::from_slice(&payload).map_err(|_| {
            Error::new(
                ErrorCode::MixValidationError,
                "Codex auth.json is not valid JSON",
            )
        })?;
        if !value.is_object() {
            return Err(Error::invalid("Codex auth.json must contain an object"));
        }
        Ok(value)
    }

    pub fn auth_document(client: &ClientConfig) -> Result<Value> {
        let path = safe_child(&client.live_dir, "auth.json")?;
        Self::parse_auth(&path)
    }

    pub fn fingerprint(document: &Value) -> Option<String> {
        let account = document
            .pointer("/tokens/account_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let api_key = document
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let source = account
            .map(|value| format!("chatgpt:{value}"))
            .or_else(|| api_key.map(|value| format!("api-key:{value}")))?;
        Some(format!("{:x}", Sha256::digest(source.as_bytes())))
    }

    pub fn identity(document: &Value) -> AccountIdentity {
        let mut documents = vec![document.clone()];
        if let Some(tokens) = document.get("tokens") {
            documents.push(tokens.clone());
            for key in ["id_token", "access_token"] {
                if let Some(jwt) = tokens.get(key).and_then(Value::as_str) {
                    if let Some(claims) = decode_jwt_claims(jwt) {
                        documents.push(claims);
                    }
                }
            }
        }
        let mut expanded = Vec::new();
        for value in documents {
            expanded.push(value.clone());
            for key in [
                "https://api.openai.com/profile",
                "https://api.openai.com/auth",
                "profile",
                "auth",
            ] {
                if let Some(value) = value.get(key).filter(|value| value.is_object()) {
                    expanded.push(value.clone());
                }
            }
        }
        let email = first_text(&expanded, &["email", "user_email"], 254)
            .filter(|value| value.contains('@') && !value.chars().any(char::is_whitespace));
        let name = first_text(&expanded, &["name", "display_name", "user_name"], 120);
        let plan = first_text(&expanded, &["chatgpt_plan_type", "plan_type", "plan"], 64);
        let suffix = Self::fingerprint(document).map(|value| value[..6].to_ascii_uppercase());
        let suggested_label = email
            .clone()
            .or_else(|| name.clone())
            .or_else(|| suffix.as_ref().map(|value| format!("Codex · {value}")));
        AccountIdentity {
            email,
            name,
            plan,
            suffix,
            suggested_label,
        }
    }

    pub fn provider(client: &ClientConfig) -> Result<ProviderIdentity> {
        Self::config(client).map(|document| provider_from_toml(&document))
    }

    pub(crate) fn provider_endpoint_host(client: &ClientConfig) -> Result<Option<String>> {
        let document = Self::config(client)?;
        Ok(codex_endpoint_host(&document))
    }

    fn config(client: &ClientConfig) -> Result<toml::Table> {
        let path = client.live_dir.join("config.toml");
        if !path.exists() {
            return Ok(toml::Table::new());
        }
        read_toml(&path)
    }

    fn storage(document: &toml::Table) -> String {
        document
            .get("cli_auth_credentials_store")
            .and_then(toml::Value::as_str)
            .unwrap_or("default")
            .to_owned()
    }
}

impl Adapter for CodexAdapter {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            kind: AdapterKind::Codex,
            name: "codex",
            label: "Codex",
            default_home: ".codex",
            default_command: &["codex"],
            session_roots: &["sessions", "archived_sessions"],
            import_files: &[],
            profile_category: ProfileCategory::Account,
            profile_prefix: "account",
        }
    }

    fn capabilities(
        &self,
        client: &ClientConfig,
        command_found: bool,
    ) -> BTreeMap<String, Capability> {
        let account = self.account_capability(client);
        base_capabilities(command_found, self.desktop_process(client).is_some())
            .into_iter()
            .chain([
                (
                    "identity.detect".into(),
                    Capability {
                        available: account.available,
                        support: CapabilitySupport::NativeFilesReadonly,
                        authority: CapabilityAuthority::ReadOnly,
                        fallback: None,
                        reason: (!account.available)
                            .then(|| format!("{:?}", account.status).to_lowercase()),
                        completeness: Some(Completeness::Complete),
                    },
                ),
                (
                    "identity.capture".into(),
                    Capability {
                        available: account.available,
                        support: CapabilitySupport::NativeFilesReadonly,
                        authority: CapabilityAuthority::CredentialProjection,
                        fallback: None,
                        reason: None,
                        completeness: Some(Completeness::Complete),
                    },
                ),
                (
                    "identity.enroll".into(),
                    Capability {
                        available: command_found,
                        support: CapabilitySupport::NativeCli,
                        authority: CapabilityAuthority::CredentialProjection,
                        fallback: None,
                        reason: None,
                        completeness: Some(Completeness::Complete),
                    },
                ),
                (
                    "identity.switch_global".into(),
                    Capability {
                        available: client
                            .profiles
                            .values()
                            .any(|profile| profile.account_fingerprint.is_some()),
                        support: CapabilitySupport::Configured,
                        authority: CapabilityAuthority::CredentialProjection,
                        fallback: None,
                        reason: None,
                        completeness: Some(Completeness::Complete),
                    },
                ),
                (
                    "session.list".into(),
                    Capability {
                        available: client.live_dir.is_dir(),
                        support: CapabilitySupport::NativeFilesReadonly,
                        authority: CapabilityAuthority::ReadOnly,
                        fallback: None,
                        reason: None,
                        completeness: Some(Completeness::BestEffort),
                    },
                ),
                (
                    "session.resume".into(),
                    Capability {
                        available: command_found,
                        support: CapabilitySupport::NativeCli,
                        authority: CapabilityAuthority::Launch,
                        fallback: None,
                        reason: None,
                        completeness: Some(Completeness::Complete),
                    },
                ),
                (
                    "session.open_native".into(),
                    Capability {
                        available: false,
                        support: CapabilitySupport::Unsupported,
                        authority: CapabilityAuthority::Launch,
                        fallback: None,
                        reason: None,
                        completeness: Some(Completeness::BestEffort),
                    },
                ),
                (
                    "project.open_native".into(),
                    Capability {
                        available: command_found,
                        support: CapabilitySupport::NativeCli,
                        authority: CapabilityAuthority::Launch,
                        fallback: None,
                        reason: None,
                        completeness: Some(Completeness::Complete),
                    },
                ),
            ])
            .collect()
    }

    fn account_capability(&self, client: &ClientConfig) -> AccountSwitchCapability {
        let config = match Self::config(client) {
            Ok(value) => value,
            Err(_) => {
                return AccountSwitchCapability {
                    status: AccountSwitchStatus::InvalidConfig,
                    storage: Some("invalid".into()),
                    ..Default::default()
                }
            }
        };
        let storage = Self::storage(&config);
        if matches!(storage.as_str(), "keyring" | "auto" | "ephemeral") {
            return AccountSwitchCapability {
                status: AccountSwitchStatus::FileAuthRequired,
                storage: Some(storage),
                ..Default::default()
            };
        }
        let document = match Self::auth_document(client) {
            Ok(value) => value,
            Err(error) => {
                let status = if client.live_dir.join("auth.json").exists() {
                    AccountSwitchStatus::InvalidAuth
                } else {
                    AccountSwitchStatus::SignInRequired
                };
                let _ = error;
                return AccountSwitchCapability {
                    status,
                    storage: Some(storage),
                    ..Default::default()
                };
            }
        };
        let Some(fingerprint) = Self::fingerprint(&document) else {
            return AccountSwitchCapability {
                status: AccountSwitchStatus::IdentityUnavailable,
                storage: Some(storage),
                ..Default::default()
            };
        };
        AccountSwitchCapability {
            available: true,
            status: AccountSwitchStatus::Ready,
            storage: Some(storage),
            identity: Some(Self::identity(&document)),
            current_profile: matching_codex_profile(client, &fingerprint, &config),
            provider: Some(provider_from_toml(&config)),
        }
    }

    fn capture_payload(&self, client: &ClientConfig, relative: &str) -> Result<Vec<u8>> {
        let source = safe_child(&client.live_dir, relative)?;
        read_bounded(&source, 16 * 1024 * 1024, "cannot capture client file")
    }

    fn validate_profile(&self, _client: &ClientConfig, profile: &Profile) -> Result<()> {
        validate_managed_paths(profile, &["config.toml"], &["auth.json"])?;
        if profile.account_fingerprint.is_none() {
            return Err(Error::new(
                ErrorCode::MixCodexFileAuthRequired,
                "Codex accounts must be added from a verified native login",
            ));
        }
        let config = profile.files.get("config.toml").ok_or_else(|| {
            Error::new(
                ErrorCode::MixCodexFileAuthRequired,
                "a managed Codex account requires a private config.toml",
            )
        })?;
        let document = read_toml(config)?;
        validate_codex_config(&document)?;
        if document
            .get("cli_auth_credentials_store")
            .and_then(toml::Value::as_str)
            .is_some_and(|value| value != "file")
        {
            return Err(Error::new(
                ErrorCode::MixCodexFileAuthRequired,
                "a Codex account overlay cannot select a non-file credential store",
            ));
        }
        if !profile.secret_files.contains_key("auth.json") {
            return Err(Error::new(
                ErrorCode::MixCredentialNotFound,
                "managed Codex account has no auth.json vault reference",
            ));
        }
        Ok(())
    }

    fn sessions(
        &self,
        app: &str,
        client: &ClientConfig,
        query: Option<&str>,
    ) -> Result<Vec<Session>> {
        rollout_sessions(app, client, query)
    }

    fn resume_argv(&self, client: &ClientConfig, native_id: &str) -> Result<Vec<String>> {
        validate_native_id(native_id)?;
        let mut argv = configured_command(client)?;
        let provider = Self::provider(client)?.id;
        let provider_override = format!(
            "model_provider={}",
            serde_json::to_string(&provider).map_err(|error| {
                Error::new(
                    ErrorCode::MixInternalError,
                    format!("cannot encode Codex provider override: {error}"),
                )
            })?
        );
        argv.splice(
            1..1,
            [
                "-c".into(),
                provider_override,
                "resume".into(),
                native_id.into(),
            ],
        );
        Ok(argv)
    }

    fn open_project_argv(&self, client: &ClientConfig, cwd: &Path) -> Result<Vec<String>> {
        let executable = discover_executable(client)
            .ok_or_else(|| Error::new(ErrorCode::MixNotFound, "Codex executable is unavailable"))?;
        Ok(vec![
            executable.display().to_string(),
            "app".into(),
            cwd.display().to_string(),
        ])
    }

    fn runtime_variable(&self) -> Option<&'static str> {
        Some("CODEX_HOME")
    }

    fn profile_provider(&self, profile: &Profile) -> Result<Option<ProviderIdentity>> {
        codex_profile_provider(profile).map(Some)
    }

    fn profile_display_label(
        &self,
        profile: &Profile,
        identity: Option<&AccountIdentity>,
        provider: Option<&ProviderIdentity>,
    ) -> String {
        if profile.label_origin == ProfileLabelOrigin::User {
            return profile.label.clone();
        }
        identity
            .map(|identity| generated_codex_label(identity, provider, &profile.label))
            .unwrap_or_else(|| profile.label.clone())
    }

    fn desktop_process(&self, client: &ClientConfig) -> Option<ProcessSpec> {
        codex_desktop_process(client)
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

    fn runtime_identity_matches(
        &self,
        profile: &Profile,
        runtime: &Path,
        marker_fingerprint: Option<&str>,
    ) -> bool {
        let Some(expected) = profile.account_fingerprint.as_deref() else {
            return true;
        };
        if marker_fingerprint != Some(expected) {
            return false;
        }
        let auth = runtime.join("auth.json");
        match fs::symlink_metadata(&auth) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Ok(_) => {
                Self::parse_auth(&auth)
                    .ok()
                    .and_then(|document| Self::fingerprint(&document))
                    .as_deref()
                    == Some(expected)
            }
            Err(_) => false,
        }
    }
}

impl AccountCapture for CodexAdapter {
    fn capture_current(&self, client: &ClientConfig) -> Result<CapturedAccount> {
        let document = Self::auth_document(client)?;
        let fingerprint = Self::fingerprint(&document).ok_or_else(|| {
            Error::new(
                ErrorCode::MixValidationError,
                "the current Codex login has no stable account identity",
            )
        })?;
        let identity = Self::identity(&document);
        let provider = Self::provider(client)?;
        let suggested_label = generated_codex_label(
            &identity,
            Some(&provider),
            &format!("Codex · {}", fingerprint[..6].to_ascii_uppercase()),
        );
        Ok(CapturedAccount {
            fingerprint,
            identity,
            suggested_label,
            files: BTreeMap::from([(
                "config.toml".into(),
                codex_account_overlay_text(&client.live_dir.join("config.toml"))?.into_bytes(),
            )]),
            secret_files: BTreeMap::from([("auth.json".into(), serde_json::to_vec(&document)?)]),
        })
    }

    fn matching_profile(
        &self,
        client: &ClientConfig,
        captured: &CapturedAccount,
    ) -> Result<Option<String>> {
        let payload = captured.files.get("config.toml").ok_or_else(|| {
            Error::new(
                ErrorCode::MixInternalError,
                "captured Codex account has no configuration overlay",
            )
        })?;
        let text = std::str::from_utf8(payload)
            .map_err(|_| Error::invalid("captured Codex configuration is not UTF-8"))?;
        let document = toml::from_str::<toml::Table>(text).map_err(|error| {
            Error::invalid(format!("captured Codex configuration is invalid: {error}"))
        })?;
        Ok(matching_codex_profile(
            client,
            &captured.fingerprint,
            &document,
        ))
    }

    fn refresh_profile(
        &self,
        client: &mut ClientConfig,
        profile: &str,
        captured: &CapturedAccount,
        vault: &dyn CredentialVault,
    ) -> Result<()> {
        let document = captured
            .secret_files
            .get("auth.json")
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::MixInternalError,
                    "captured Codex account has no credential",
                )
            })
            .and_then(|payload| {
                serde_json::from_slice::<Value>(payload).map_err(|error| {
                    Error::new(
                        ErrorCode::MixInternalError,
                        format!("captured Codex credential is invalid: {error}"),
                    )
                })
            })?;
        let reference = client
            .profiles
            .get(profile)
            .and_then(|profile| profile.secret_files.get("auth.json"))
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::MixCredentialNotFound,
                    "the existing account has no credential reference",
                )
            })?
            .clone();
        merge_codex_credential(vault, &reference, &captured.fingerprint, &document)?;
        sync_live_codex_overlay(client, profile)?;
        client
            .profiles
            .get_mut(profile)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::MixInternalError,
                    "the matching account disappeared during capture",
                )
            })?
            .account_identity = Some(captured.identity.clone());
        Ok(())
    }
}

impl AccountEnrollment for CodexAdapter {
    fn prepare(&self, client: &ClientConfig, executable: &Path) -> Result<AccountEnrollmentPlan> {
        Ok(AccountEnrollmentPlan {
            files: BTreeMap::from([(
                "config.toml".into(),
                codex_login_config(&client.live_dir.join("config.toml"))?.into_bytes(),
            )]),
            argv: vec![executable.display().to_string(), "login".into()],
            runtime_variable: Some("CODEX_HOME"),
        })
    }

    fn capture_completed(
        &self,
        client: &ClientConfig,
        state_dir: &Path,
    ) -> Result<Option<CapturedAccount>> {
        let auth = safe_child(state_dir, "auth.json")?;
        match fs::symlink_metadata(&auth) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(Error::new(
                    ErrorCode::MixLocalFailure,
                    "the isolated login credential is not a regular file",
                ));
            }
            Ok(_) => {}
            Err(error) => {
                return Err(Error::io("cannot inspect isolated login credential", error));
            }
        }
        if Self::parse_auth(&auth).is_err() {
            return Ok(None);
        }
        let mut isolated = client.clone();
        isolated.live_dir = state_dir.to_path_buf();
        self.capture_current(&isolated).map(Some)
    }
}

impl GlobalAccountProjection for CodexAdapter {
    fn switch_source(&self, client: &ClientConfig) -> Result<Option<String>> {
        codex_switch_source(self.account_capability(client))
    }

    fn validate_target_credential(
        &self,
        _client: &ClientConfig,
        target: &Profile,
        vault: &dyn CredentialVault,
    ) -> Result<()> {
        verify_managed_codex_credential(target, vault)
    }

    fn prepare_target_credential(
        &self,
        _client: &ClientConfig,
        target: &Profile,
        vault: &dyn CredentialVault,
    ) -> Result<()> {
        refresh_managed_codex_credential(target, vault)
    }

    fn synchronize_source(
        &self,
        client: &mut ClientConfig,
        vault: &dyn CredentialVault,
    ) -> Result<Option<String>> {
        let Some(_) = self.switch_source(client)? else {
            return Ok(None);
        };
        let (profile, identity) = sync_live_credential(client, vault)?;
        sync_live_codex_overlay(client, &profile)?;
        client
            .profiles
            .get_mut(&profile)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::MixInternalError,
                    "the synchronized account disappeared during switching",
                )
            })?
            .account_identity = Some(identity);
        Ok(Some(profile))
    }

    fn materialized_files(
        &self,
        client: &ClientConfig,
        target: &Profile,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        Ok(BTreeMap::from([(
            "config.toml".into(),
            codex_materialized_config(client, target)?,
        )]))
    }

    fn switch_projection(
        &self,
        client: &ClientConfig,
        target: &Profile,
    ) -> Result<SwitchProjection> {
        let files = self.materialized_files(client, target)?;
        let provider = codex_profile_provider(target)?.id;
        let mut files = files;
        let history_overrides = codex_history_database_overrides(client, &provider)?;
        let history_replacements = history_overrides.keys().cloned().collect::<BTreeSet<_>>();
        files.extend(history_overrides);
        Ok(SwitchProjection {
            files,
            removals: codex_history_sidecar_removals(client, &history_replacements),
            rewrites: codex_history_rollout_rewrites(client, &provider)?,
        })
    }

    fn verify_projection(&self, client: &ClientConfig, target: &Profile) -> Result<()> {
        verify_live_projection(client, target)
    }

    fn verify_stable_projection(&self, client: &ClientConfig, target: &Profile) -> Result<()> {
        verify_stable_codex_projection(client, target)
    }

    fn synchronize_target(
        &self,
        client: &ClientConfig,
        target: &Profile,
        vault: &dyn CredentialVault,
    ) -> Result<()> {
        sync_target_credential(client, target, vault)
    }
}

const CODEX_ACCOUNT_OVERLAY_KEYS: &[&str] = &[
    "apps_mcp_product_sku",
    "chatgpt_base_url",
    "disable_response_storage",
    "experimental_realtime_ws_base_url",
    "model",
    "model_auto_compact_token_limit",
    "model_auto_compact_token_limit_scope",
    "model_catalog_json",
    "model_context_window",
    "model_provider",
    "model_reasoning_effort",
    "model_reasoning_summary",
    "model_supports_reasoning_summaries",
    "model_verbosity",
    "openai_base_url",
    "service_tier",
    "wire_api",
];

pub(crate) fn read_codex_config(path: &Path) -> Result<toml::Table> {
    let document = if path.exists() {
        let payload = read_bounded(path, 2 * 1024 * 1024, "cannot read Codex config")?;
        let text = std::str::from_utf8(&payload)
            .map_err(|_| Error::invalid("Codex config.toml is not valid UTF-8"))?;
        toml::from_str::<toml::Table>(text)
            .map_err(|error| Error::invalid(format!("invalid Codex config.toml: {error}")))?
    } else {
        toml::Table::new()
    };
    validate_codex_config(&document)?;
    Ok(document)
}

fn codex_account_overlay(document: &toml::Table) -> toml::Table {
    let mut profile = CODEX_ACCOUNT_OVERLAY_KEYS
        .iter()
        .filter_map(|key| {
            document
                .get(*key)
                .cloned()
                .map(|value| ((*key).to_owned(), value))
        })
        .collect::<toml::Table>();
    if let Some(provider) = document
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .and_then(|provider| {
            document
                .get("model_providers")
                .and_then(toml::Value::as_table)
                .and_then(|providers| providers.get(provider))
                .cloned()
                .map(|definition| (provider.to_owned(), definition))
        })
    {
        profile.insert(
            "model_providers".into(),
            toml::Value::Table(toml::Table::from_iter([provider])),
        );
    }
    profile
}

fn encode_codex_config(document: &toml::Table) -> Result<String> {
    toml::to_string_pretty(document).map_err(|error| {
        Error::new(
            ErrorCode::MixLocalFailure,
            format!("cannot encode Codex config: {error}"),
        )
    })
}

fn codex_account_overlay_text(path: &Path) -> Result<String> {
    encode_codex_config(&codex_account_overlay(&read_codex_config(path)?))
}

pub(crate) fn codex_login_config(path: &Path) -> Result<String> {
    let mut document = read_codex_config(path)?;
    for key in CODEX_ACCOUNT_OVERLAY_KEYS {
        document.remove(*key);
    }
    document.insert(
        "cli_auth_credentials_store".into(),
        toml::Value::String("file".into()),
    );
    encode_codex_config(&document)
}

pub(crate) fn codex_materialized_config(
    client: &ClientConfig,
    profile: &Profile,
) -> Result<Vec<u8>> {
    let mut document = read_codex_config(&client.live_dir.join("config.toml"))?;
    for key in CODEX_ACCOUNT_OVERLAY_KEYS {
        document.remove(*key);
    }
    document.remove("cli_auth_credentials_store");

    let source = profile
        .files
        .get("config.toml")
        .ok_or_else(|| Error::invalid("Codex account has no configuration overlay"))?;
    let overlay = codex_account_overlay(&read_codex_config(source)?);
    let target_provider = overlay
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai")
        .to_owned();
    let target_defines_provider = overlay
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .is_some_and(|providers| providers.contains_key(&target_provider));
    if !target_defines_provider {
        let providers_empty = document
            .get_mut("model_providers")
            .and_then(toml::Value::as_table_mut)
            .is_some_and(|providers| {
                providers.remove(&target_provider);
                providers.is_empty()
            });
        if providers_empty {
            document.remove("model_providers");
        }
    }
    for (key, value) in overlay {
        if key == "model_providers" {
            let providers = document
                .entry(key)
                .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                .as_table_mut()
                .ok_or_else(|| Error::invalid("Codex model_providers must be a table"))?;
            for (provider, definition) in value
                .as_table()
                .ok_or_else(|| Error::invalid("Codex account provider overlay is invalid"))?
            {
                providers.insert(provider.clone(), definition.clone());
            }
        } else {
            document.insert(key, value);
        }
    }
    document.insert(
        "cli_auth_credentials_store".into(),
        toml::Value::String("file".into()),
    );
    validate_codex_config(&document)?;
    Ok(encode_codex_config(&document)?.into_bytes())
}

const CODEX_HISTORY_DATABASES: &[&str] = &["state_5.sqlite", "sqlite/state_5.sqlite"];
const CODEX_HISTORY_SIDECARS: &[&str] = &["-wal", "-shm", "-journal"];
const CODEX_HISTORY_PREFIX_BYTES: usize = 64 * 1024;
const CODEX_HISTORY_DATABASE_BYTES: u64 = 128 * 1024 * 1024;

fn codex_history_database_paths(client: &ClientConfig) -> Vec<(&'static str, PathBuf)> {
    CODEX_HISTORY_DATABASES
        .iter()
        .copied()
        .map(|relative| (relative, client.live_dir.join(relative)))
        .collect()
}

fn sqlite_error(context: &str, error: rusqlite::Error) -> Error {
    Error::new(ErrorCode::MixLocalFailure, format!("{context}: {error}"))
}

fn is_unusable_history_database(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(sqlite_error, _)
            if matches!(
                sqlite_error.code,
                rusqlite::ffi::ErrorCode::DatabaseCorrupt
                    | rusqlite::ffi::ErrorCode::NotADatabase
                    | rusqlite::ffi::ErrorCode::SchemaChanged
            )
    )
}

fn open_codex_history_database(path: &Path, context: &str) -> Result<Option<Connection>> {
    match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(connection) => Ok(Some(connection)),
        Err(error) if is_unusable_history_database(&error) => Ok(None),
        Err(error) => Err(sqlite_error(context, error)),
    }
}

fn codex_history_columns(connection: &Connection) -> Result<Option<BTreeSet<String>>> {
    let has_threads: bool = match connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'threads')",
        [],
        |row| row.get(0),
    ) {
        Ok(value) => value,
        Err(error) if is_unusable_history_database(&error) => return Ok(None),
        Err(error) => {
            return Err(sqlite_error(
                "cannot inspect Codex state database schema",
                error,
            ));
        }
    };
    if !has_threads {
        return Ok(None);
    }
    let columns =
        match connection
            .prepare("PRAGMA table_info(threads)")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<rusqlite::Result<BTreeSet<_>>>()
            }) {
            Ok(columns) => columns,
            Err(error) if is_unusable_history_database(&error) => return Ok(None),
            Err(error) => return Err(sqlite_error("cannot inspect Codex thread schema", error)),
        };
    if !columns.contains("model_provider") {
        return Ok(None);
    }
    Ok(Some(columns))
}

fn codex_history_database_overrides(
    client: &ClientConfig,
    target_provider: &str,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut overrides = BTreeMap::new();
    for (relative, source) in codex_history_database_paths(client) {
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Error::io("cannot inspect Codex state database", error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(Error::invalid("Codex state database is not a regular file"));
        }

        let temporary = tempfile::tempdir()
            .map_err(|error| Error::io("cannot create Codex state database staging area", error))?;
        let staged = temporary.path().join("state_5.sqlite");
        let Some(source_connection) =
            open_codex_history_database(&source, "cannot open Codex state database")?
        else {
            continue;
        };
        source_connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|error| sqlite_error("cannot configure Codex state database", error))?;
        if let Err(error) = source_connection.backup(rusqlite::MAIN_DB, &staged, None) {
            if is_unusable_history_database(&error) {
                continue;
            }
            return Err(sqlite_error("cannot stage Codex state database", error));
        }
        drop(source_connection);

        let mut staged_connection = match Connection::open(&staged) {
            Ok(connection) => connection,
            Err(error) if is_unusable_history_database(&error) => continue,
            Err(error) => {
                return Err(sqlite_error(
                    "cannot open staged Codex state database",
                    error,
                ));
            }
        };
        let Some(_) = codex_history_columns(&staged_connection)? else {
            continue;
        };
        let needs_update: bool = match staged_connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM threads WHERE model_provider IS NULL OR model_provider != ?1)",
                [target_provider],
                |row| row.get(0),
            ) {
                Ok(value) => value,
                Err(error) if is_unusable_history_database(&error) => continue,
                Err(error) => {
                    return Err(sqlite_error("cannot inspect Codex thread providers", error));
                }
            };
        if !needs_update {
            continue;
        }
        staged_connection
            .execute_batch("PRAGMA journal_mode = DELETE;")
            .map_err(|error| sqlite_error("cannot prepare staged Codex state database", error))?;
        let transaction = staged_connection
            .transaction()
            .map_err(|error| sqlite_error("cannot begin Codex state database update", error))?;
        transaction
            .execute(
                "UPDATE threads SET model_provider = ?1 WHERE model_provider IS NULL OR model_provider != ?1",
                [target_provider],
            )
            .map_err(|error| sqlite_error("cannot update Codex thread providers", error))?;
        transaction
            .commit()
            .map_err(|error| sqlite_error("cannot commit Codex thread providers", error))?;
        drop(staged_connection);

        let payload = read_bounded(
            &staged,
            CODEX_HISTORY_DATABASE_BYTES,
            "staged Codex state database",
        )?;
        overrides.insert(relative.to_owned(), payload);
    }
    Ok(overrides)
}

fn codex_history_sidecar_removals(
    client: &ClientConfig,
    replaced_databases: &BTreeSet<String>,
) -> BTreeSet<String> {
    codex_history_database_paths(client)
        .into_iter()
        .filter(|(relative, _)| replaced_databases.contains(*relative))
        .flat_map(|(relative, path)| {
            CODEX_HISTORY_SIDECARS.iter().filter_map(move |suffix| {
                let sidecar = format!("{relative}{suffix}");
                path.with_file_name(format!("{}{}", path.file_name()?.to_string_lossy(), suffix))
                    .is_file()
                    .then_some(sidecar)
            })
        })
        .collect()
}

fn read_first_line(path: &Path) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| Error::io("cannot read Codex rollout", error))?;
    let reader = BufReader::new(file);
    let mut line = Vec::with_capacity(CODEX_HISTORY_PREFIX_BYTES);
    reader
        .take((CODEX_HISTORY_PREFIX_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|error| Error::io("cannot read Codex rollout metadata", error))?;
    if line.len() > CODEX_HISTORY_PREFIX_BYTES {
        return Err(Error::new(
            ErrorCode::MixValidationError,
            "Codex rollout metadata exceeds the scan limit",
        ));
    }
    Ok(line)
}

fn model_provider_token_offset(line: &[u8], expected: &str) -> Option<usize> {
    let record: Value = serde_json::from_slice(trim_json_line(line)).ok()?;
    let actual = record
        .pointer("/payload/model_provider")
        .and_then(Value::as_str)
        .or_else(|| record.get("model_provider").and_then(Value::as_str))?;
    if actual != expected {
        return None;
    }
    let needle = b"\"model_provider\"";
    let mut search_from = 0;
    let mut found = None;
    while search_from + needle.len() <= line.len() {
        let relative = line[search_from..]
            .windows(needle.len())
            .position(|window| window == needle)?;
        let key_start = search_from + relative;
        let mut cursor = key_start + needle.len();
        while line.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if line.get(cursor) != Some(&b':') {
            search_from = key_start + needle.len();
            continue;
        }
        cursor += 1;
        while line.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if line.get(cursor) != Some(&b'\"') {
            search_from = key_start + needle.len();
            continue;
        }
        let value_start = cursor + 1;
        let mut end = value_start;
        let mut escaped = false;
        while let Some(byte) = line.get(end).copied() {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'\"' {
                let value = serde_json::from_slice::<String>(&line[cursor..=end]).ok()?;
                if value == expected {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(cursor);
                }
                break;
            }
            end += 1;
        }
        search_from = key_start + needle.len();
    }
    found
}

fn codex_history_rollout_rewrites(
    client: &ClientConfig,
    target_provider: &str,
) -> Result<Vec<FileTokenRewrite>> {
    let mut paths = BTreeSet::new();
    for (_, database) in codex_history_database_paths(client) {
        if !database.is_file() {
            continue;
        }
        let Some(connection) =
            open_codex_history_database(&database, "cannot read Codex state database")?
        else {
            continue;
        };
        let Some(columns) = codex_history_columns(&connection)? else {
            continue;
        };
        if !columns.contains("rollout_path") {
            continue;
        }
        let mut statement =
            match connection.prepare("SELECT rollout_path FROM threads WHERE rollout_path != ''") {
                Ok(statement) => statement,
                Err(error) if is_unusable_history_database(&error) => continue,
                Err(error) => return Err(sqlite_error("cannot read Codex rollout paths", error)),
            };
        let rows = match statement.query_map([], |row| row.get::<_, String>(0)) {
            Ok(rows) => rows,
            Err(error) if is_unusable_history_database(&error) => continue,
            Err(error) => return Err(sqlite_error("cannot enumerate Codex rollouts", error)),
        };
        for row in rows {
            let raw = row.map_err(|error| sqlite_error("cannot read Codex rollout path", error))?;
            let path = PathBuf::from(raw);
            paths.insert(if path.is_absolute() {
                path
            } else {
                client.live_dir.join(path)
            });
        }
    }

    let mut rewrites = Vec::new();
    let mut seen = BTreeSet::new();
    for path in paths {
        let relative_path = path.strip_prefix(&client.live_dir).map_err(|_| {
            Error::new(
                ErrorCode::MixUnsupported,
                format!(
                    "Codex thread references a rollout outside its native home: {}",
                    path.display()
                ),
            )
        })?;
        let relative = relative_path.to_str().ok_or_else(|| {
            Error::new(
                ErrorCode::MixUnsupported,
                format!("Codex rollout path is not valid UTF-8: {}", path.display()),
            )
        })?;
        let safe_path = safe_child(&client.live_dir, relative)?;
        if !safe_path.is_file() {
            // A deleted or pruned native rollout must not block an account
            // switch. The SQLite row is still rebound; session discovery
            // reports the missing transcript separately.
            continue;
        }
        let line = match read_first_line(&safe_path) {
            Ok(line) => line,
            Err(error) if error.code == ErrorCode::MixValidationError => continue,
            Err(_error) if !safe_path.exists() => continue,
            Err(error) => return Err(error),
        };
        let Ok(record) = serde_json::from_slice::<Value>(trim_json_line(&line)) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(current) = record
            .pointer("/payload/model_provider")
            .and_then(Value::as_str)
            .or_else(|| record.get("model_provider").and_then(Value::as_str))
        else {
            continue;
        };
        if current == target_provider {
            continue;
        }
        let Some(offset) = model_provider_token_offset(&line, current) else {
            continue;
        };
        let key = (relative.to_owned(), offset as u64);
        if !seen.insert(key) {
            continue;
        }
        rewrites.push(FileTokenRewrite {
            relative: relative.to_owned(),
            offset: offset as u64,
            from: serde_json::to_vec(current)?,
            to: serde_json::to_vec(target_provider)?,
        });
    }
    Ok(rewrites)
}

fn trim_json_line(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && matches!(line[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    &line[..end]
}

fn verify_codex_history_projection(client: &ClientConfig, target_provider: &str) -> Result<()> {
    for (relative, database) in codex_history_database_paths(client) {
        if !database.exists() {
            continue;
        }
        let Some(connection) =
            open_codex_history_database(&database, "cannot verify Codex state database")?
        else {
            continue;
        };
        let Some(_) = codex_history_columns(&connection)? else {
            continue;
        };
        let mismatched: i64 = match connection.query_row(
            "SELECT COUNT(*) FROM threads WHERE model_provider IS NULL OR model_provider != ?1",
            [target_provider],
            |row| row.get(0),
        ) {
            Ok(value) => value,
            Err(error) if is_unusable_history_database(&error) => continue,
            Err(error) => {
                return Err(sqlite_error("cannot verify Codex thread providers", error));
            }
        };
        if mismatched != 0 {
            return Err(Error::new(
                ErrorCode::MixSwitchVerificationFailed,
                format!(
                    "Codex state database still contains {mismatched} old provider rows: {relative}"
                ),
            ));
        }
    }
    if !codex_history_rollout_rewrites(client, target_provider)?.is_empty() {
        return Err(Error::new(
            ErrorCode::MixSwitchVerificationFailed,
            "Codex rollout history still points at another Provider",
        ));
    }
    Ok(())
}

fn sync_live_credential(
    client: &ClientConfig,
    vault: &dyn CredentialVault,
) -> Result<(String, AccountIdentity)> {
    let document = CodexAdapter::auth_document(client)?;
    let fingerprint = CodexAdapter::fingerprint(&document).ok_or_else(|| {
        Error::new(
            ErrorCode::MixValidationError,
            "the current Codex login has no stable identity",
        )
    })?;
    let live_config = read_codex_config(&client.live_dir.join("config.toml"))?;
    let profile = matching_codex_profile(client, &fingerprint, &live_config).ok_or_else(|| {
        Error::new(
            ErrorCode::MixActiveAccountUnmanaged,
            "the current Codex login and Provider route do not identify one saved account",
        )
    })?;
    let target = client
        .profiles
        .get(&profile)
        .ok_or_else(|| Error::not_found("account", &profile))?;
    let reference = target.secret_files.get("auth.json").ok_or_else(|| {
        Error::new(
            ErrorCode::MixCredentialNotFound,
            "the active account has no credential reference",
        )
    })?;
    merge_codex_credential(vault, reference, &fingerprint, &document)?;
    Ok((profile, CodexAdapter::identity(&document)))
}

fn sync_live_codex_overlay(client: &ClientConfig, profile: &str) -> Result<()> {
    let source = client
        .profiles
        .get(profile)
        .and_then(|profile| profile.files.get("config.toml"))
        .ok_or_else(|| Error::invalid("the active Codex account has no configuration overlay"))?;
    let payload = codex_account_overlay_text(&client.live_dir.join("config.toml"))?;
    atomic_write(source, payload.as_bytes())
}

fn codex_switch_source(capability: AccountSwitchCapability) -> Result<Option<String>> {
    match capability.status {
        AccountSwitchStatus::Ready => capability.current_profile.map(Some).ok_or_else(|| {
            Error::new(
                ErrorCode::MixActiveAccountUnmanaged,
                "the current Codex login is not managed by Mix; add it before switching",
            )
        }),
        AccountSwitchStatus::SignInRequired => Ok(None),
        AccountSwitchStatus::FileAuthRequired => Err(Error::new(
            ErrorCode::MixCodexFileAuthRequired,
            "Codex must use file credential storage for managed account switching",
        )),
        AccountSwitchStatus::InvalidConfig => Err(Error::new(
            ErrorCode::MixConfigInvalid,
            "the current Codex configuration is invalid",
        )),
        AccountSwitchStatus::InvalidAuth => Err(Error::new(
            ErrorCode::MixValidationError,
            "the current Codex login file is invalid; sign in again before switching",
        )),
        AccountSwitchStatus::IdentityUnavailable => Err(Error::new(
            ErrorCode::MixValidationError,
            "the current Codex login has no stable account identity",
        )),
        AccountSwitchStatus::Unsupported => Err(Error::new(
            ErrorCode::MixUnsupported,
            "Codex account switching is unavailable",
        )),
    }
}

fn sync_target_credential(
    client: &ClientConfig,
    profile: &Profile,
    vault: &dyn CredentialVault,
) -> Result<()> {
    let document = CodexAdapter::auth_document(client)?;
    let fingerprint = profile.account_fingerprint.as_deref().ok_or_else(|| {
        Error::new(
            ErrorCode::MixValidationError,
            "the target Codex account has no stable identity",
        )
    })?;
    let reference = profile.secret_files.get("auth.json").ok_or_else(|| {
        Error::new(
            ErrorCode::MixCredentialNotFound,
            "the target account has no credential reference",
        )
    })?;
    merge_codex_credential(vault, reference, fingerprint, &document)
}

const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_OAUTH_RESPONSE_BYTES: u64 = 64 * 1024;
const CODEX_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES: i64 = 5;
const CODEX_TOKEN_REFRESH_INTERVAL_DAYS: i64 = 8;

fn refresh_managed_codex_credential(target: &Profile, vault: &dyn CredentialVault) -> Result<()> {
    let document = managed_codex_credential_document(target, vault)?;
    if document.get("auth_mode").and_then(Value::as_str) != Some("chatgpt")
        || !codex_auth_requires_refresh(&document, chrono::Utc::now())
    {
        return Ok(());
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| {
            Error::new(
                ErrorCode::MixAccountRefreshFailed,
                "Mix could not initialize secure Codex authentication",
            )
        })?;
    refresh_managed_codex_credential_with(target, vault, &client, CODEX_OAUTH_TOKEN_URL)
}

fn refresh_managed_codex_credential_with(
    target: &Profile,
    vault: &dyn CredentialVault,
    client: &reqwest::blocking::Client,
    endpoint: &str,
) -> Result<()> {
    let expected_fingerprint = target.account_fingerprint.as_deref().ok_or_else(|| {
        Error::new(
            ErrorCode::MixAccountReauthRequired,
            "the saved Codex login has no stable account identity",
        )
    })?;
    let reference = target.secret_files.get("auth.json").ok_or_else(|| {
        Error::new(
            ErrorCode::MixCredentialNotFound,
            "the target account has no credential reference",
        )
    })?;
    let document = managed_codex_credential_document(target, vault)?;
    if document.get("auth_mode").and_then(Value::as_str) != Some("chatgpt") {
        return Ok(());
    }

    let refreshed = request_codex_oauth_refresh(client, endpoint, &document)?;
    if CodexAdapter::fingerprint(&refreshed).as_deref() != Some(expected_fingerprint) {
        return Err(Error::new(
            ErrorCode::MixAccountReauthRequired,
            "Codex refreshed a different account; the saved login was not changed",
        ));
    }
    validate_codex_token_accounts(&refreshed)?;
    vault.set(reference, &serde_json::to_vec(&refreshed)?)
}

fn managed_codex_credential_document(
    target: &Profile,
    vault: &dyn CredentialVault,
) -> Result<Value> {
    let expected_fingerprint = target.account_fingerprint.as_deref().ok_or_else(|| {
        Error::new(
            ErrorCode::MixAccountReauthRequired,
            "the saved Codex login has no stable account identity",
        )
    })?;
    let reference = target.secret_files.get("auth.json").ok_or_else(|| {
        Error::new(
            ErrorCode::MixCredentialNotFound,
            "the target account has no credential reference",
        )
    })?;
    let payload = vault.get(reference).map_err(|_| {
        Error::new(
            ErrorCode::MixAccountReauthRequired,
            "the saved Codex login must be verified again",
        )
    })?;
    let document: Value = serde_json::from_slice(&payload).map_err(|_| {
        Error::new(
            ErrorCode::MixAccountReauthRequired,
            "the saved Codex login must be verified again",
        )
    })?;
    if !document.is_object()
        || CodexAdapter::fingerprint(&document).as_deref() != Some(expected_fingerprint)
    {
        return Err(Error::new(
            ErrorCode::MixAccountReauthRequired,
            "the saved Codex login belongs to another account",
        ));
    }
    if document.get("auth_mode").and_then(Value::as_str) == Some("chatgpt") {
        validate_codex_token_accounts(&document)?;
    }
    Ok(document)
}

fn codex_auth_requires_refresh(document: &Value, now: chrono::DateTime<chrono::Utc>) -> bool {
    if let Some(expires_at) = document
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .and_then(decode_jwt_claims)
        .and_then(|claims| claims.get("exp").and_then(Value::as_i64))
    {
        return expires_at
            <= (now + chrono::Duration::minutes(CODEX_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES))
                .timestamp();
    }
    codex_refresh_time(document).is_some_and(|last_refresh| {
        last_refresh < now - chrono::Duration::days(CODEX_TOKEN_REFRESH_INTERVAL_DAYS)
    })
}

fn request_codex_oauth_refresh(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    document: &Value,
) -> Result<Value> {
    let refresh_token = document
        .pointer("/tokens/refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            Error::new(
                ErrorCode::MixAccountReauthRequired,
                "the saved Codex login has no refresh token; add the account again",
            )
        })?;
    let response = client
        .post(endpoint)
        .json(&serde_json::json!({
            "client_id": CODEX_OAUTH_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .map_err(|_| {
            Error::new(
                ErrorCode::MixAccountRefreshFailed,
                "the target Codex login could not be refreshed; the current account was not changed",
            )
        })?;
    let status = response.status();
    let mut body = Vec::new();
    response
        .take(CODEX_OAUTH_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|_| {
            Error::new(
                ErrorCode::MixAccountRefreshFailed,
                "the Codex authentication service returned an unreadable response",
            )
        })?;
    if body.len() as u64 > CODEX_OAUTH_RESPONSE_BYTES {
        return Err(Error::new(
            ErrorCode::MixAccountRefreshFailed,
            "the Codex authentication service returned an oversized response",
        ));
    }
    if !status.is_success() {
        let code = oauth_error_code(&body);
        let permanent = oauth_refresh_requires_reauth(status, code.as_deref());
        return Err(Error::new(
            if permanent {
                ErrorCode::MixAccountReauthRequired
            } else {
                ErrorCode::MixAccountRefreshFailed
            },
            if permanent {
                "the saved Codex login has expired; add the account again"
            } else {
                "the target Codex login could not be refreshed; the current account was not changed"
            },
        )
        .details(serde_json::json!({
            "http_status": status.as_u16(),
            "oauth_code": code,
        })));
    }

    let refresh: Value = serde_json::from_slice(&body).map_err(|_| {
        Error::new(
            ErrorCode::MixAccountRefreshFailed,
            "the Codex authentication service returned an invalid response",
        )
    })?;
    apply_codex_oauth_refresh(document, &refresh)
}

fn oauth_refresh_requires_reauth(status: reqwest::StatusCode, code: Option<&str>) -> bool {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return true;
    }
    let code = code.map(str::to_ascii_lowercase);
    matches!(
        code.as_deref(),
        Some(
            "invalid_grant"
                | "refresh_token_expired"
                | "refresh_token_reused"
                | "refresh_token_invalidated"
        )
    )
}

fn apply_codex_oauth_refresh(document: &Value, refresh: &Value) -> Result<Value> {
    let mut refreshed = document.clone();
    let tokens = refreshed
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            Error::new(
                ErrorCode::MixAccountReauthRequired,
                "the saved Codex login has no token set; add the account again",
            )
        })?;
    let access_token = refresh
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            Error::new(
                ErrorCode::MixAccountRefreshFailed,
                "the Codex authentication service returned no access token",
            )
        })?;
    tokens.insert(
        "access_token".into(),
        Value::String(access_token.to_owned()),
    );
    for key in ["refresh_token", "id_token"] {
        if let Some(value) = refresh
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            tokens.insert(key.into(), Value::String(value.into()));
        }
    }
    refreshed["auth_mode"] = Value::String("chatgpt".into());
    refreshed["OPENAI_API_KEY"] = Value::Null;
    refreshed["last_refresh"] = Value::String(chrono::Utc::now().to_rfc3339());
    Ok(refreshed)
}

fn validate_codex_token_accounts(document: &Value) -> Result<()> {
    let expected = document
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Error::new(
                ErrorCode::MixAccountReauthRequired,
                "the saved Codex login has no account ID; add the account again",
            )
        })?;
    for key in ["id_token", "access_token"] {
        let Some(account) = document
            .pointer(&format!("/tokens/{key}"))
            .and_then(Value::as_str)
            .and_then(decode_jwt_claims)
            .and_then(|claims| codex_claim_account_id(&claims).map(str::to_owned))
        else {
            continue;
        };
        if account != expected {
            return Err(Error::new(
                ErrorCode::MixAccountReauthRequired,
                "the saved Codex tokens contain conflicting account identities; add the account again",
            ));
        }
    }
    Ok(())
}

fn codex_claim_account_id(claims: &Value) -> Option<&str> {
    claims
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(Value::as_object)
                .and_then(|auth| {
                    auth.get("chatgpt_account_id")
                        .or_else(|| auth.get("account_id"))
                })
                .and_then(Value::as_str)
        })
}

fn oauth_error_code(body: &[u8]) -> Option<String> {
    let document = serde_json::from_slice::<Value>(body).ok()?;
    let value = document
        .pointer("/error/code")
        .or_else(|| document.get("error"))
        .or_else(|| document.get("code"))?
        .as_str()?;
    (value.len() <= 80
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-".contains(character)))
    .then(|| value.to_owned())
}

pub(crate) fn merge_codex_credential(
    vault: &dyn CredentialVault,
    reference: &SecretRef,
    expected_fingerprint: &str,
    live: &Value,
) -> Result<()> {
    if CodexAdapter::fingerprint(live).as_deref() != Some(expected_fingerprint) {
        return Err(Error::new(
            ErrorCode::MixSwitchVerificationFailed,
            "the live Codex credential does not match its saved account",
        ));
    }
    let saved_payload = match vault.get(reference) {
        Ok(payload) => payload,
        Err(error) if error.code == ErrorCode::MixCredentialNotFound => {
            return vault.set(reference, &serde_json::to_vec(live)?);
        }
        Err(error) => return Err(error),
    };
    let Ok(saved) = serde_json::from_slice::<Value>(&saved_payload) else {
        return vault.set(reference, &serde_json::to_vec(live)?);
    };
    match CodexAdapter::fingerprint(&saved) {
        Some(saved_fingerprint) if saved_fingerprint != expected_fingerprint => {
            return Err(Error::new(
                ErrorCode::MixAccountReauthRequired,
                "the saved Codex credential belongs to another account",
            ));
        }
        None => return vault.set(reference, &serde_json::to_vec(live)?),
        Some(_) => {}
    }
    let live_refresh = codex_refresh_time(live);
    let saved_refresh = codex_refresh_time(&saved);
    match (saved_refresh, live_refresh) {
        (Some(saved), Some(live)) if saved >= live => return Ok(()),
        (Some(_), None) => return Ok(()),
        _ => {}
    }
    vault.set(reference, &serde_json::to_vec(live)?)
}

fn codex_refresh_time(document: &Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    document
        .get("last_refresh")
        .and_then(Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
}

pub(crate) fn verify_live_projection(client: &ClientConfig, target: &Profile) -> Result<()> {
    let live_overlay =
        codex_account_overlay(&read_codex_config(&client.live_dir.join("config.toml"))?);
    let target_config = target
        .files
        .get("config.toml")
        .ok_or_else(|| Error::invalid("Codex account has no configuration overlay"))?;
    let target_overlay = codex_account_overlay(&read_codex_config(target_config)?);
    if live_overlay != target_overlay {
        let expected = provider_from_toml(&target_overlay).id;
        let actual = provider_from_toml(&live_overlay).id;
        return Err(Error::new(
            ErrorCode::MixSwitchVerificationFailed,
            "the projected Codex provider route does not match the target account",
        )
        .details(BTreeMap::from([
            ("expected_provider", expected),
            ("actual_provider", actual),
        ])));
    }
    let document = CodexAdapter::auth_document(client)?;
    if target
        .account_fingerprint
        .as_deref()
        .is_some_and(|expected| CodexAdapter::fingerprint(&document).as_deref() != Some(expected))
    {
        return Err(Error::new(
            ErrorCode::MixSwitchVerificationFailed,
            "the projected Codex login does not match the target account",
        ));
    }
    let provider = codex_profile_provider(target)?.id;
    verify_codex_history_projection(client, &provider)?;
    Ok(())
}

fn verify_managed_codex_credential(target: &Profile, vault: &dyn CredentialVault) -> Result<()> {
    if target.account_fingerprint.is_none() {
        return Ok(());
    }
    managed_codex_credential_document(target, vault).map(drop)
}

fn verify_stable_codex_projection(client: &ClientConfig, target: &Profile) -> Result<()> {
    if target.account_fingerprint.is_none() {
        return Ok(());
    }
    for _ in 0..10 {
        thread::sleep(Duration::from_millis(200));
        verify_live_projection(client, target).map_err(|error| {
            Error::new(
                ErrorCode::MixSwitchVerificationFailed,
                format!("Codex replaced the target account after restart: {error}"),
            )
        })?;
    }
    Ok(())
}

fn codex_profile_provider(profile: &Profile) -> Result<ProviderIdentity> {
    let path = profile
        .files
        .get("config.toml")
        .ok_or_else(|| Error::invalid("profile has no config"))?;
    let document = read_codex_config(path)?;
    Ok(provider_from_toml(&document))
}

fn generated_codex_label(
    identity: &AccountIdentity,
    provider: Option<&ProviderIdentity>,
    fallback: &str,
) -> String {
    if identity
        .email
        .as_deref()
        .is_some_and(|value| !value.is_empty())
        || identity
            .name
            .as_deref()
            .is_some_and(|value| !value.is_empty())
    {
        return identity.display_label(fallback);
    }
    if let Some(provider) = provider.filter(|provider| !provider.official) {
        return match identity.suffix.as_deref() {
            Some(suffix) => format!("{} · ID {suffix}", provider.name),
            None => provider.name.clone(),
        };
    }
    identity.display_label(fallback)
}

#[derive(Default)]
pub struct ClaudeAdapter;

impl Adapter for ClaudeAdapter {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            kind: AdapterKind::Claude,
            name: "claude",
            label: "Claude Code",
            default_home: ".claude",
            default_command: &["claude"],
            session_roots: &["projects"],
            import_files: &["settings.json"],
            profile_category: ProfileCategory::Environment,
            profile_prefix: "environment",
        }
    }

    fn capabilities(
        &self,
        client: &ClientConfig,
        command_found: bool,
    ) -> BTreeMap<String, Capability> {
        let mut values = base_capabilities(command_found, self.desktop_process(client).is_some());
        values.insert(
            "session.list".into(),
            Capability {
                available: client.live_dir.is_dir(),
                support: CapabilitySupport::NativeFilesReadonly,
                authority: CapabilityAuthority::ReadOnly,
                fallback: None,
                reason: None,
                completeness: Some(Completeness::BestEffort),
            },
        );
        values.insert(
            "session.resume".into(),
            Capability {
                available: command_found,
                support: CapabilitySupport::NativeCli,
                authority: CapabilityAuthority::Launch,
                fallback: None,
                reason: None,
                completeness: Some(Completeness::Complete),
            },
        );
        values
    }

    fn account_capability(&self, _: &ClientConfig) -> AccountSwitchCapability {
        AccountSwitchCapability::default()
    }

    fn capture_payload(&self, client: &ClientConfig, relative: &str) -> Result<Vec<u8>> {
        if relative != "settings.json" {
            return Err(Error::new(
                ErrorCode::MixUnsupported,
                "Claude environment import only supports settings.json",
            ));
        }
        let source = safe_child(&client.live_dir, relative)?;
        validated_claude_settings(&source)
    }

    fn validate_profile(&self, _: &ClientConfig, profile: &Profile) -> Result<()> {
        validate_managed_paths(profile, &["settings.json"], &[])?;
        if let Some(settings) = profile.files.get("settings.json") {
            validated_claude_settings(settings)?;
        }
        Ok(())
    }

    fn sessions(
        &self,
        app: &str,
        client: &ClientConfig,
        query: Option<&str>,
    ) -> Result<Vec<Session>> {
        claude_sessions(app, client, query)
    }

    fn resume_argv(&self, client: &ClientConfig, native_id: &str) -> Result<Vec<String>> {
        validate_native_id(native_id)?;
        let mut argv = configured_command(client)?;
        argv.splice(1..1, ["--resume".into(), native_id.into()]);
        Ok(argv)
    }

    fn runtime_variable(&self) -> Option<&'static str> {
        Some("CLAUDE_CONFIG_DIR")
    }

    fn native_session_environment_profile(
        &self,
        client: &ClientConfig,
        project_profile: Option<&str>,
    ) -> Option<String> {
        project_profile
            .map(str::to_owned)
            .or_else(|| client.active_profile.clone())
    }
}

fn validated_claude_settings(path: &Path) -> Result<Vec<u8>> {
    let payload = read_bounded(path, 16 * 1024 * 1024, "cannot read Claude settings")?;
    let document: Value = serde_json::from_slice(&payload).map_err(|_| {
        Error::new(
            ErrorCode::MixValidationError,
            "Claude settings.json is not valid JSON",
        )
    })?;
    if !document.is_object() {
        return Err(Error::invalid(
            "Claude settings.json must contain an object",
        ));
    }
    if contains_sensitive_settings(&document) {
        return Err(Error::new(
            ErrorCode::MixSensitiveDataRejected,
            "Claude settings contain authentication material; use advanced provider settings with a local credential reference",
        ));
    }
    serde_json::to_vec_pretty(&document).map_err(Error::from)
}

fn contains_sensitive_settings(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    for (key, child) in object {
        if sensitive_setting_name(key) {
            return true;
        }
        if contains_sensitive_settings(child) {
            return true;
        }
        if let Some(items) = child.as_array() {
            for item in items {
                if contains_sensitive_settings(item) {
                    return true;
                }
            }
        }
    }
    false
}

fn sensitive_setting_name(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    matches!(
        normalized.as_str(),
        "apikey"
            | "apikeyhelper"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "secret"
            | "password"
            | "credential"
            | "authorization"
            | "auth"
    ) || normalized.contains("apikey")
        || normalized.contains("token")
        || normalized.contains("secret")
        || normalized.contains("credential")
        || normalized.contains("password")
        || normalized.contains("authorization")
}

fn validate_managed_paths(
    profile: &Profile,
    allowed_files: &[&str],
    allowed_secret_files: &[&str],
) -> Result<()> {
    let invalid_file = profile
        .files
        .keys()
        .find(|path| !allowed_files.contains(&path.as_str()));
    let invalid_secret = profile
        .secret_files
        .keys()
        .find(|path| !allowed_secret_files.contains(&path.as_str()));
    if let Some(path) = invalid_file.or(invalid_secret) {
        return Err(Error::new(
            ErrorCode::MixValidationError,
            format!("the adapter does not manage this client path: {path}"),
        ));
    }
    Ok(())
}

fn base_capabilities(command_found: bool, process_control: bool) -> BTreeMap<String, Capability> {
    [
        ("identity.detect", CapabilityAuthority::ReadOnly, false),
        (
            "identity.capture",
            CapabilityAuthority::CredentialProjection,
            false,
        ),
        (
            "identity.enroll",
            CapabilityAuthority::CredentialProjection,
            false,
        ),
        (
            "identity.switch_global",
            CapabilityAuthority::CredentialProjection,
            false,
        ),
        (
            "environment.run_isolated",
            CapabilityAuthority::Launch,
            command_found,
        ),
        ("session.list", CapabilityAuthority::ReadOnly, false),
        ("session.resume", CapabilityAuthority::Launch, false),
        ("session.open_native", CapabilityAuthority::Launch, false),
        ("project.discover", CapabilityAuthority::ReadOnly, true),
        ("project.open_native", CapabilityAuthority::Launch, false),
        (
            "process.control",
            CapabilityAuthority::Launch,
            process_control,
        ),
    ]
    .into_iter()
    .map(|(name, authority, available)| {
        (
            name.into(),
            Capability {
                available,
                support: if available {
                    CapabilitySupport::Configured
                } else {
                    CapabilitySupport::Unsupported
                },
                authority,
                fallback: None,
                reason: None,
                completeness: None,
            },
        )
    })
    .collect()
}

fn codex_desktop_process(client: &ClientConfig) -> Option<ProcessSpec> {
    #[cfg(target_os = "macos")]
    if let Some(home) = dirs::home_dir() {
        let candidates = [
            PathBuf::from("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"),
            home.join("Applications/ChatGPT.app/Contents/MacOS/ChatGPT"),
        ]
        .into_iter()
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
        let executable = match candidates.as_slice() {
            [only] => only.clone(),
            _ => running_executable(&candidates)
                .ok()
                .flatten()
                .or_else(|| candidates.first().cloned())?,
        };
        let mut process = codex_desktop_process_for(client, &home, executable)?;
        process.managed_executables = candidates;
        return Some(process);
    }
    let _ = client;
    None
}

fn codex_desktop_process_for(
    client: &ClientConfig,
    home: &Path,
    executable: PathBuf,
) -> Option<ProcessSpec> {
    if client.live_dir != home.join(".codex") || !executable.is_file() {
        return None;
    }
    let app = executable.parent()?.parent()?.parent()?.to_path_buf();
    if app.extension().and_then(|value| value.to_str()) != Some("app") {
        return None;
    }
    let ready_executable = app.join("Contents/Resources/codex");
    let app = app.display().to_string();
    Some(ProcessSpec {
        managed_executables: vec![executable.clone()],
        executable: Some(executable),
        ready_executable: Some(ready_executable),
        grace_seconds: 8,
        launch: vec!["/usr/bin/open".into(), "-a".into(), app.clone()],
        activate: vec!["/usr/bin/open".into(), "-a".into(), app],
        ready_timeout: 12,
    })
}

pub fn discover_executable(client: &ClientConfig) -> Option<PathBuf> {
    let value = client.run.command.first()?;
    let path = Path::new(value);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    let home = dirs::home_dir()?;
    let candidates = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .chain([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
            home.join(".local/bin"),
            PathBuf::from("/Applications/ChatGPT.app/Contents/Resources"),
            home.join("Applications/ChatGPT.app/Contents/Resources"),
        ])
        .map(|root| root.join(value));
    candidates.into_iter().find(|path| path.is_file())
}

fn configured_command(client: &ClientConfig) -> Result<Vec<String>> {
    if client.run.command.is_empty() || client.run.command.iter().any(String::is_empty) {
        return Err(Error::new(
            ErrorCode::MixConfigInvalid,
            "client run command is not configured",
        ));
    }
    let mut command = client.run.command.clone();
    if let Some(executable) = discover_executable(client) {
        command[0] = executable.display().to_string();
    }
    Ok(command)
}

fn read_toml(path: &Path) -> Result<toml::Table> {
    let payload = read_bounded(path, 2 * 1024 * 1024, "cannot read client config")?;
    let text = std::str::from_utf8(&payload).map_err(|_| {
        Error::new(
            ErrorCode::MixValidationError,
            "client config is not valid UTF-8",
        )
    })?;
    toml::from_str(text).map_err(|error| {
        Error::new(
            ErrorCode::MixValidationError,
            format!("invalid client TOML: {error}"),
        )
    })
}

pub(crate) fn validate_codex_config(document: &toml::Table) -> Result<()> {
    fn looks_like_secret(value: &str) -> bool {
        let value = value.trim();
        let lower = value.to_ascii_lowercase();
        lower.starts_with("bearer ")
            || lower.starts_with("sk-")
            || lower.starts_with("ghp_")
            || lower.starts_with("github_pat_")
            || lower.starts_with("xoxb-")
            || lower.starts_with("xoxp-")
            || value.starts_with("AKIA")
            || value.starts_with("AIza")
            || (value.starts_with("eyJ") && value.matches('.').count() == 2 && value.len() > 40)
    }

    fn contains_literal_secret(value: &toml::Value) -> bool {
        !value.as_table().is_some_and(|values| {
            values.iter().all(|(key, value)| {
                !sensitive_setting_name(key) && !value.as_str().is_some_and(looks_like_secret)
            })
        })
    }

    fn invalid_environment_reference(
        key: &str,
        value: &toml::Value,
        ancestors: &[String],
    ) -> Option<&'static str> {
        let in_provider = ancestors.iter().any(|value| value == "model_providers");
        let in_mcp = ancestors.iter().any(|value| value == "mcp_servers");
        match key {
            "env_key" if in_provider => (!value.as_str().is_some_and(is_environment_variable_name))
                .then_some("an invalid provider environment-variable reference"),
            "env_http_headers" if in_provider => (!value.as_table().is_some_and(|headers| {
                headers
                    .values()
                    .all(|value| value.as_str().is_some_and(is_environment_variable_name))
            }))
            .then_some("an invalid environment-backed HTTP header reference"),
            "bearer_token_env_var" if in_mcp => {
                (!value.as_str().is_some_and(is_environment_variable_name))
                    .then_some("an invalid MCP bearer-token environment-variable reference")
            }
            "env_vars" if in_mcp => (!value.as_array().is_some_and(|names| {
                names
                    .iter()
                    .all(|value| value.as_str().is_some_and(is_environment_variable_name))
            }))
            .then_some("an invalid MCP environment-variable reference"),
            _ => None,
        }
    }

    fn rejected_value(value: &toml::Value, ancestors: &mut Vec<String>) -> Option<&'static str> {
        match value {
            toml::Value::Table(table) => rejected_setting(table, ancestors),
            toml::Value::Array(values) => values
                .iter()
                .find_map(|value| rejected_value(value, ancestors)),
            _ => None,
        }
    }

    fn rejected_setting(table: &toml::Table, ancestors: &mut Vec<String>) -> Option<&'static str> {
        for (key, value) in table {
            let reason =
                invalid_environment_reference(key, value, ancestors).or(match key.as_str() {
                    "experimental_bearer_token" => Some("a direct bearer token"),
                    "http_headers" if contains_literal_secret(value) => {
                        Some("credential-bearing static HTTP headers")
                    }
                    "query_params" if contains_literal_secret(value) => {
                        Some("credential-bearing static provider query parameters")
                    }
                    "env"
                        if ancestors.iter().any(|value| value == "mcp_servers")
                            && contains_literal_secret(value) =>
                    {
                        Some("credential-bearing MCP environment values")
                    }
                    "set"
                        if ancestors
                            .last()
                            .is_some_and(|value| value == "shell_environment_policy")
                            && contains_literal_secret(value) =>
                    {
                        Some("credential-bearing shell environment values")
                    }
                    _ => None,
                });
            if reason.is_some() {
                return reason;
            }
            ancestors.push(key.clone());
            let result = rejected_value(value, ancestors);
            ancestors.pop();
            if result.is_some() {
                return result;
            }
        }
        None
    }

    let Some(reason) = rejected_setting(document, &mut Vec::new()) else {
        return Ok(());
    };
    Err(Error::new(
        ErrorCode::MixSensitiveDataRejected,
        format!(
            "Codex config contains {reason}; move secrets to env_key, env_http_headers, bearer_token_env_var, or env_vars before adding the account"
        ),
    ))
}

pub(crate) fn provider_from_toml(document: &toml::Table) -> ProviderIdentity {
    let id = document
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai")
        .to_owned();
    let definition = document
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .and_then(|values| values.get(&id))
        .and_then(toml::Value::as_table);
    let endpoint_overridden = [
        "chatgpt_base_url",
        "experimental_realtime_ws_base_url",
        "openai_base_url",
    ]
    .iter()
    .any(|key| document.contains_key(*key))
        || definition.is_some_and(|provider| provider.contains_key("base_url"));
    let official =
        matches!(id.to_ascii_lowercase().as_str(), "openai" | "chatgpt") && !endpoint_overridden;
    let name = definition
        .and_then(|provider| provider.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if official {
                "OpenAI".into()
            } else {
                id.clone()
            }
        });
    ProviderIdentity { id, name, official }
}

pub(crate) fn codex_endpoint_host(document: &toml::Table) -> Option<String> {
    let provider_id = document
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai");
    let provider = document
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .and_then(|providers| providers.get(provider_id))
        .and_then(toml::Value::as_table);
    let raw = provider
        .and_then(|value| value.get("base_url"))
        .and_then(toml::Value::as_str)
        .or_else(|| {
            document
                .get("openai_base_url")
                .and_then(toml::Value::as_str)
        })
        .or_else(|| {
            document
                .get("chatgpt_base_url")
                .and_then(toml::Value::as_str)
        });
    raw.and_then(endpoint_host).or_else(|| {
        matches!(
            provider_id.to_ascii_lowercase().as_str(),
            "openai" | "chatgpt"
        )
        .then_some("api.openai.com".into())
    })
}

fn endpoint_host(value: &str) -> Option<String> {
    let (_, authority) = value.trim().split_once("://")?;
    let authority = authority.split(['/', '?', '#']).next()?.trim();
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if authority.is_empty() || authority.chars().any(char::is_control) {
        return None;
    }
    Some(authority.to_ascii_lowercase())
}

const CODEX_ROUTE_KEYS: &[&str] = &[
    "chatgpt_base_url",
    "experimental_realtime_ws_base_url",
    "model_catalog_json",
    "openai_base_url",
    "wire_api",
];

fn codex_route(document: &toml::Table) -> toml::Table {
    let provider = document
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai")
        .to_owned();
    let mut route = CODEX_ROUTE_KEYS
        .iter()
        .filter_map(|key| {
            document
                .get(*key)
                .cloned()
                .map(|value| ((*key).to_owned(), value))
        })
        .collect::<toml::Table>();
    route.insert(
        "model_provider".into(),
        toml::Value::String(provider.clone()),
    );
    if let Some(definition) = document
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .and_then(|providers| providers.get(&provider))
        .cloned()
    {
        route.insert(
            "model_providers".into(),
            toml::Value::Table(toml::Table::from_iter([(provider, definition)])),
        );
    }
    route
}

pub(crate) fn matching_codex_profile(
    client: &ClientConfig,
    credential_fingerprint: &str,
    current_config: &toml::Table,
) -> Option<String> {
    let route = codex_route(current_config);
    let exact = client
        .profiles
        .iter()
        .filter(|(_, profile)| {
            profile.account_fingerprint.as_deref() == Some(credential_fingerprint)
        })
        .filter(|(_, profile)| {
            profile
                .files
                .get("config.toml")
                .and_then(|path| read_toml(path).ok())
                .is_some_and(|document| codex_route(&document) == route)
        })
        .collect::<Vec<_>>();
    if exact.len() == 1 {
        return Some(exact[0].0.clone());
    }
    None
}

fn decode_jwt_claims(jwt: &str) -> Option<Value> {
    if jwt.len() > 2 * 1024 * 1024 {
        return None;
    }
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn first_text(documents: &[Value], keys: &[&str], maximum: usize) -> Option<String> {
    documents.iter().find_map(|document| {
        keys.iter().find_map(|key| {
            document
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty() && value.len() <= maximum)
                .map(str::to_owned)
        })
    })
}

fn validate_native_id(value: &str) -> Result<()> {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return Err(Error::new(
            ErrorCode::MixRequestInvalid,
            "native session id is invalid",
        ));
    };
    if value.len() > 512
        || !first.is_ascii_alphanumeric()
        || characters.any(|character| {
            !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
        })
    {
        return Err(Error::new(
            ErrorCode::MixRequestInvalid,
            "native session id is invalid",
        ));
    }
    Ok(())
}

fn resume_id(app: &str, state_dir: &Path, native_id: &str) -> String {
    let value = format!("{app}\0{}\0{native_id}", state_dir.display());
    format!("mix-session-v1-{:x}", Sha256::digest(value.as_bytes()))
}

fn bounded_text(value: Option<&str>, fallback: &str) -> String {
    let bidi = [
        '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
        '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
    ];
    let text = value
        .unwrap_or(fallback)
        .chars()
        .filter(|ch| !ch.is_control() && !bidi.contains(ch))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        fallback.to_owned()
    } else if text.chars().count() > SESSION_TITLE_CHARS {
        format!(
            "{}…",
            text.chars().take(SESSION_TITLE_CHARS).collect::<String>()
        )
    } else {
        text
    }
}

fn codex_session_title(value: &str) -> Option<String> {
    const REQUEST_MARKER: &str = "## My request:";
    const SYNTHETIC_PREFIXES: &[&str] = &[
        "<app-context",
        "<collaboration_mode",
        "<environment_context",
        "<in-app-browser-context",
        "<permissions instructions",
        "<recommended_plugins",
        "<skills_instructions",
        "# AGENTS.md instructions for ",
    ];

    let value = value.trim();
    let injected_context = SYNTHETIC_PREFIXES
        .iter()
        .any(|prefix| value.contains(prefix))
        || value.contains("# Files mentioned by the user:");
    let candidate = if injected_context {
        value
            .rfind(REQUEST_MARKER)
            .map(|offset| value[offset + REQUEST_MARKER.len()..].trim())
            .unwrap_or(value)
    } else {
        value
    };
    if candidate.is_empty()
        || SYNTHETIC_PREFIXES
            .iter()
            .any(|prefix| candidate.starts_with(prefix))
    {
        None
    } else {
        Some(bounded_text(Some(candidate), "Session"))
    }
}

fn modified_iso(path: &Path) -> Option<String> {
    let duration = path
        .metadata()
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    chrono::DateTime::<chrono::Utc>::from_timestamp(
        duration.as_secs() as i64,
        duration.subsec_nanos(),
    )
    .map(|value| value.to_rfc3339())
}

fn rollout_sessions(app: &str, client: &ClientConfig, query: Option<&str>) -> Result<Vec<Session>> {
    let root = fs::canonicalize(&client.live_dir).unwrap_or_else(|_| client.live_dir.clone());
    let query = query.map(|value| (value.to_lowercase(), value.to_uppercase()));
    let mut sessions = Vec::new();
    let mut seen = HashSet::new();
    let mut scanned_bytes = 0_u64;
    let mut candidates = Vec::new();
    for relative in ["sessions", "archived_sessions"] {
        let directory = root.join(relative);
        if !directory.is_dir() {
            continue;
        }
        candidates.extend(session_candidates(&directory, &root));
        if candidates.len() >= SESSION_MAX_CANDIDATES {
            break;
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.1));
    candidates.truncate(SESSION_MAX_FILES);
    for (canonical, _) in candidates {
        if scanned_bytes >= SESSION_MAX_TOTAL_BYTES {
            break;
        }
        let Ok(mut reader) = bounded_reader(&canonical) else {
            continue;
        };
        let mut first = String::new();
        let first_limit = (SESSION_MAX_TOTAL_BYTES - scanned_bytes).min(SESSION_META_BYTES);
        let Ok(first_bytes) = reader.by_ref().take(first_limit + 1).read_line(&mut first) else {
            continue;
        };
        if first_bytes as u64 > first_limit {
            break;
        }
        scanned_bytes = scanned_bytes.saturating_add(first_bytes as u64);
        let Ok(meta) = serde_json::from_str::<Value>(&first) else {
            continue;
        };
        if meta.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = meta.get("payload").and_then(Value::as_object);
        let native_id = payload
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .or_else(|| canonical.file_stem().and_then(|value| value.to_str()))
            .unwrap_or_default();
        if validate_native_id(native_id).is_err() || !seen.insert(native_id.to_owned()) {
            continue;
        }
        let cwd = payload
            .and_then(|value| value.get("cwd"))
            .and_then(Value::as_str)
            .filter(|value| value.len() <= 32 * 1024 && !value.contains('\0'))
            .map(str::to_owned);
        let provider = payload
            .and_then(|value| value.get("model_provider"))
            .and_then(Value::as_str)
            .map(|value| bounded_text(Some(value), "unknown"));
        let title_limit = (SESSION_MAX_TOTAL_BYTES - scanned_bytes).min(SESSION_SCAN_BYTES);
        let (title, title_bytes) = scan_codex_title(&mut reader, title_limit as usize);
        scanned_bytes = scanned_bytes.saturating_add(title_bytes as u64);
        let timestamp_limit = (SESSION_MAX_TOTAL_BYTES - scanned_bytes).min(SESSION_META_BYTES);
        let (native_updated_at, timestamp_bytes) =
            latest_native_timestamp(&canonical, timestamp_limit as usize);
        scanned_bytes = scanned_bytes.saturating_add(timestamp_bytes as u64);
        let title = title.unwrap_or_else(|| native_id.to_owned());
        let haystack = format!(
            "{native_id} {title} {} {}",
            cwd.as_deref().unwrap_or(""),
            provider.as_deref().unwrap_or("")
        );
        if query
            .as_ref()
            .is_some_and(|(lower, upper)| !contains_case_insensitive(&haystack, lower, upper))
        {
            continue;
        }
        let command_found = discover_executable(client).is_some();
        let working_directory_exists = cwd
            .as_deref()
            .is_some_and(|value| Path::new(value).is_dir());
        let recoverable = command_found && working_directory_exists && canonical.is_file();
        let recovery_reason = if recoverable {
            SessionRecoveryReason::Verified
        } else if !command_found {
            SessionRecoveryReason::UnsupportedCommand
        } else if !working_directory_exists {
            SessionRecoveryReason::WorkingDirectoryMissing
        } else {
            SessionRecoveryReason::TranscriptMissing
        };
        let native_command = CodexAdapter
            .resume_argv(client, native_id)
            .ok()
            .map(|argv| resume_command("CODEX_HOME", &root, &argv));
        sessions.push(Session {
            id: native_id.into(),
            title: Some(title),
            app: app.into(),
            profile: None,
            provider,
            workspace: cwd.clone(),
            project_id: cwd.clone(),
            project_name: cwd
                .as_ref()
                .and_then(|value| Path::new(value).file_name())
                .and_then(|value| value.to_str())
                .map(str::to_owned),
            project_path: cwd.clone(),
            project_source: Some(SessionProjectSource::Directory),
            project_registered: Some(false),
            project_exists: cwd.as_ref().map(|value| Path::new(value).is_dir()),
            project_session_count: None,
            cwd,
            transcript: Some(canonical.display().to_string()),
            catalog_source: SessionCatalogSource::NativeFilesReadonly,
            native_command,
            resume_id: resume_id(app, &root, native_id),
            state_dir: root.display().to_string(),
            updated_at: native_updated_at.or_else(|| modified_iso(&canonical)),
            recoverability: if recoverable {
                Recoverability::A
            } else {
                Recoverability::B
            },
            recovery_reason: Some(recovery_reason),
        });
    }
    sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(sessions)
}

fn scan_codex_title<R: Read>(
    reader: &mut BufReader<R>,
    byte_limit: usize,
) -> (Option<String>, usize) {
    let mut consumed = 0usize;
    for _ in 0..200 {
        if consumed >= byte_limit {
            break;
        }
        let mut line = String::new();
        let line_limit = SESSION_LINE_BYTES.min(byte_limit - consumed);
        let read = reader
            .by_ref()
            .take((line_limit + 1) as u64)
            .read_line(&mut line)
            .unwrap_or_default();
        if read == 0 || read > line_limit {
            break;
        }
        consumed += read;
        let event: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(payload) = event.get("payload") else {
            continue;
        };
        let title = if event.get("type").and_then(Value::as_str) == Some("event_msg")
            && payload.get("type").and_then(Value::as_str) == Some("user_message")
        {
            payload.get("message").and_then(Value::as_str)
        } else if event.get("type").and_then(Value::as_str) == Some("response_item")
            && payload.get("type").and_then(Value::as_str) == Some("message")
            && payload.get("role").and_then(Value::as_str) == Some("user")
        {
            payload
                .get("content")
                .and_then(Value::as_array)
                .and_then(|content| {
                    content
                        .iter()
                        .find_map(|part| part.get("text").and_then(Value::as_str))
                })
        } else {
            None
        };
        if let Some(title) = title.and_then(codex_session_title) {
            return (Some(title), consumed);
        }
    }
    (None, consumed)
}

fn claude_sessions(app: &str, client: &ClientConfig, query: Option<&str>) -> Result<Vec<Session>> {
    let root = fs::canonicalize(&client.live_dir).unwrap_or_else(|_| client.live_dir.clone());
    let query = query.map(|value| (value.to_lowercase(), value.to_uppercase()));
    let directory = root.join("projects");
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    let mut scanned_bytes = 0_u64;
    for (canonical, _) in session_candidates(&directory, &root)
        .into_iter()
        .take(SESSION_MAX_FILES)
    {
        if scanned_bytes >= SESSION_MAX_TOTAL_BYTES {
            break;
        }
        let native_id = canonical
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if validate_native_id(native_id).is_err() {
            continue;
        }
        let byte_limit = (SESSION_MAX_TOTAL_BYTES - scanned_bytes).min(SESSION_SCAN_BYTES);
        let (title, cwd, consumed) = scan_claude_metadata(&canonical, byte_limit as usize);
        scanned_bytes = scanned_bytes.saturating_add(consumed as u64);
        let title = title.unwrap_or_else(|| native_id.to_owned());
        let haystack = format!("{native_id} {title} {}", cwd.as_deref().unwrap_or(""));
        if query
            .as_ref()
            .is_some_and(|(lower, upper)| !contains_case_insensitive(&haystack, lower, upper))
        {
            continue;
        }
        let command_found = discover_executable(client).is_some();
        let working_directory_exists = cwd
            .as_deref()
            .is_some_and(|value| Path::new(value).is_dir());
        let recoverable = command_found && working_directory_exists && canonical.is_file();
        let recovery_reason = if recoverable {
            SessionRecoveryReason::Verified
        } else if !command_found {
            SessionRecoveryReason::UnsupportedCommand
        } else if !working_directory_exists {
            SessionRecoveryReason::WorkingDirectoryMissing
        } else {
            SessionRecoveryReason::TranscriptMissing
        };
        let native_command = ClaudeAdapter
            .resume_argv(client, native_id)
            .ok()
            .map(|argv| resume_command("CLAUDE_CONFIG_DIR", &root, &argv));
        sessions.push(Session {
            id: native_id.into(),
            title: Some(title),
            app: app.into(),
            profile: None,
            provider: None,
            workspace: cwd.clone(),
            project_id: cwd.clone(),
            project_name: cwd
                .as_ref()
                .and_then(|value| Path::new(value).file_name())
                .and_then(|value| value.to_str())
                .map(str::to_owned),
            project_path: cwd.clone(),
            project_source: Some(SessionProjectSource::Directory),
            project_registered: Some(false),
            project_exists: cwd.as_ref().map(|value| Path::new(value).is_dir()),
            project_session_count: None,
            cwd,
            transcript: Some(canonical.display().to_string()),
            catalog_source: SessionCatalogSource::NativeFilesReadonly,
            native_command,
            resume_id: resume_id(app, &root, native_id),
            state_dir: root.display().to_string(),
            updated_at: modified_iso(&canonical),
            recoverability: if recoverable {
                Recoverability::A
            } else {
                Recoverability::B
            },
            recovery_reason: Some(recovery_reason),
        });
    }
    sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(sessions)
}

fn scan_claude_metadata(path: &Path, byte_limit: usize) -> (Option<String>, Option<String>, usize) {
    let Ok(mut reader) = bounded_reader(path) else {
        return (None, None, 0);
    };
    let mut title = None;
    let mut cwd = None;
    let mut consumed = 0usize;
    for _ in 0..200 {
        if consumed >= byte_limit {
            break;
        }
        let mut line = String::new();
        let line_limit = SESSION_LINE_BYTES.min(byte_limit - consumed);
        let Ok(read) = reader
            .by_ref()
            .take((line_limit + 1) as u64)
            .read_line(&mut line)
        else {
            break;
        };
        if read == 0 || read > line_limit {
            break;
        }
        consumed += read;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) == Some("summary") {
            title = event
                .get("summary")
                .and_then(Value::as_str)
                .map(|value| bounded_text(Some(value), "Session"));
        }
        if cwd.is_none() {
            cwd = event
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|value| value.len() <= 32 * 1024 && !value.contains('\0'))
                .map(str::to_owned);
        }
    }
    (title, cwd, consumed)
}

fn session_candidates(directory: &Path, root: &Path) -> Vec<(PathBuf, Option<SystemTime>)> {
    let mut candidates = WalkDir::new(directory)
        .follow_links(false)
        .max_depth(SESSION_MAX_DEPTH)
        .sort_by(|left, right| right.file_name().cmp(left.file_name()))
        .into_iter()
        .take(SESSION_MAX_WALK_ENTRIES)
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
        })
        .filter_map(|entry| {
            let path = fs::canonicalize(entry.path()).ok()?;
            if !path.starts_with(root) {
                return None;
            }
            let modified = entry
                .metadata()
                .ok()
                .and_then(|value| value.modified().ok());
            Some((path, modified))
        })
        .take(SESSION_MAX_CANDIDATES)
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.1));
    candidates
}

fn bounded_reader(path: &Path) -> std::io::Result<BufReader<File>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    Ok(BufReader::new(file))
}

fn latest_native_timestamp(path: &Path, byte_limit: usize) -> (Option<String>, usize) {
    if byte_limit == 0 {
        return (None, 0);
    }
    let Ok(mut reader) = bounded_reader(path) else {
        return (None, 0);
    };
    let Ok(length) = reader.get_ref().metadata().map(|value| value.len()) else {
        return (None, 0);
    };
    let start = length.saturating_sub(byte_limit as u64);
    if reader.seek(SeekFrom::Start(start)).is_err() {
        return (None, 0);
    }
    let mut tail = Vec::with_capacity((length - start) as usize);
    if reader
        .by_ref()
        .take(byte_limit as u64)
        .read_to_end(&mut tail)
        .is_err()
    {
        return (None, 0);
    }
    let consumed = tail.len();
    let complete = if start == 0 {
        tail.as_slice()
    } else {
        tail.iter()
            .position(|byte| *byte == b'\n')
            .map(|index| &tail[index + 1..])
            .unwrap_or_default()
    };
    let latest = complete
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .filter_map(|event| {
            event
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&chrono::Utc))
        })
        .max();
    (latest.map(|value| value.to_rfc3339()), consumed)
}

fn contains_case_insensitive(haystack: &str, lower: &str, upper: &str) -> bool {
    haystack.to_lowercase().contains(lower) || haystack.to_uppercase().contains(upper)
}

fn resume_command(variable: &str, state_dir: &Path, argv: &[String]) -> String {
    format!(
        "{variable}={} {}",
        shell_quote(&state_dir.display().to_string()),
        argv.iter()
            .map(|value| shell_quote(value))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_jwt(account_id: &str) -> String {
        let claims = serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
        });
        format!(
            "e30.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    fn test_jwt_with_expiration(account_id: &str, expiration: i64) -> String {
        let claims = serde_json::json!({
            "exp": expiration,
            "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
        });
        format!(
            "e30.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    #[test]
    fn oauth_refresh_preserves_identity_and_replaces_every_returned_token() {
        let original = serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "last_refresh": "2026-09-04T00:00:00Z",
            "tokens": {
                "account_id": "account-a",
                "access_token": test_jwt("account-a"),
                "refresh_token": "old-refresh",
                "id_token": test_jwt("account-a")
            }
        });
        let response = serde_json::json!({
            "access_token": test_jwt("account-a"),
            "refresh_token": "new-refresh",
            "id_token": test_jwt("account-a")
        });

        let refreshed = apply_codex_oauth_refresh(&original, &response).unwrap();

        assert_eq!(refreshed["tokens"]["account_id"], "account-a");
        assert_eq!(refreshed["tokens"]["refresh_token"], "new-refresh");
        assert_ne!(refreshed["last_refresh"], original["last_refresh"]);
        validate_codex_token_accounts(&refreshed).unwrap();
    }

    #[test]
    fn oauth_refresh_requires_a_new_access_token() {
        let original = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "account_id": "account-a",
                "access_token": test_jwt("account-a"),
                "refresh_token": "old-refresh"
            }
        });
        let response = serde_json::json!({"refresh_token": "new-refresh"});

        let error = apply_codex_oauth_refresh(&original, &response).unwrap_err();

        assert_eq!(error.code, ErrorCode::MixAccountRefreshFailed);
        assert_eq!(original["tokens"]["refresh_token"], "old-refresh");
    }

    #[test]
    fn oauth_refresh_rejects_cross_account_tokens() {
        let document = serde_json::json!({
            "tokens": {
                "account_id": "account-a",
                "access_token": test_jwt("account-b")
            }
        });

        let error = validate_codex_token_accounts(&document).unwrap_err();

        assert_eq!(error.code, ErrorCode::MixAccountReauthRequired);
    }

    #[test]
    fn oauth_refresh_only_requires_login_for_terminal_failures() {
        assert!(oauth_refresh_requires_reauth(
            reqwest::StatusCode::UNAUTHORIZED,
            None
        ));
        assert!(oauth_refresh_requires_reauth(
            reqwest::StatusCode::BAD_REQUEST,
            Some("invalid_grant")
        ));
        assert!(oauth_refresh_requires_reauth(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            Some("refresh_token_reused")
        ));
        assert!(!oauth_refresh_requires_reauth(
            reqwest::StatusCode::BAD_REQUEST,
            Some("invalid_request")
        ));
        assert!(!oauth_refresh_requires_reauth(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            None
        ));
    }

    #[test]
    fn oauth_refresh_timing_matches_codex() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-06T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let fresh = serde_json::json!({
            "last_refresh": "2026-08-01T00:00:00Z",
            "tokens": {
                "access_token": test_jwt_with_expiration("account-a", now.timestamp() + 301)
            }
        });
        let expiring = serde_json::json!({
            "last_refresh": "2026-09-06T12:00:00Z",
            "tokens": {
                "access_token": test_jwt_with_expiration("account-a", now.timestamp() + 300)
            }
        });
        let old_without_expiration = serde_json::json!({
            "last_refresh": "2026-08-29T11:59:59Z",
            "tokens": {"access_token": "opaque"}
        });
        let fresh_without_expiration = serde_json::json!({
            "last_refresh": "2026-08-29T12:00:01Z",
            "tokens": {"access_token": "opaque"}
        });

        assert!(!codex_auth_requires_refresh(&fresh, now));
        assert!(codex_auth_requires_refresh(&expiring, now));
        assert!(codex_auth_requires_refresh(&old_without_expiration, now));
        assert!(!codex_auth_requires_refresh(&fresh_without_expiration, now));
        assert!(!codex_auth_requires_refresh(
            &serde_json::json!({"tokens": {"access_token": "opaque"}}),
            now
        ));
    }

    #[test]
    fn oauth_refresh_request_matches_codex_protocol() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("POST /token HTTP/1.1"));
            assert!(request.contains("\"client_id\":\"app_EMoamEEZ73f0CkXaXp7hrann\""));
            assert!(request.contains("\"grant_type\":\"refresh_token\""));
            assert!(request.contains("\"refresh_token\":\"old-refresh\""));
            let body = format!(
                "{{\"access_token\":{},\"refresh_token\":\"new-refresh\"}}",
                serde_json::to_string(&test_jwt("account-a")).unwrap()
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let document = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "account_id": "account-a",
                "access_token": test_jwt("account-a"),
                "refresh_token": "old-refresh"
            }
        });
        let client = reqwest::blocking::Client::builder().build().unwrap();

        let refreshed =
            request_codex_oauth_refresh(&client, &format!("http://{address}/token"), &document)
                .unwrap();

        server.join().unwrap();
        assert_eq!(refreshed["tokens"]["refresh_token"], "new-refresh");
        validate_codex_token_accounts(&refreshed).unwrap();
    }

    #[test]
    fn failed_oauth_preflight_keeps_the_saved_credential_unchanged() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request).unwrap();
            let body = r#"{"error":{"code":"refresh_token_reused"}}"#;
            write!(
                stream,
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let original = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "account_id": "account-a",
                "access_token": test_jwt("account-a"),
                "refresh_token": "old-refresh",
                "id_token": test_jwt("account-a")
            }
        });
        let reference = SecretRef {
            service: "com.mix.test".into(),
            account: "account-a".into(),
        };
        let profile = Profile {
            account_fingerprint: CodexAdapter::fingerprint(&original),
            secret_files: BTreeMap::from([("auth.json".into(), reference.clone())]),
            ..Profile::default()
        };
        let vault = crate::vault::MemoryVault::default();
        vault
            .set(&reference, &serde_json::to_vec(&original).unwrap())
            .unwrap();
        let client = reqwest::blocking::Client::builder().build().unwrap();

        let error = refresh_managed_codex_credential_with(
            &profile,
            &vault,
            &client,
            &format!("http://{address}/token"),
        )
        .unwrap_err();

        server.join().unwrap();
        assert_eq!(error.code, ErrorCode::MixAccountReauthRequired);
        assert_eq!(error.details["oauth_code"], "refresh_token_reused");
        assert_eq!(
            serde_json::from_slice::<Value>(&vault.get(&reference).unwrap()).unwrap(),
            original
        );
    }

    #[test]
    fn global_account_lifecycle_is_declared_by_the_adapter() {
        let root = tempfile::tempdir().unwrap();
        let codex = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: root.path().join("codex"),
            active_profile: Some("stale-selection".into()),
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };
        let claude = ClientConfig {
            adapter: AdapterKind::Claude,
            live_dir: root.path().join("claude"),
            active_profile: Some("work".into()),
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };

        let codex_adapter = CodexAdapter;
        assert!(codex_adapter.global_account_projection().is_some());
        assert_eq!(codex_adapter.observed_profile(&codex), None);

        let claude_adapter = ClaudeAdapter;
        assert!(claude_adapter.global_account_projection().is_none());
        assert_eq!(
            claude_adapter.observed_profile(&claude).as_deref(),
            Some("work")
        );
        assert!(codex_adapter.capabilities(&codex, true)["project.open_native"].available);
        assert!(!codex_adapter.capabilities(&codex, false)["project.open_native"].available);
        assert!(!claude_adapter.capabilities(&claude, true)["project.open_native"].available);
    }

    #[test]
    fn registry_descriptors_are_unique_and_self_consistent() {
        let registry = AdapterRegistry::default();
        let descriptors = registry.all().map(Adapter::descriptor).collect::<Vec<_>>();
        let kinds = descriptors
            .iter()
            .map(|descriptor| descriptor.kind)
            .collect::<HashSet<_>>();
        let names = descriptors
            .iter()
            .map(|descriptor| descriptor.name)
            .collect::<HashSet<_>>();

        assert_eq!(kinds.len(), descriptors.len());
        assert_eq!(names.len(), descriptors.len());
        for descriptor in descriptors {
            let adapter = registry.get(descriptor.kind);
            assert_eq!(adapter.descriptor().name, descriptor.name);
            assert!(!descriptor.default_command.is_empty());
            assert!(!descriptor.profile_prefix.is_empty());
            assert_eq!(
                adapter.global_account_projection().is_some(),
                descriptor.profile_category == ProfileCategory::Account
            );
        }
    }

    #[test]
    fn codex_provider_route_disambiguates_a_shared_credential() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.toml");
        let second = root.path().join("second.toml");
        fs::write(
            &first,
            "model_provider = \"gateway\"\n[model_providers.gateway]\nbase_url = \"https://first.example\"\n",
        )
        .unwrap();
        fs::write(
            &second,
            "model_provider = \"gateway\"\n[model_providers.gateway]\nbase_url = \"https://second.example\"\n",
        )
        .unwrap();
        let fingerprint = "shared-credential";
        let mut client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: root.path().to_path_buf(),
            active_profile: Some("first".into()),
            profiles: BTreeMap::from([
                (
                    "first".into(),
                    Profile {
                        files: BTreeMap::from([("config.toml".into(), first)]),
                        account_fingerprint: Some(fingerprint.into()),
                        ..Profile::default()
                    },
                ),
                (
                    "second".into(),
                    Profile {
                        files: BTreeMap::from([("config.toml".into(), second)]),
                        account_fingerprint: Some(fingerprint.into()),
                        ..Profile::default()
                    },
                ),
            ]),
            run: crate::RunSpec::default(),
        };
        let second_route = toml::from_str::<toml::Table>(
            "model_provider = \"gateway\"\n[model_providers.gateway]\nbase_url = \"https://second.example\"\n",
        )
        .unwrap();

        assert_eq!(
            matching_codex_profile(&client, fingerprint, &second_route).as_deref(),
            Some("second")
        );

        let unknown_route = toml::from_str::<toml::Table>(
            "model_provider = \"gateway\"\n[model_providers.gateway]\nbase_url = \"https://third.example\"\n",
        )
        .unwrap();
        assert_eq!(
            matching_codex_profile(&client, fingerprint, &unknown_route),
            None
        );
        client.profiles.remove("second");
        assert_eq!(
            matching_codex_profile(&client, fingerprint, &unknown_route),
            None
        );
        client.active_profile = None;
        assert_eq!(
            matching_codex_profile(&client, fingerprint, &unknown_route),
            None
        );
    }

    #[test]
    fn an_openai_route_with_a_custom_endpoint_is_not_official() {
        let built_in = toml::Table::new();
        let overridden = toml::from_str::<toml::Table>(
            "model_provider = \"openai\"\nopenai_base_url = \"https://gateway.example\"\n",
        )
        .unwrap();

        assert!(provider_from_toml(&built_in).official);
        let provider = provider_from_toml(&overridden);
        assert!(!provider.official);
        assert_eq!(provider.id, "openai");
    }

    #[test]
    fn diagnostic_endpoint_host_follows_the_effective_provider_route() {
        let hair_free = toml::from_str::<toml::Table>(
            "model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"https://HairFree.example/gateway/v1\"\n",
        )
        .unwrap();
        assert_eq!(
            codex_endpoint_host(&hair_free).as_deref(),
            Some("hairfree.example")
        );

        let official = toml::Table::new();
        assert_eq!(
            codex_endpoint_host(&official).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn codex_history_rebind_uses_the_exact_provider_id_and_is_reversible() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(live.join("sessions/2026/09/06")).unwrap();
        fs::write(live.join("config.toml"), "model_provider = \"openai\"\n").unwrap();
        let rollout = live.join("sessions/2026/09/06/rollout.jsonl");
        let original = b"{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\"}}\n{\"type\":\"event_msg\",\"payload\":{\"message\":\"preserved\"}}\n";
        fs::write(&rollout, original).unwrap();

        let database = live.join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (rollout_path TEXT NOT NULL, model_provider TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (rollout_path, model_provider) VALUES (?1, ?2)",
                rusqlite::params![rollout.to_string_lossy(), "openai"],
            )
            .unwrap();
        drop(connection);
        fs::write(live.join("state_5.sqlite-wal"), b"sidecar").unwrap();

        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: live.clone(),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };
        let target_config = root.path().join("enterprise.toml");
        fs::write(
            &target_config,
            "model_provider = \"enterprise-gateway\"\n[model_providers.enterprise-gateway]\nbase_url = \"https://gateway.example/v1\"\n",
        )
        .unwrap();
        let target = Profile {
            files: BTreeMap::from([("config.toml".into(), target_config)]),
            ..Profile::default()
        };
        let projection = CodexAdapter.switch_projection(&client, &target).unwrap();
        assert!(projection.files.contains_key("config.toml"));
        assert!(projection.files.contains_key("state_5.sqlite"));
        assert_eq!(projection.rewrites.len(), 1);
        assert_eq!(projection.rewrites[0].from, b"\"openai\"");
        assert_eq!(projection.rewrites[0].to, b"\"enterprise-gateway\"");

        let vault = crate::vault::MemoryVault::default();
        let journal = crate::transaction::SwitchJournal::prepare(
            root.path(),
            crate::transaction::SwitchPlan {
                app: "codex",
                live_dir: &live,
                from_name: None,
                from: None,
                to_name: "target",
                to: &target,
                file_overrides: projection.files,
                file_removals: projection.removals,
                file_rewrites: projection.rewrites,
                restart_required: false,
            },
            &vault,
        )
        .unwrap();
        journal.apply(&vault).unwrap();
        journal.apply(&vault).unwrap();

        let connection = Connection::open(&database).unwrap();
        let provider: String = connection
            .query_row("SELECT model_provider FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(provider, "enterprise-gateway");
        assert!(!live.join("state_5.sqlite-wal").exists());
        let switched = fs::read(&rollout).unwrap();
        assert!(String::from_utf8(switched.clone())
            .unwrap()
            .contains("enterprise-gateway"));
        assert!(switched
            .ends_with(b"{\"type\":\"event_msg\",\"payload\":{\"message\":\"preserved\"}}\n"));

        journal.restore(root.path(), &vault).unwrap();
        journal.restore(root.path(), &vault).unwrap();
        let connection = Connection::open(&database).unwrap();
        let provider: String = connection
            .query_row("SELECT model_provider FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(provider, "openai");
        assert_eq!(fs::read(&rollout).unwrap(), original);
        assert_eq!(
            fs::read(live.join("state_5.sqlite-wal")).unwrap(),
            b"sidecar"
        );
        journal.cleanup_after_restore(root.path(), &vault).unwrap();
    }

    #[test]
    fn codex_history_sidecars_are_kept_when_database_is_already_consistent() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        let database = live.join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (rollout_path TEXT NOT NULL, model_provider TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (rollout_path, model_provider) VALUES ('', 'openai')",
                [],
            )
            .unwrap();
        drop(connection);
        let sidecar = live.join("state_5.sqlite-wal");
        fs::write(&sidecar, b"uncheckpointed data").unwrap();

        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: live,
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };
        let overrides = codex_history_database_overrides(&client, "openai").unwrap();
        assert!(overrides.is_empty());
        assert!(
            codex_history_sidecar_removals(&client, &overrides.keys().cloned().collect(),)
                .is_empty()
        );
        assert!(sidecar.exists());
    }

    #[test]
    fn codex_history_ignores_a_missing_referenced_rollout() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        let database = live.join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (rollout_path TEXT NOT NULL, model_provider TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (rollout_path, model_provider) VALUES (?1, 'custom')",
                [live.join("missing.jsonl").to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(connection);

        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: live,
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };
        let rewrites = codex_history_rollout_rewrites(&client, "openai").unwrap();
        assert!(rewrites.is_empty());
    }

    #[test]
    fn codex_history_skips_unrecognized_schema_in_both_supported_locations() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(live.join("sqlite")).unwrap();

        let legacy = Connection::open(live.join("state_5.sqlite")).unwrap();
        legacy
            .execute_batch("CREATE TABLE sessions (id TEXT NOT NULL);")
            .unwrap();
        drop(legacy);

        let current = Connection::open(live.join("sqlite/state_5.sqlite")).unwrap();
        current
            .execute_batch("CREATE TABLE threads (rollout_path TEXT NOT NULL);")
            .unwrap();
        drop(current);

        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: live,
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };

        assert!(codex_history_database_overrides(&client, "openai")
            .unwrap()
            .is_empty());
        assert!(codex_history_rollout_rewrites(&client, "openai")
            .unwrap()
            .is_empty());
        verify_codex_history_projection(&client, "openai").unwrap();
    }

    #[test]
    fn codex_history_skips_a_corrupt_database_without_touching_it() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::create_dir_all(&live).unwrap();
        let database = live.join("state_5.sqlite");
        let original = b"not a sqlite database\n";
        fs::write(&database, original).unwrap();

        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: live,
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };

        assert!(codex_history_database_overrides(&client, "openai")
            .unwrap()
            .is_empty());
        assert!(codex_history_rollout_rewrites(&client, "openai")
            .unwrap()
            .is_empty());
        verify_codex_history_projection(&client, "openai").unwrap();
        assert_eq!(fs::read(&database).unwrap(), original);
    }

    #[test]
    fn codex_history_skips_ambiguous_provider_offsets() {
        let line = br#"{"type":"session_meta","payload":{"model_provider":"custom","nested":{"model_provider":"custom"}}}"#;
        assert_eq!(model_provider_token_offset(line, "custom"), None);
    }

    #[test]
    fn codex_desktop_switch_starts_and_foregrounds_the_live_home() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("ChatGPT.app/Contents/MacOS/ChatGPT");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"test executable").unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: root.path().join(".codex"),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };

        let process = codex_desktop_process_for(&client, root.path(), executable.clone()).unwrap();
        let app = root.path().join("ChatGPT.app").display().to_string();
        assert_eq!(process.managed_executables, [executable]);
        assert_eq!(
            process.ready_executable,
            Some(root.path().join("ChatGPT.app/Contents/Resources/codex"))
        );
        assert_eq!(process.launch, ["/usr/bin/open", "-a", &app]);
        assert_eq!(process.activate, ["/usr/bin/open", "-a", &app]);
    }

    #[test]
    fn codex_desktop_switch_never_controls_an_isolated_home() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("ChatGPT.app/Contents/MacOS/ChatGPT");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"test executable").unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: root.path().join("runtime/account"),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };

        assert!(codex_desktop_process_for(&client, root.path(), executable).is_none());
    }

    #[test]
    fn fingerprints_account_without_exposing_tokens() {
        let value =
            serde_json::json!({"tokens": {"account_id": "account-a", "access_token": "secret"}});
        let fingerprint = CodexAdapter::fingerprint(&value).unwrap();
        assert_eq!(fingerprint.len(), 64);
        assert!(!fingerprint.contains("account-a"));
        assert!(!fingerprint.contains("secret"));
    }

    #[test]
    fn claude_import_rejects_auth_material_before_returning_payload() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("settings.json"),
            r#"{"theme":"dark","env":{"ANTHROPIC_API_KEY":"sk-secret"}}"#,
        )
        .unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Claude,
            live_dir: root.path().to_path_buf(),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec {
                command: vec!["claude".into()],
            },
        };

        let error = ClaudeAdapter
            .capture_payload(&client, "settings.json")
            .expect_err("authentication material must be rejected");
        assert_eq!(error.code, ErrorCode::MixSensitiveDataRejected);
        assert!(!error.to_string().contains("sk-secret"));
    }

    #[test]
    fn claude_import_preserves_non_sensitive_settings_only() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("settings.json"),
            r#"{"theme":"dark","env":{"ANTHROPIC_BASE_URL":"https://gateway.example"},"permissions":{"allow":["Bash"]}}"#,
        )
        .unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Claude,
            live_dir: root.path().to_path_buf(),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec {
                command: vec!["claude".into()],
            },
        };

        let payload = ClaudeAdapter
            .capture_payload(&client, "settings.json")
            .expect("ordinary settings should import");
        let document: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(document["theme"], "dark");
        assert_eq!(
            document["env"]["ANTHROPIC_BASE_URL"],
            "https://gateway.example"
        );
        assert_eq!(document["permissions"]["allow"][0], "Bash");
    }

    #[test]
    fn claude_profile_is_revalidated_after_its_saved_settings_change() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("settings.json");
        fs::write(&settings, r#"{"theme":"dark"}"#).unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Claude,
            live_dir: root.path().to_path_buf(),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };
        let profile = Profile {
            label: "Team".into(),
            files: BTreeMap::from([("settings.json".into(), settings.clone())]),
            ..Profile::default()
        };
        ClaudeAdapter.validate_profile(&client, &profile).unwrap();

        fs::write(
            settings,
            r#"{"env":{"ANTHROPIC_API_KEY":"must-not-project"}}"#,
        )
        .unwrap();

        let error = ClaudeAdapter
            .validate_profile(&client, &profile)
            .expect_err("tampered settings must fail before projection");
        assert_eq!(error.code, ErrorCode::MixSensitiveDataRejected);
        assert!(!error.to_string().contains("must-not-project"));
    }

    #[test]
    fn known_adapters_cannot_manage_native_session_paths() {
        let client = ClientConfig {
            adapter: AdapterKind::Claude,
            live_dir: PathBuf::from("/tmp/claude"),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };
        let claude = Profile {
            label: "unsafe".into(),
            files: BTreeMap::from([(
                "projects/example/session.jsonl".into(),
                PathBuf::from("/tmp/session.jsonl"),
            )]),
            ..Profile::default()
        };
        let codex = Profile {
            label: "unsafe".into(),
            files: BTreeMap::from([(
                "sessions/2026/session.jsonl".into(),
                PathBuf::from("/tmp/session.jsonl"),
            )]),
            ..Profile::default()
        };

        assert_eq!(
            ClaudeAdapter
                .validate_profile(&client, &claude)
                .unwrap_err()
                .code,
            ErrorCode::MixValidationError
        );
        assert_eq!(
            CodexAdapter
                .validate_profile(&client, &codex)
                .unwrap_err()
                .code,
            ErrorCode::MixValidationError
        );
    }

    #[test]
    fn claude_credentials_must_use_vault_backed_environment_references() {
        let client = ClientConfig {
            adapter: AdapterKind::Claude,
            live_dir: PathBuf::from("/tmp/claude"),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };
        let profile = Profile {
            label: "unsafe".into(),
            secret_files: BTreeMap::from([(
                "auth.json".into(),
                crate::SecretRef {
                    service: "example".into(),
                    account: "claude".into(),
                },
            )]),
            ..Profile::default()
        };

        assert_eq!(
            ClaudeAdapter
                .validate_profile(&client, &profile)
                .unwrap_err()
                .code,
            ErrorCode::MixValidationError
        );
    }

    #[test]
    fn codex_profile_validation_rejects_stored_literal_credentials() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        fs::write(
            &config,
            "cli_auth_credentials_store = \"file\"\n[model_providers.gateway]\nexperimental_bearer_token = \"must-not-escape\"\n",
        )
        .unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: root.path().to_path_buf(),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec::default(),
        };
        let profile = Profile {
            label: "unsafe".into(),
            files: BTreeMap::from([("config.toml".into(), config)]),
            secret_files: BTreeMap::from([(
                "auth.json".into(),
                crate::SecretRef {
                    service: "com.mix.test".into(),
                    account: "unsafe".into(),
                },
            )]),
            account_fingerprint: Some("fingerprint".into()),
            ..Profile::default()
        };

        let error = CodexAdapter
            .validate_profile(&client, &profile)
            .expect_err("stored plaintext credentials must fail before a switch");
        assert_eq!(error.code, ErrorCode::MixSensitiveDataRejected);
        assert!(!error.to_string().contains("must-not-escape"));
    }

    #[test]
    fn codex_config_allows_environment_variable_references() {
        let document = toml::from_str::<toml::Table>(
            "[model_providers.gateway]\nenv_key = \"GATEWAY_API_KEY\"\n[model_providers.gateway.env_http_headers]\nX-Token = \"GATEWAY_HEADER\"\n[mcp_servers.company]\nbearer_token_env_var = \"MCP_TOKEN\"\nenv_vars = [\"MCP_EXTRA\"]\n",
        )
        .unwrap();

        validate_codex_config(&document).unwrap();
    }

    #[test]
    fn codex_config_preserves_non_secret_literal_settings() {
        let document = toml::from_str::<toml::Table>(
            "[model_providers.gateway.http_headers]\nOpenAI-Organization = \"org-example\"\n[model_providers.gateway.query_params]\napi-version = \"2026-01-01\"\n[mcp_servers.node.env]\nCODEX_CLI_PATH = \"/Applications/Codex\"\nFEATURE_ENABLED = \"true\"\n[shell_environment_policy.set]\nRUST_LOG = \"info\"\n",
        )
        .unwrap();

        validate_codex_config(&document).unwrap();
    }

    #[test]
    fn codex_config_rejects_secrets_disguised_as_environment_references() {
        for config in [
            "[model_providers.gateway]\nenv_key = \"sk-not-a-name\"\n",
            "[model_providers.gateway.env_http_headers]\nX-Token = \"Bearer secret\"\n",
            "[mcp_servers.company]\nbearer_token_env_var = \"secret value\"\n",
            "[mcp_servers.company]\nenv_vars = [\"SAFE_NAME\", \"secret-value\"]\n",
        ] {
            let document = toml::from_str::<toml::Table>(config).unwrap();
            let error = validate_codex_config(&document)
                .expect_err("environment references must contain names, never values");
            assert_eq!(error.code, ErrorCode::MixSensitiveDataRejected);
            assert!(!error.to_string().contains("sk-not-a-name"));
            assert!(!error.to_string().contains("Bearer secret"));
            assert!(!error.to_string().contains("secret value"));
            assert!(!error.to_string().contains("secret-value"));
        }
    }

    #[test]
    fn session_search_handles_unicode_case_expansion() {
        assert!(contains_case_insensitive(
            "Archived Straße customer migration",
            "strasse customer",
            "STRASSE CUSTOMER",
        ));
    }

    #[test]
    fn native_ids_cannot_become_cli_options_or_paths() {
        for value in [
            "safe-id",
            "0199cd2d-2f7a-77f2-82ee-589fbdd1bca4",
            "session_1",
        ] {
            assert!(validate_native_id(value).is_ok(), "{value}");
        }
        for value in [
            "bad\ncommand",
            "--help",
            "../session",
            "session/name",
            "会话",
        ] {
            assert!(validate_native_id(value).is_err(), "{value}");
        }
    }

    #[test]
    fn codex_resume_uses_the_current_route_for_every_provider() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: root.path().to_path_buf(),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec {
                command: vec!["/usr/bin/true".into()],
            },
        };

        for (source, expected) in [
            ("model = \"official-model\"\n", "openai"),
            ("model_provider = \"company-a\"\n", "company-a"),
            ("model_provider = \"company-b\"\n", "company-b"),
        ] {
            fs::write(&config, source).unwrap();
            let argv = CodexAdapter.resume_argv(&client, "safe-session").unwrap();
            let provider_override = format!("model_provider={expected:?}");

            assert_eq!(
                argv,
                vec![
                    "/usr/bin/true".to_owned(),
                    "-c".to_owned(),
                    provider_override,
                    "resume".to_owned(),
                    "safe-session".to_owned(),
                ]
            );
            assert_eq!(fs::read_to_string(&config).unwrap(), source);
        }
    }

    #[test]
    fn codex_resume_defaults_only_when_config_is_absent() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: root.path().to_path_buf(),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec {
                command: vec!["/usr/bin/true".into()],
            },
        };

        let argv = CodexAdapter.resume_argv(&client, "safe-session").unwrap();
        assert_eq!(argv[2], "model_provider=\"openai\"");

        fs::write(&config, "model_provider = [\n").unwrap();
        let error = CodexAdapter
            .resume_argv(&client, "safe-session")
            .expect_err("an invalid route must not fall back to OpenAI");
        assert_eq!(error.code, ErrorCode::MixValidationError);
    }

    #[test]
    fn codex_resume_reads_the_provider_from_an_isolated_runtime() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        let runtime = root.path().join("runtime");
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(live.join("config.toml"), "model_provider = \"live\"\n").unwrap();
        fs::write(
            runtime.join("config.toml"),
            "model_provider = \"hair-free\"\n",
        )
        .unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: runtime,
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec {
                command: vec!["/usr/bin/true".into()],
            },
        };

        let argv = CodexAdapter.resume_argv(&client, "safe-session").unwrap();

        assert_eq!(argv[2], "model_provider=\"hair-free\"");
    }

    #[test]
    fn codex_sessions_skip_a_damaged_sibling() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions/2026/09/02");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("broken.jsonl"), b"not-json\n").unwrap();
        fs::write(
            sessions.join("good.jsonl"),
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"good\",\"cwd\":\"/tmp/project\",\"model_provider\":\"openai\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Build Mix\"}}\n",
        )
        .unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: root.path().to_path_buf(),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec {
                command: vec!["codex".into()],
            },
        };
        let rows = rollout_sessions("codex", &client, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "good");
        assert_eq!(rows[0].title.as_deref(), Some("Build Mix"));
    }

    #[test]
    fn codex_sessions_use_native_event_time_instead_of_file_touch_time() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions/2026/09/02");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("session.jsonl"),
            format!(
                "{{\"timestamp\":\"2020-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"native-time\",\"cwd\":{}}}}}\n{{\"timestamp\":\"2020-01-02T03:05:06Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Old task\"}}}}\n",
                serde_json::to_string(&root.path().display().to_string()).unwrap()
            ),
        )
        .unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: root.path().to_path_buf(),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec {
                command: vec!["codex".into()],
            },
        };

        let rows = rollout_sessions("codex", &client, None).unwrap();

        assert_eq!(
            rows[0].updated_at.as_deref(),
            Some("2020-01-02T03:05:06+00:00")
        );
    }

    #[test]
    fn native_timestamp_scan_recovers_the_last_complete_tail_event() {
        let root = tempfile::tempdir().unwrap();
        let transcript = root.path().join("session.jsonl");
        let mut payload = vec![b'x'; SESSION_META_BYTES as usize + 1];
        payload.extend_from_slice(
            b"\n{\"timestamp\":\"2024-05-06T07:08:09Z\",\"type\":\"event_msg\"}\n",
        );
        fs::write(&transcript, payload).unwrap();

        let (timestamp, consumed) =
            latest_native_timestamp(&transcript, SESSION_META_BYTES as usize);

        assert_eq!(timestamp.as_deref(), Some("2024-05-06T07:08:09+00:00"));
        assert_eq!(consumed, SESSION_META_BYTES as usize);
    }

    #[test]
    fn codex_title_scan_skips_events_without_payloads() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions/2026/09/02");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("good.jsonl"),
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"good\",\"cwd\":\"/tmp/project\"}}\n{\"type\":\"notice\"}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Real title\"}}\n",
        )
        .unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: root.path().to_path_buf(),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec {
                command: vec!["codex".into()],
            },
        };
        let rows = rollout_sessions("codex", &client, None).unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("Real title"));
    }

    #[test]
    fn codex_title_scan_ignores_injected_runtime_context() {
        let input = b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"<environment_context>\\n<cwd>/tmp/private</cwd>\\n</environment_context>\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Fix the account switch\"}}\n";
        let mut reader = BufReader::new(input.as_slice());

        let (title, _) = scan_codex_title(&mut reader, input.len());

        assert_eq!(title.as_deref(), Some("Fix the account switch"));
    }

    #[test]
    fn codex_title_scan_extracts_the_request_after_attachment_context() {
        let input = b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"# Files mentioned by the user:\\n\\n/tmp/screenshot.png\\n\\n## My request:\\nWhy did switching fail?\"}}\n";
        let mut reader = BufReader::new(input.as_slice());

        let (title, _) = scan_codex_title(&mut reader, input.len());

        assert_eq!(title.as_deref(), Some("Why did switching fail?"));
    }

    #[test]
    fn codex_title_scan_does_not_treat_a_user_written_marker_as_context() {
        let input = b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Explain why ## My request: appears in this question\"}}\n";
        let mut reader = BufReader::new(input.as_slice());

        let (title, _) = scan_codex_title(&mut reader, input.len());

        assert_eq!(
            title.as_deref(),
            Some("Explain why ## My request: appears in this question")
        );
    }

    #[test]
    fn codex_catalog_budget_tracks_bytes_read_not_rollout_file_size() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions/2026/09/02");
        fs::create_dir_all(&sessions).unwrap();
        let large = sessions.join("large.jsonl");
        fs::write(
            &large,
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"large\",\"cwd\":\"/tmp\"}}\n",
        )
        .unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&large)
            .unwrap()
            .set_len(SESSION_MAX_TOTAL_BYTES + 1)
            .unwrap();
        fs::write(
            sessions.join("small.jsonl"),
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"small\",\"cwd\":\"/tmp\"}}\n",
        )
        .unwrap();
        let client = ClientConfig {
            adapter: AdapterKind::Codex,
            live_dir: root.path().to_path_buf(),
            active_profile: None,
            profiles: BTreeMap::new(),
            run: crate::RunSpec {
                command: vec!["codex".into()],
            },
        };

        let rows = rollout_sessions("codex", &client, None).unwrap();
        assert_eq!(
            rows.iter()
                .map(|session| session.id.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["large", "small"]),
        );
    }
}
