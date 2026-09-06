export type Language = "zh" | "en";
export type Theme = "system" | "light" | "dark";
export type View = "workspaces" | "environments" | "sessions" | "activity" | "settings";

export type Profile = {
  name: string;
  label: string;
  display_label?: string;
  managed_account?: boolean;
  has_credentials: boolean;
  auth_strategy: "configuration" | "interactive" | "local-file";
  file_count: number;
  secret_count: number;
  identity?: AccountIdentity | null;
  provider?: { id: string; name: string; official: boolean } | null;
};

export type AccountIdentity = {
  email?: string;
  name?: string;
  plan?: string;
  suffix?: string;
  suggested_label?: string;
};

export type AccountSwitchCapability = {
  available: boolean;
  status: "ready" | "file_auth_required" | "sign_in_required" | "invalid_config" | "invalid_auth" | "identity_unavailable" | "unsupported";
  storage?: "file" | "keyring" | "auto" | "ephemeral" | "default" | "invalid" | null;
  identity?: AccountIdentity;
  current_profile?: string | null;
  provider?: { id: string; name: string; official: boolean } | null;
};

export type Client = {
  name: string;
  adapter: string;
  profile_category: "account" | "environment";
  live_dir: string;
  active: string | null;
  configured_active?: string | null;
  status: "ready" | "warning" | "setup_required";
  command?: string | null;
  command_found: boolean;
  issues: ClientIssue[];
  account_switch?: AccountSwitchCapability;
  capabilities: Record<string, ClientCapability>;
  import_files: string[];
  profiles: Profile[];
};

export type ClientIssue = {
  code: "interrupted_switch" | "cleanup_pending" | "no_profiles" | "run_command_missing" | "run_command_unavailable" | "unmanaged_account" | "unsupported_account_profile" | "credential_store_unavailable";
};

export type ClientCapability = {
  available: boolean;
  support: "native_api" | "native_cli" | "native_files_readonly" | "configured" | "unsupported";
  authority: "read_only" | "launch" | "configuration_write" | "credential_projection";
  fallback?: string | null;
  reason?: string | null;
  completeness?: "complete" | "eventually_consistent" | "best_effort";
  explicit_only?: boolean;
};

export type Workspace = {
  id: string;
  name: string;
  path: string;
  bindings: Record<string, string>;
  binding_conflicts?: Record<string, string[]>;
  exists: boolean;
};

export type Activity = {
  id?: string;
  kind: string;
  app?: string;
  profile?: string;
  from_profile?: string;
  to_profile?: string;
  workspace?: string;
  cwd?: string;
  runtime_dir?: string;
  files?: string[];
  secret_files?: string[];
  at?: string;
  backup?: string;
};

export type Session = {
  id: string;
  title?: string;
  app: string;
  profile?: string;
  provider?: string;
  workspace?: string;
  /** Display grouping inferred independently from safe-resume verification. */
  project_id?: string;
  project_name?: string;
  project_path?: string;
  project_source?: "registered" | "runtime" | "directory";
  project_registered?: boolean;
  project_exists?: boolean;
  project_session_count?: number;
  cwd?: string;
  transcript?: string;
  catalog_source?: "native_api" | "native_cli" | "native_files_readonly" | "runtime";
  native_command?: string;
  resume_id?: string;
  state_dir: string;
  updated_at?: string;
  recoverability?: "A" | "B";
  recovery_reason?: "verified" | "unsupported_command" | "profile_missing" | "runtime_changed" | "working_directory_missing" | "working_directory_changed" | "transcript_missing" | "transcript_outside_runtime";
};

export type DiscoveredClient = {
  adapter: string;
  profile_category: "account" | "environment";
  name: string;
  label: string;
  installed: boolean;
  executable?: string | null;
  desktop_executable?: string | null;
  live_dir: string;
  config_exists: boolean;
  sessions_detected: boolean;
  import_files: string[];
  configured: boolean;
  account_switch?: AccountSwitchCapability;
};

export type CredentialStoreCapability = {
  supported: boolean;
  available: boolean;
  backend?: string | null;
  kind?: "local-files" | null;
  reason?: "permission_denied" | "unavailable" | null;
};

export type Snapshot = {
  apps: Client[];
  workspaces: Workspace[];
  activity: Activity[];
  needs_setup: boolean;
  health: {
    status: "healthy" | "attention";
    issues: { app: string; issue: ClientIssue }[];
    local_only: boolean;
  };
  security: {
    credential_store: CredentialStoreCapability;
  };
  recovery: {
    interrupted_switch: {
      required: boolean;
      status?: "pending" | "cleanup_pending" | "invalid";
      id?: string | null;
      app?: string | null;
      from_profile?: string | null;
      to_profile?: string | null;
      created_at?: string | null;
      error?: string;
    };
  };
};

export type DialogState =
  | { kind: "client" }
  | { kind: "profile"; app?: string }
  | { kind: "profile-edit"; app: string; profile: string }
  | { kind: "profile-delete"; app: string; profile: string }
  | { kind: "workspace"; workspace?: string }
  | { kind: "workspace-delete"; workspace: string }
  | { kind: "switch-back"; activity: Activity }
  | null;
