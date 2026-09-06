use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub(crate) fn is_environment_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| {
        (first == '_' || first.is_ascii_alphabetic())
            && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
    })
}

pub const CONFIG_VERSION: u32 = 4;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterKind {
    Codex,
    Claude,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCategory {
    Account,
    Environment,
}

impl AdapterKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

impl std::str::FromStr for AdapterKind {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" | "claude-code" => Ok(Self::Claude),
            _ => Err(format!("unknown client type: {value}")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub root: PathBuf,
    #[serde(default)]
    pub apps: BTreeMap<String, ClientConfig>,
    #[serde(default)]
    pub workspace_bindings: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub workspace_meta: BTreeMap<String, WorkspaceMeta>,
    #[serde(default)]
    pub pending_cleanup: PendingCleanup,
    #[serde(default)]
    pub meta: ConfigMeta,
}

impl Config {
    pub fn empty(root: PathBuf) -> Self {
        Self {
            version: CONFIG_VERSION,
            root,
            apps: BTreeMap::new(),
            workspace_bindings: BTreeMap::new(),
            workspace_meta: BTreeMap::new(),
            pending_cleanup: PendingCleanup::default(),
            meta: ConfigMeta {
                created_at: Some(chrono::Utc::now().to_rfc3339()),
                updated_at: None,
                revision: None,
            },
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingCleanup {
    #[serde(default)]
    pub profile_directories: Vec<PathBuf>,
    #[serde(default)]
    pub credentials: Vec<SecretRef>,
    #[serde(default)]
    pub runtime_revocations: Vec<PendingRuntimeRevocation>,
}

impl PendingCleanup {
    pub fn is_empty(&self) -> bool {
        self.profile_directories.is_empty()
            && self.credentials.is_empty()
            && self.runtime_revocations.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRuntimeRevocation {
    pub app: String,
    pub profile: String,
    pub profile_id: String,
    pub files: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigMeta {
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub adapter: AdapterKind,
    pub live_dir: PathBuf,
    #[serde(default)]
    pub active_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    pub run: RunSpec,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    #[serde(default)]
    pub command: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub label_origin: ProfileLabelOrigin,
    #[serde(default)]
    pub files: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub secret_files: BTreeMap<String, SecretRef>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretRef>,
    #[serde(default)]
    pub auth_strategy: Option<AuthStrategy>,
    #[serde(default)]
    pub account_fingerprint: Option<String>,
    #[serde(default)]
    pub account_identity: Option<AccountIdentity>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            label: String::new(),
            label_origin: ProfileLabelOrigin::Generated,
            files: BTreeMap::new(),
            secret_files: BTreeMap::new(),
            env: BTreeMap::new(),
            secrets: BTreeMap::new(),
            auth_strategy: None,
            account_fingerprint: None,
            account_identity: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileLabelOrigin {
    #[default]
    Generated,
    User,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthStrategy {
    #[default]
    Configuration,
    Interactive,
    LocalFile,
}

#[derive(Default)]
pub struct ProfileInput {
    pub files: BTreeMap<String, PathBuf>,
    pub secret_files: BTreeMap<String, SecretRef>,
    pub env: BTreeMap<String, String>,
    pub secrets: BTreeMap<String, SecretRef>,
    pub label: Option<String>,
    pub auth_strategy: Option<AuthStrategy>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    pub service: String,
    pub account: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccountIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_label: Option<String>,
}

impl AccountIdentity {
    pub fn display_label(&self, fallback: &str) -> String {
        self.email
            .as_ref()
            .or(self.name.as_ref())
            .or(self.suggested_label.as_ref())
            .cloned()
            .unwrap_or_else(|| fallback.to_owned())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMeta {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub apps: Vec<ClientSnapshot>,
    pub workspaces: Vec<WorkspaceSnapshot>,
    pub activity: Vec<Activity>,
    pub needs_setup: bool,
    pub health: Health,
    pub security: SecuritySnapshot,
    pub recovery: RecoverySnapshot,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClientSnapshot {
    pub name: String,
    pub adapter: AdapterKind,
    pub profile_category: ProfileCategory,
    pub live_dir: PathBuf,
    pub active: Option<String>,
    pub configured_active: Option<String>,
    pub status: ClientStatus,
    pub command: Option<String>,
    pub command_found: bool,
    pub issues: Vec<ClientIssue>,
    pub account_switch: AccountSwitchCapability,
    pub capabilities: BTreeMap<String, Capability>,
    pub import_files: Vec<String>,
    pub profiles: Vec<ProfileSnapshot>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientStatus {
    Ready,
    Warning,
    SetupRequired,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClientIssue {
    pub code: ClientIssueCode,
}

impl ClientIssue {
    pub fn new(code: ClientIssueCode) -> Self {
        Self { code }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientIssueCode {
    InterruptedSwitch,
    CleanupPending,
    NoProfiles,
    RunCommandMissing,
    RunCommandUnavailable,
    UnmanagedAccount,
    UnsupportedAccountProfile,
    CredentialStoreUnavailable,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProfileSnapshot {
    pub name: String,
    pub label: String,
    pub display_label: String,
    pub managed_account: bool,
    pub has_credentials: bool,
    pub auth_strategy: AuthStrategy,
    pub file_count: usize,
    pub secret_count: usize,
    pub identity: Option<AccountIdentity>,
    pub provider: Option<ProviderIdentity>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AccountSwitchCapability {
    pub available: bool,
    pub status: AccountSwitchStatus,
    pub storage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<AccountIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderIdentity>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountSwitchStatus {
    Ready,
    FileAuthRequired,
    SignInRequired,
    InvalidConfig,
    InvalidAuth,
    IdentityUnavailable,
    #[default]
    Unsupported,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderIdentity {
    pub id: String,
    pub name: String,
    pub official: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Capability {
    pub available: bool,
    pub support: CapabilitySupport,
    pub authority: CapabilityAuthority,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<CapabilitySupport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completeness: Option<Completeness>,
}

impl Capability {
    pub const fn unsupported(authority: CapabilityAuthority) -> Self {
        Self {
            available: false,
            support: CapabilitySupport::Unsupported,
            authority,
            fallback: None,
            reason: None,
            completeness: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySupport {
    NativeApi,
    NativeCli,
    NativeFilesReadonly,
    Configured,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityAuthority {
    ReadOnly,
    Launch,
    ConfigurationWrite,
    CredentialProjection,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Complete,
    EventuallyConsistent,
    BestEffort,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceSnapshot {
    pub id: String,
    pub name: String,
    pub path: String,
    pub bindings: BTreeMap<String, String>,
    pub binding_conflicts: BTreeMap<String, Vec<String>>,
    pub exists: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Activity {
    pub id: String,
    pub kind: String,
    pub at: String,
    #[serde(flatten)]
    pub data: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Health {
    pub status: HealthStatus,
    pub issues: Vec<HealthIssue>,
    pub local_only: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Attention,
}

#[derive(Clone, Debug, Serialize)]
pub struct HealthIssue {
    pub app: String,
    pub issue: ClientIssue,
}

#[derive(Clone, Debug, Serialize)]
pub struct SecuritySnapshot {
    pub credential_store: crate::vault::VaultCapability,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecoverySnapshot {
    pub interrupted_switch: InterruptedSwitch,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct InterruptedSwitch {
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Discovery {
    pub adapter: AdapterKind,
    pub profile_category: ProfileCategory,
    pub name: String,
    pub label: String,
    pub installed: bool,
    pub executable: Option<PathBuf>,
    pub desktop_executable: Option<PathBuf>,
    pub live_dir: PathBuf,
    pub config_exists: bool,
    pub sessions_detected: bool,
    pub import_files: Vec<String>,
    pub configured: bool,
    pub account_switch: AccountSwitchCapability,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Recoverability {
    #[serde(rename = "A")]
    A,
    #[serde(rename = "B")]
    B,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCatalogSource {
    NativeApi,
    NativeCli,
    NativeFilesReadonly,
    Runtime,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionProjectSource {
    Registered,
    Runtime,
    Directory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRecoveryReason {
    Verified,
    UnsupportedCommand,
    ProfileMissing,
    RuntimeChanged,
    WorkingDirectoryMissing,
    WorkingDirectoryChanged,
    TranscriptMissing,
    TranscriptOutsideRuntime,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub app: String,
    pub profile: Option<String>,
    pub provider: Option<String>,
    pub workspace: Option<String>,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub project_path: Option<String>,
    pub project_source: Option<SessionProjectSource>,
    pub project_registered: Option<bool>,
    pub project_exists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_session_count: Option<usize>,
    pub cwd: Option<String>,
    pub transcript: Option<String>,
    pub catalog_source: SessionCatalogSource,
    pub native_command: Option<String>,
    pub resume_id: String,
    pub state_dir: String,
    pub updated_at: Option<String>,
    pub recoverability: Recoverability,
    pub recovery_reason: Option<SessionRecoveryReason>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SwitchOutcome {
    pub status: String,
    pub app: String,
    pub from_profile: Option<String>,
    pub to_profile: String,
    pub backup: String,
    pub stopped_pids: Vec<u32>,
    pub restarted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}
