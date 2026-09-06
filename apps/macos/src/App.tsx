import type { FormEvent, ReactNode } from "react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import { check as checkForTauriUpdate, type Update } from "@tauri-apps/plugin-updater";
import { Icon, type IconName } from "./components/Icon";
import { ApiError, request, parseBindings, parseSecretBindings } from "./lib/api";
import { createTranslator, type CopyKey } from "./lib/i18n";
import {
  getUpdaterCapability,
  isDesktopShell,
  listenTrayActions,
  openLatestRelease,
  openThirdPartyLicenses,
  pickDirectory,
  updateTrayState,
  writeClipboardText,
  type TrayAction,
  type UpdaterCapability,
} from "./lib/native";
import type { Activity, Client, ClientIssue, CredentialStoreCapability, DialogState, DiscoveredClient, Language, Profile, Session, Snapshot, Theme, View, Workspace } from "./lib/types";

type Translate = ReturnType<typeof createTranslator>;
type Toast = { message: string; tone: "success" | "warning" | "error" | "neutral" } | null;
type OperationResult = { restart_warning?: string; activity_warning?: string; warnings?: string[]; warning?: string; status?: string };
type EnrollmentResult = { enrollment_id: string; state: "pending" | "completed" | "failed"; mode?: "repair"; profile?: string; repair_profile?: string; label?: string; error?: string; code?: string };
type LocalRepairResult = { repaired: boolean; status?: "already_ready"; source?: "active_login" };
type UpdatePhase = "idle" | "checking" | "available";
type UpdateResult = "never" | "current" | "available" | "failed";
const ENROLLMENT_POLL_ATTEMPTS = 15 * 60;
type ProjectSummary = {
  id: string;
  name: string;
  path: string;
  source: "registered" | "runtime" | "directory";
  registered: boolean;
  exists: boolean;
  bindings: Record<string, string>;
  binding_conflicts?: Record<string, string[]>;
  sessions: Session[];
  latest?: Session;
  sessionCount: number;
};
type PendingAfterSwitch =
  | { kind: "project"; app: string; profile: string; cwd: string }
  | { kind: "session"; app: string; profile: string; resumeId: string };

const EMPTY_SNAPSHOT: Snapshot = {
  apps: [], workspaces: [], activity: [], needs_setup: false,
  health: { status: "healthy", issues: [], local_only: true },
  security: {
    credential_store: { supported: false, available: false, backend: null, kind: null, reason: "unavailable" },
  },
  recovery: { interrupted_switch: { required: false } },
};

function readPreference<T extends string>(key: string, allowed: readonly T[], fallback: T): T {
  try {
    const value = window.localStorage.getItem(key) as T | null;
    return value && allowed.includes(value) ? value : fallback;
  } catch {
    return fallback;
  }
}

function formatTimestamp(value: string | undefined, language: Language): string {
  if (!value) return "";
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return value;
  return new Intl.DateTimeFormat(language === "zh" ? "zh-CN" : "en-US", { dateStyle: "medium", timeStyle: "short" }).format(parsed);
}

function compactPath(value: string | undefined): string {
  if (!value) return "—";
  const unixHome = value.match(/^\/(?:Users|home)\/[^/]+/);
  if (unixHome) return value.replace(unixHome[0], "~");
  const windowsHome = value.match(/^[A-Za-z]:\\Users\\[^\\]+/i);
  return windowsHome ? value.replace(windowsHome[0], "~") : value;
}

function pathContains(parent: string, child: string | undefined): boolean {
  if (!child) return false;
  const normalizedParent = parent.replace(/[\\/]+$/, "");
  const normalizedChild = child.replace(/[\\/]+$/, "");
  return normalizedChild === normalizedParent || normalizedChild.startsWith(`${normalizedParent}/`) || normalizedChild.startsWith(`${normalizedParent}\\`);
}

function sessionTimestamp(session: Session | undefined): number {
  if (!session) return 0;
  const value = session.updated_at ? Date.parse(session.updated_at) : 0;
  return Number.isNaN(value) ? 0 : value;
}

export function buildProjects(workspaces: Workspace[], sessions: Session[]): ProjectSummary[] {
  const projects = new Map<string, ProjectSummary>();
  const countedProjectTotals = new Map<string, Set<string>>();
  for (const workspace of workspaces) {
    projects.set(workspace.path, {
      id: workspace.path,
      name: workspace.name,
      path: workspace.path,
      source: "registered",
      registered: true,
      exists: workspace.exists,
      bindings: workspace.bindings,
      binding_conflicts: workspace.binding_conflicts,
      sessions: [],
      sessionCount: 0,
    });
  }
  for (const session of sessions) {
    const registered = workspaces
      .filter((workspace) => pathContains(workspace.path, session.cwd))
      .sort((left, right) => right.path.length - left.path.length)[0];
    const projectPath = registered?.path || session.project_path || session.cwd;
    if (!projectPath) continue;
    let project = projects.get(projectPath);
    if (!project) {
      project = {
        id: session.project_id || projectPath,
        name: session.project_name || projectPath.split(/[\\/]/).filter(Boolean).at(-1) || projectPath,
        path: projectPath,
        source: session.project_source || "directory",
        registered: Boolean(session.project_registered),
        exists: session.project_exists !== false,
        bindings: {},
        binding_conflicts: {},
        sessions: [],
        sessionCount: 0,
      };
      projects.set(projectPath, project);
    }
    project.sessions.push(session);
    if (session.project_session_count === undefined) {
      project.sessionCount += 1;
    } else {
      const sourceProject = session.project_id || session.project_path || session.cwd
        || `${session.state_dir}\0${session.app}\0${session.id}`;
      const counted = countedProjectTotals.get(projectPath) || new Set<string>();
      if (!counted.has(sourceProject)) {
        counted.add(sourceProject);
        countedProjectTotals.set(projectPath, counted);
        project.sessionCount += session.project_session_count;
      }
    }
  }
  for (const project of projects.values()) {
    project.sessions.sort((left, right) => sessionTimestamp(right) - sessionTimestamp(left));
    project.latest = project.sessions[0];
  }
  return [...projects.values()].sort((left, right) => {
    const recent = sessionTimestamp(right.latest) - sessionTimestamp(left.latest);
    if (recent) return recent;
    return left.name.localeCompare(right.name);
  });
}

function adapterName(adapter: string): string {
  return adapter === "codex" ? "Codex" : adapter === "claude" ? "Claude Code" : adapter;
}

function isAccountClient(app: Pick<Client, "profile_category"> | DiscoveredClient | undefined): boolean {
  return app?.profile_category === "account";
}

function clientCapabilityAvailable(app: Client | undefined, capability: string): boolean {
  return app?.capabilities?.[capability]?.available === true;
}

function currentAccountLabel(app: Client | undefined, t: Translate): string {
  const identity = app?.account_switch?.identity;
  const recognizableIdentity = identity?.email?.trim() || identity?.name?.trim();
  if (recognizableIdentity) return recognizableIdentity;

  const provider = app?.account_switch?.provider;
  if (provider && !provider.official) {
    return [provider.name.trim(), identity?.suffix && `ID ${identity.suffix}`]
      .filter(Boolean)
      .join(" · ") || t("customProvider");
  }
  return identity?.suggested_label || t("officialAccount");
}

function nextProfileDefaults(app: Client | undefined, t: Translate): { name: string; label: string } {
  const accountClient = isAccountClient(app);
  const prefix = accountClient ? "account" : "environment";
  const names = new Set(app?.profiles.map((profile) => profile.name) || []);
  let sequence = 1;
  while (names.has(`${prefix}-${sequence}`)) sequence += 1;
  return {
    name: `${prefix}-${sequence}`,
    label: accountClient && app?.account_switch?.identity
      ? currentAccountLabel(app, t)
      : t(accountClient ? "defaultAccountLabel" : "defaultEnvironmentLabel", {
      client: adapterName(app?.adapter || "client"),
      number: sequence,
      }),
  };
}

function credentialStoreFor(snapshot: Snapshot): CredentialStoreCapability {
  return snapshot.security.credential_store;
}

function credentialStoreUnavailableDetail(capability: CredentialStoreCapability, t: Translate): string {
  return t("credentialStorageUnavailableAction", { reason: capability.reason || "unknown" });
}

function accountSwitchCapabilityDetail(app: Client | DiscoveredClient | undefined, t: Translate): string {
  const capability = app?.account_switch;
  const hasManagedAccounts = Boolean(app && "profiles" in app && app.profiles.some((profile) => profile.managed_account));
  if (!capability) return t("codexFileAuthReady");
  if (capability.available) {
    if (hasManagedAccounts && !capability.current_profile) return t("activeAccountUnmanagedError");
    return t("codexFileAuthReady");
  }
  if (capability.status === "file_auth_required") return t("codexFileAuthRequired", { storage: capability.storage || "keyring/auto/ephemeral" });
  if (capability.status === "sign_in_required") return t(hasManagedAccounts ? "codexSignedOutSavedReady" : "codexSignInRequired");
  if (capability.status === "invalid_config") return t("codexInvalidConfig");
  if (capability.status === "invalid_auth") return t("codexInvalidAuth");
  if (capability.status === "identity_unavailable") return t("codexIdentityUnavailable");
  return t("codexAccountSwitchUnsupported");
}

function isManagedAccountProfile(profile: Profile | undefined): boolean {
  return Boolean(profile?.managed_account);
}

function sessionDisplayTitle(session: Session, t: Translate): string {
  const title = session.title?.trim();
  return title && title !== session.id ? title : t("untitledSession");
}

function isManagedAccountFlow(app: Client | undefined, targetName: string | undefined): boolean {
  if (!app || !isAccountClient(app)) return false;
  const current = app.profiles.find((profile) => profile.name === app.active);
  const target = app.profiles.find((profile) => profile.name === targetName);
  return isManagedAccountProfile(current) || isManagedAccountProfile(target);
}

function managedAccountSwitchReady(app: Client | undefined, credentialStoreAvailable: boolean): boolean {
  if (!app || !credentialStoreAvailable) return false;
  const capability = app.account_switch;
  return Boolean(capability && (capability.status === "sign_in_required" || (capability.available && capability.current_profile)));
}

function profileSwitchSupported(app: Client, profile: Profile): boolean {
  return !isAccountClient(app) || isManagedAccountProfile(profile);
}

function profileSwitchReady(app: Client, profile: Profile, credentialStoreAvailable: boolean): boolean {
  return profile.name !== app.active
    && profileSwitchSupported(app, profile)
    && (!isManagedAccountFlow(app, profile.name) || managedAccountSwitchReady(app, credentialStoreAvailable));
}

function trayProfileActionReady(app: Client, profile: Profile, credentialStoreAvailable: boolean): boolean {
  if (profile.name !== app.active) return profileSwitchReady(app, profile, credentialStoreAvailable);
  return isAccountClient(app)
    && isManagedAccountProfile(profile)
    && managedAccountSwitchReady(app, credentialStoreAvailable);
}

function profileDisplayLabel(profile: Profile | undefined): string {
  return profile?.display_label || profile?.label || "—";
}

function profileConnection(profile: Profile | undefined, t: Translate): string {
  if (!profile) return "—";
  if (profile.provider) {
    if (profile.provider.official && profile.identity?.plan) return `OpenAI · ${profile.identity.plan}`;
    return profile.provider.name;
  }
  if (profile.has_credentials || profile.auth_strategy === "local-file") return t("credentialStorage");
  return t(profile.auth_strategy === "interactive" ? "interactive" : "configuration");
}

function activityLabel(kind: string, t: Translate): string {
  const keys: Record<string, CopyKey> = {
    run: "runStarted",
    switch: "switchApplied",
    account_added: "accountSaved",
    account_synchronized: "accountSynced",
    environment_selected: "environmentSelected",
    profile_added: "profileCreated",
  };
  return t(keys[kind] || "localOperation");
}

function recoveryReasonLabel(reason: string | undefined, t: Translate): string {
  const keys: Record<string, CopyKey> = {
    unsupported_command: "recoveryReasonUnsupportedCommand",
    profile_missing: "recoveryReasonProfileMissing",
    runtime_changed: "recoveryReasonRuntimeChanged",
    working_directory_missing: "recoveryReasonWorkingDirectory",
    working_directory_changed: "recoveryReasonWorkingDirectory",
    transcript_missing: "recoveryReasonTranscript",
    transcript_outside_runtime: "recoveryReasonTranscript",
  };
  return t(keys[reason || ""] || "recoveryReasonGeneric");
}

function statusCopy(status: Client["status"], t: Translate): string {
  return t(status === "ready" ? "ready" : status === "warning" ? "warning" : "setupRequired");
}

function issueLabel(issue: ClientIssue, t: Translate): string {
  const keys: Record<string, CopyKey> = {
    interrupted_switch: "issueInterruptedSwitch",
    cleanup_pending: "issueCleanupPending",
    no_profiles: "issueNoProfiles",
    run_command_missing: "issueRunCommandMissing",
    run_command_unavailable: "issueRunCommandUnavailable",
    unmanaged_account: "issueUnmanagedAccount",
    unsupported_account_profile: "issueUnsupportedAccountProfile",
    credential_store_unavailable: "issueCredentialStoreUnavailable",
  };
  const key = keys[issue.code];
  return key ? t(key) : t("unknownError");
}

function failureMessage(error: unknown, fallback: string): string {
  if (error instanceof Error && error.message.trim()) return error.message;
  if (typeof error === "string" && error.trim()) return error;
  if (error && typeof error === "object" && "message" in error) {
    const message = String((error as { message?: unknown }).message || "").trim();
    if (message) return message;
  }
  return fallback;
}

function localizedFailureMessage(error: unknown, t: Translate, fallback: string): string {
  if (error instanceof ApiError) {
    const keys: Record<string, CopyKey> = {
      MIX_VALIDATION_ERROR: "invalidOperationError",
      MIX_REQUEST_INVALID: "invalidOperationError",
      MIX_NOT_FOUND: "resourceNotFoundError",
      MIX_CONFLICT: "resourceConflictError",
      MIX_CONFIG_INVALID: "configurationInvalidError",
      MIX_LOCAL_FAILURE: "unknownError",
      MIX_INTERNAL_ERROR: "unknownError",
      MIX_AUTH_REQUIRED: "localServiceAuthorizationExpired",
      MIX_ORIGIN_DENIED: "localServiceOffline",
      MIX_PROCESS_STILL_RUNNING: "clientStillRunningError",
      MIX_SWITCH_ROLLED_BACK: "switchRolledBackError",
      MIX_SWITCH_RECOVERY_REQUIRED: "switchRecoveryRequiredError",
      MIX_SWITCH_RECOVERY_FAILED: "switchRecoveryIncompleteError",
      MIX_SWITCH_VERIFICATION_FAILED: "switchRevertedByClientError",
      MIX_ACTIVE_ACCOUNT_UNMANAGED: "activeAccountUnmanagedError",
      MIX_ACCOUNT_REFRESH_FAILED: "accountRefreshFailed",
      MIX_ACCOUNT_REAUTH_REQUIRED: "accountReauthRequired",
      MIX_ACCOUNT_LOCAL_REPAIR_UNAVAILABLE: "accountRepairFailed",
      MIX_CODEX_FILE_AUTH_REQUIRED: "codexFileAuthRequired",
      MIX_CREDENTIAL_STORE_UNAVAILABLE: "credentialStoreUnavailableError",
      MIX_CREDENTIAL_NOT_FOUND: "accountCredentialInvalidError",
      MIX_SENSITIVE_DATA_REJECTED: "sensitiveConfigRejected",
      MIX_CREDENTIAL_TOO_LARGE: "credentialTooLargeError",
      MIX_SESSION_UNAVAILABLE: "sessionContextChangedError",
      MIX_UNSUPPORTED: "invalidOperationError",
    };
    if (error.code === "MIX_SWITCH_ROLLED_BACK") {
      const summary = t(error.details.cleanup_pending === true
        ? "switchRolledBackCleanupPending"
        : "switchRolledBackError");
      const causeCode = typeof error.details.cause_code === "string"
        ? error.details.cause_code
        : "";
      const causeKey = causeCode !== error.code ? keys[causeCode] : undefined;
      return causeKey
        ? t("switchRolledBackWithReason", { summary, reason: t(causeKey) })
        : summary;
    }
    if (keys[error.code]) return t(keys[error.code]);
  }
  return failureMessage(error, fallback);
}

export default function App() {
  const [language, setLanguage] = useState<Language>(() => readPreference("mix.language", ["zh", "en"], "zh"));
  const [theme, setTheme] = useState<Theme>(() => readPreference("mix.theme", ["system", "light", "dark"], "system"));
  const [view, setView] = useState<View>("environments");
  const [snapshot, setSnapshot] = useState<Snapshot>(EMPTY_SNAPSHOT);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [sessionsLoading, setSessionsLoading] = useState(false);
  const [sessionsHasMore, setSessionsHasMore] = useState(false);
  const [sessionQuery, setSessionQuery] = useState("");
  const [sessionClient, setSessionClient] = useState("all");
  const [sessionRecovery, setSessionRecovery] = useState("all");
  const sessionContext = useRef({ view, query: sessionQuery, client: sessionClient, recovery: sessionRecovery });
  sessionContext.current = { view, query: sessionQuery, client: sessionClient, recovery: sessionRecovery };
  const [discovery, setDiscovery] = useState<DiscoveredClient[]>([]);
  const [dialog, setDialog] = useState<DialogState>(null);
  const [palette, setPalette] = useState(false);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [switchingTarget, setSwitchingTarget] = useState<string | null>(null);
  const [serviceError, setServiceError] = useState<string | null>(null);
  const [toast, setToast] = useState<Toast>(null);
  const [updaterCapability, setUpdaterCapability] = useState<UpdaterCapability>({ enabled: false, automatic_checks: false });
  const [automaticUpdateChecks, setAutomaticUpdateChecks] = useState(() => readPreference("mix.updates.autoCheck", ["on", "off"], "on") === "on");
  const [availableUpdate, setAvailableUpdate] = useState<Update | null>(null);
  const [updateDialogOpen, setUpdateDialogOpen] = useState(false);
  const [updatePhase, setUpdatePhase] = useState<UpdatePhase>("idle");
  const [updateResult, setUpdateResult] = useState<UpdateResult>("never");
  const [trayListening, setTrayListening] = useState(false);
  const initialRouteResolved = useRef(false);
  const pendingAfterSwitch = useRef<PendingAfterSwitch | null>(null);
  const loadGeneration = useRef(0);
  const sessionLoadGeneration = useRef(0);
  const enrollmentInFlight = useRef(false);
  const switchInFlight = useRef(false);
  const traySwitchAction = useRef<(app: string, profile: string) => void>(() => undefined);
  const t = useMemo(() => createTranslator(language), [language]);

  const currentAccountClient = snapshot.apps.find(isAccountClient);
  const currentAccountProfile = currentAccountClient?.profiles.find((profile) => profile.name === currentAccountClient.active);
  const currentAccountIdentity = currentAccountClient?.account_switch?.identity;
  const credentialStore = credentialStoreFor(snapshot);
  const interruptedSwitch = snapshot.recovery.interrupted_switch;
  const clientActionsBlocked = busy || interruptedSwitch.required;
  const trayActionContext = useRef({
    clients: snapshot.apps,
    credentialStoreAvailable: credentialStore.available,
    modalOpen: Boolean(dialog || updateDialogOpen),
    switchBlocked: loading || Boolean(serviceError) || clientActionsBlocked,
  });
  trayActionContext.current = {
    clients: snapshot.apps,
    credentialStoreAvailable: credentialStore.available,
    modalOpen: Boolean(dialog || updateDialogOpen),
    switchBlocked: loading || Boolean(serviceError) || clientActionsBlocked,
  };

  function notify(message: string, tone: "success" | "warning" | "error" | "neutral" = "success") {
    setToast({ message, tone });
  }

  async function load(silent = false) {
    const generation = ++loadGeneration.current;
    if (!silent) setLoading(true);
    try {
      const [nextSnapshot, nextDiscovery] = await Promise.all([
        request<Snapshot>("/api/state"),
        request<DiscoveredClient[]>("/api/discovery"),
      ]);
      if (generation !== loadGeneration.current) return;
      setSnapshot(nextSnapshot);
      setDiscovery(nextDiscovery);
      setServiceError(null);
      if (!initialRouteResolved.current) {
        initialRouteResolved.current = true;
        if (nextSnapshot.needs_setup) setView("environments");
      }
      const current = sessionContext.current;
      if (current.view === "workspaces") {
        void loadProjectSessions();
      } else if (current.view === "sessions") {
        void loadSessions(200, {
          query: current.query,
          adapter: current.client,
          recovery: current.recovery,
        });
      }
    } catch (error) {
      if (generation !== loadGeneration.current) return;
      setServiceError(error instanceof ApiError
        ? localizedFailureMessage(error, t, t("localServiceOffline"))
        : t("localServiceOffline"));
    } finally {
      if (generation === loadGeneration.current) setLoading(false);
    }
  }

  async function loadSessions(
    limit: number,
    options: { offset?: number; append?: boolean; query?: string; adapter?: string; recovery?: string } = {},
  ) {
    const generation = ++sessionLoadGeneration.current;
    setSessionsLoading(true);
    try {
      const offset = Math.max(0, options.offset || 0);
      const query = (options.query || "").trim();
      const queryPart = query ? `&query=${encodeURIComponent(query)}` : "";
      const adapterPart = options.adapter && options.adapter !== "all" ? `&adapter=${encodeURIComponent(options.adapter)}` : "";
      const recoveryPart = options.recovery && options.recovery !== "all" ? `&recovery=${encodeURIComponent(options.recovery)}` : "";
      const fetchedSessions = await request<Session[]>(`/api/sessions?app=all&limit=${limit + 1}&offset=${offset}${queryPart}${adapterPart}${recoveryPart}`);
      if (generation !== sessionLoadGeneration.current) return;
      const nextSessions = fetchedSessions.slice(0, limit);
      setSessions((current) => {
        if (!options.append) return nextSessions;
        const seen = new Set(current.map((session) => `${session.state_dir}\0${session.app}\0${session.id}`));
        return [...current, ...nextSessions.filter((session) => !seen.has(`${session.state_dir}\0${session.app}\0${session.id}`))];
      });
      setSessionsHasMore(fetchedSessions.length > limit);
    } catch (error) {
      if (generation !== sessionLoadGeneration.current) return;
      if (sessionContext.current.view === "sessions") notify(localizedFailureMessage(error, t, t("unknownError")), "error");
    } finally {
      if (generation === sessionLoadGeneration.current) setSessionsLoading(false);
    }
  }

  async function loadProjectSessions() {
    const generation = ++sessionLoadGeneration.current;
    setSessionsLoading(true);
    try {
      const rows = await request<Session[]>("/api/sessions?projects=true");
      if (generation !== sessionLoadGeneration.current) return;
      setSessions(rows);
      setSessionsHasMore(false);
    } catch (error) {
      if (generation !== sessionLoadGeneration.current) return;
      setSessions([]);
      setSessionsHasMore(false);
      if (sessionContext.current.view === "workspaces") {
        notify(localizedFailureMessage(error, t, t("unknownError")), "error");
      }
    } finally {
      if (generation === sessionLoadGeneration.current) setSessionsLoading(false);
    }
  }

  useEffect(() => { void load(); }, []);

  useEffect(() => {
    let lastRefresh = 0;
    const refreshExternalState = () => {
      const now = Date.now();
      if (now - lastRefresh < 750 || document.visibilityState === "hidden") return;
      lastRefresh = now;
      void load(true);
    };
    window.addEventListener("focus", refreshExternalState);
    document.addEventListener("visibilitychange", refreshExternalState);
    return () => {
      window.removeEventListener("focus", refreshExternalState);
      document.removeEventListener("visibilitychange", refreshExternalState);
    };
  }, []);

  useEffect(() => {
    if (view === "workspaces") {
      void loadProjectSessions();
      return;
    }
    if (view !== "sessions") return;
    // Invalidate an older catalog request immediately, including during the
    // search debounce window. The server is authoritative for cross-page,
    // cross-client filtering, so do not briefly present results from the
    // previous query as if they belonged to the new one.
    sessionLoadGeneration.current += 1;
    setSessions([]);
    setSessionsHasMore(false);
    const timer = window.setTimeout(
      () => void loadSessions(200, { query: sessionQuery, adapter: sessionClient, recovery: sessionRecovery }),
      sessionQuery.trim() ? 250 : 0,
    );
    return () => window.clearTimeout(timer);
  }, [view, sessionQuery, sessionClient, sessionRecovery]);

  useEffect(() => {
    try { window.localStorage.setItem("mix.language", language); } catch { /* optional preference */ }
    document.documentElement.lang = language === "zh" ? "zh-CN" : "en";
  }, [language]);

  useEffect(() => {
    try { window.localStorage.setItem("mix.theme", theme); } catch { /* optional preference */ }
    document.documentElement.dataset.theme = theme;
  }, [theme]);

  useEffect(() => {
    try { window.localStorage.setItem("mix.updates.autoCheck", automaticUpdateChecks ? "on" : "off"); } catch { /* optional preference */ }
  }, [automaticUpdateChecks]);

  useEffect(() => {
    let active = true;
    void getUpdaterCapability()
      .then((capability) => { if (active) setUpdaterCapability(capability); })
      .catch(() => { if (active) setUpdaterCapability({ enabled: false, automatic_checks: false }); });
    return () => { active = false; };
  }, []);

  useEffect(() => {
    let active = true;
    let stopListening: () => void = () => undefined;
    const handleTrayAction = (action: TrayAction) => {
      if (!active || trayActionContext.current.modalOpen) return;
      if (action.kind === "view" && action.view === "sessions") {
        setPalette(false);
        setView("sessions");
        return;
      }
      if (action.kind !== "switch" || trayActionContext.current.switchBlocked) return;
      const app = trayActionContext.current.clients.find((client) => client.name === action.app);
      const profile = app?.profiles.find((item) => item.name === action.profile);
      if (!app || !profile || !trayProfileActionReady(app, profile, trayActionContext.current.credentialStoreAvailable)) return;
      setPalette(false);
      traySwitchAction.current(app.name, profile.name);
    };
    void listenTrayActions(handleTrayAction)
      .then((unlisten) => {
        if (active) {
          stopListening = unlisten;
          setTrayListening(true);
        }
        else unlisten();
      })
      .catch(() => undefined);
    return () => {
      active = false;
      stopListening();
    };
  }, []);

  useEffect(() => {
    if (loading || !trayListening || !isDesktopShell()) return;
    const switchBlocked = Boolean(serviceError) || clientActionsBlocked || Boolean(dialog || updateDialogOpen);
    void updateTrayState({
      language,
      health_status: serviceError ? "attention" : snapshot.health.status,
      clients: snapshot.apps.slice(0, 12).map((app) => {
        const profiles = app.profiles
          .filter((profile) => trayProfileActionReady(app, profile, credentialStore.available))
          .slice(0, 12)
          .map((profile) => ({ name: profile.name, label: profileDisplayLabel(profile), active: profile.name === app.active }));
        return {
          name: app.name,
          label: adapterName(app.adapter),
          active_profile: profileDisplayLabel(app.profiles.find((profile) => profile.name === app.active)) || app.active,
          profiles,
          switchable: !switchBlocked && profiles.length > 0,
        };
      }),
    }).catch(() => undefined);
  }, [
    clientActionsBlocked,
    credentialStore.available,
    dialog,
    language,
    loading,
    serviceError,
    snapshot.apps,
    snapshot.health.status,
    trayListening,
    updateDialogOpen,
    updatePhase,
  ]);

  useEffect(() => {
    if (updaterCapability.enabled && updaterCapability.automatic_checks && automaticUpdateChecks && updateResult === "never" && updatePhase === "idle") {
      void checkForUpdates(true);
    }
  }, [updaterCapability, automaticUpdateChecks, updateResult, updatePhase]);

  useEffect(() => () => { void availableUpdate?.close().catch(() => undefined); }, [availableUpdate]);

  useEffect(() => {
    if (!toast) return;
    const timer = window.setTimeout(() => setToast(null), 3600);
    return () => window.clearTimeout(timer);
  }, [toast]);

  useEffect(() => {
    if (!availableUpdate || !updateDialogOpen) return;
    const onEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") closeUpdateDialog();
    };
    window.addEventListener("keydown", onEscape);
    return () => window.removeEventListener("keydown", onEscape);
  }, [availableUpdate, updateDialogOpen, updatePhase]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        if (!dialog && !updateDialogOpen) setPalette((value) => !value);
      }
      if (event.key === "Escape") {
        setPalette(false);
        if (!busy) {
          pendingAfterSwitch.current = null;
          setDialog(null);
        }
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [busy, dialog, updateDialogOpen]);

  async function runProject(project: ProjectSummary, appName: string, profileName: string) {
    const app = snapshot.apps.find((item) => item.name === appName);
    if (app && isAccountClient(app) && profileName !== app.active) {
      pendingAfterSwitch.current = { kind: "project", app: appName, profile: profileName, cwd: project.path };
      await switchAccount(appName, profileName);
      return;
    }
    setBusy(true);
    try {
      const opensDesktop = clientCapabilityAvailable(app, "project.open_native");
      await request(opensDesktop ? "/api/native/open" : "/api/run", {
        method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify(opensDesktop
          ? { app: appName, cwd: project.path }
          : { app: appName, profile: profileName, cwd: project.path }),
      });
      await load(true);
      notify(t(opensDesktop ? "projectOpenedInClient" : "runLaunched"));
    } catch (error) {
      notify(localizedFailureMessage(error, t, t("unknownError")), "error");
    } finally {
      setBusy(false);
    }
  }

  async function resumeSession(session: Session) {
    if (!session.resume_id || session.recoverability !== "A") return;
    const app = snapshot.apps.find((item) => item.name === session.app);
    const workspace = snapshot.workspaces
      .filter((item) => pathContains(item.path, session.cwd))
      .sort((left, right) => right.path.length - left.path.length)[0];
    const boundProfile = session.profile ? undefined : workspace?.bindings[session.app];
    if (boundProfile && app?.active !== boundProfile) {
      pendingAfterSwitch.current = {
        kind: "session",
        app: session.app,
        profile: boundProfile,
        resumeId: session.resume_id,
      };
      await switchAccount(session.app, boundProfile);
      return;
    }
    setBusy(true);
    try {
      await request("/api/sessions/resume", {
        method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ app: session.app, resume_id: session.resume_id }),
      });
      notify(t("sessionResumeLaunched"));
    } catch (error) {
      notify(localizedFailureMessage(error, t, t("unknownError")), "error");
    } finally {
      setBusy(false);
    }
  }

  async function createClient(payload: Record<string, unknown>) {
    setBusy(true);
    try {
      await request("/api/apps", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(payload) });
      setDialog(null);
      await load(true);
      notify(t("clientConnected"));
    } finally {
      setBusy(false);
    }
  }

  async function createProfile(path: string, payload: Record<string, unknown>) {
    setBusy(true);
    try {
      await request(path, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(payload) });
      setDialog(null);
      await load(true);
      notify(t(path === "/api/accounts/capture" ? "accountAdded" : "profileCreated"));
    } finally {
      setBusy(false);
    }
  }

  async function enrollAccount(app: string, repairProfile?: string) {
    if (enrollmentInFlight.current) return;
    enrollmentInFlight.current = true;
    try {
      if (repairProfile) {
        try {
          const localRepair = await request<LocalRepairResult>("/api/accounts/repair/active-login", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ app, profile: repairProfile }),
          });
          if (localRepair.repaired || localRepair.status === "already_ready") {
            setDialog(null);
            await load(true);
            notify(t("accountRepairedLocally"));
            return;
          }
        } catch (error) {
          if (!(error instanceof ApiError) || error.code !== "MIX_ACCOUNT_LOCAL_REPAIR_UNAVAILABLE") throw error;
          const destination = snapshot.apps.find((item) => item.name === app)?.profiles.find((item) => item.name === repairProfile);
          if (destination?.provider?.official === false) {
            notify(t("customAccountRepairUnavailable"), "error");
            return;
          }
        }
      }
      const started = await request<EnrollmentResult>("/api/accounts/enroll", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ app, repair_profile: repairProfile || null }),
      });
      setDialog(null);
      notify(t(repairProfile ? "repairLoginStarted" : "enrollmentStarted"));
      for (let attempt = 0; attempt < ENROLLMENT_POLL_ATTEMPTS; attempt += 1) {
        await new Promise((resolve) => window.setTimeout(resolve, 1000));
        const status = await request<EnrollmentResult>(`/api/accounts/enroll/status?id=${encodeURIComponent(started.enrollment_id)}`);
        if (status.state === "completed") {
          await load(true);
          const message = t(repairProfile ? "accountRepaired" : "accountAdded");
          notify(
            status.error ? `${message} · ${t("operationCompletedWithWarning")}` : message,
            status.error ? "warning" : "success",
          );
          return;
        }
        if (status.state === "failed") {
          if (status.code === "MIX_ACCOUNT_REPAIR_IDENTITY_MISMATCH") throw new Error(t("accountRepairIdentityMismatch"));
          if (status.code === "MIX_ENROLLMENT_TIMEOUT") throw new Error(t("enrollmentTimedOut"));
          if (status.code === "MIX_SENSITIVE_DATA_REJECTED") throw new Error(t("sensitiveConfigRejected"));
          throw new Error(t(repairProfile ? "accountRepairFailed" : "enrollmentFailed"));
        }
      }
      throw new Error(t("enrollmentTimedOut"));
    } catch (error) {
      notify(localizedFailureMessage(error, t, t(repairProfile ? "accountRepairFailed" : "enrollmentFailed")), "error");
    } finally {
      enrollmentInFlight.current = false;
    }
  }

  async function createWorkspace(payload: Record<string, unknown>) {
    const editing = dialog?.kind === "workspace" && Boolean(dialog.workspace);
    setBusy(true);
    try {
      await request("/api/workspaces", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(payload) });
      setDialog(null);
      await load(true);
      notify(t(editing ? "workspaceUpdated" : "workspaceCreated"));
    } finally {
      setBusy(false);
    }
  }

  async function deleteWorkspace(workspace: string) {
    setBusy(true);
    try {
      await request("/api/workspaces", {
        method: "DELETE",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ workspace }),
      });
      setDialog(null);
      await load(true);
      notify(t("workspaceRemoved"));
    } catch (error) {
      notify(localizedFailureMessage(error, t, t("unknownError")), "error");
    } finally {
      setBusy(false);
    }
  }

  async function updateProfile(app: string, profile: string, payload: Record<string, unknown>) {
    setBusy(true);
    try {
      await request("/api/profiles", {
        method: "PATCH",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ app, profile, ...payload }),
      });
      setDialog(null);
      await load(true);
      notify(t("profileEdited"));
    } finally {
      setBusy(false);
    }
  }

  async function deleteProfile(app: string, profile: string, replacementProfile?: string) {
    const selectedApp = snapshot.apps.find((item) => item.name === app);
    const selectedProfile = selectedApp?.profiles.find((item) => item.name === profile);
    const removingAccount = Boolean(isAccountClient(selectedApp) && selectedProfile?.managed_account);
    let replacementApplied = false;
    setBusy(true);
    try {
      if (replacementProfile) {
        await request("/api/switch", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ app, profile: replacementProfile }),
        });
        replacementApplied = true;
      }
      const result = await request<OperationResult>("/api/profiles", {
        method: "DELETE",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ app, profile, detach_workspaces: true }),
      });
      setDialog(null);
      await load(true);
      const completedWithWarning = Boolean(result.warnings?.length);
      notify(
        t(completedWithWarning
          ? removingAccount ? "accountRemovedWithWarning" : "environmentRemovedWithWarning"
          : removingAccount ? "accountRemoved" : "environmentRemoved"),
        completedWithWarning ? "warning" : "success",
      );
    } catch (error) {
      if (replacementApplied) {
        await load(true).catch(() => undefined);
        notify(t("replacementAppliedRemovalFailed"), "error");
      } else {
        notify(localizedFailureMessage(error, t, t("unknownError")), "error");
      }
    } finally {
      setBusy(false);
    }
  }

  async function switchBack(activity: Activity) {
    if (!activity.id) return;
    setBusy(true);
    try {
      const result = await request<OperationResult>("/api/activity/switch-back", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ activity: activity.id }) });
      setDialog(null);
      await load(true);
      const warning = result.restart_warning || result.activity_warning;
      notify(warning ? `${t("switchBackCompleted")} · ${t("operationCompletedWithWarning")}` : t("switchBackCompleted"), warning ? "warning" : "success");
    } catch (error) {
      notify(localizedFailureMessage(error, t, t("unknownError")), "error");
    } finally {
      setBusy(false);
    }
  }

  async function recoverInterruptedSwitch() {
    setBusy(true);
    try {
      const result = await request<OperationResult>("/api/recovery/interrupted", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({}),
      });
      setDialog(null);
      await load(true);
      const warning = result.restart_warning || result.activity_warning;
      const cleanupCompleted = result.status === "cleanup_completed";
      notify(
        cleanupCompleted ? t("transactionCleanupCompleted") : warning ? `${t("interruptedSwitchRecovered")} · ${t("operationCompletedWithWarning")}` : t("interruptedSwitchRecovered"),
        warning ? "warning" : "success",
      );
    } catch (error) {
      await load(true);
      notify(localizedFailureMessage(error, t, t("unknownError")), "error");
    } finally {
      setBusy(false);
    }
  }

  async function switchAccount(app: string, profile: string) {
    if (switchInFlight.current) return;
    switchInFlight.current = true;
    const pending = pendingAfterSwitch.current;
    pendingAfterSwitch.current = null;
    setSwitchingTarget(`${app}/${profile}`);
    setBusy(true);
    try {
      const result = await request<OperationResult>("/api/switch", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ app, profile }) });
      setDialog(null);
      await load(true);
      if (pending?.kind === "project" && pending.app === app && pending.profile === profile) {
        try {
          await request("/api/native/open", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ app: pending.app, cwd: pending.cwd }),
          });
          notify(result.warning ? `${t("projectOpenedInClient")} · ${t("operationCompletedWithWarning")}` : t("projectOpenedInClient"), result.warning ? "warning" : "success");
        } catch (error) {
          const reason = localizedFailureMessage(error, t, t("unknownError"));
          notify(t("projectNotOpenedAfterSwitch", { reason }), "warning");
        }
        return;
      }
      if (pending?.kind === "session" && pending.app === app && pending.profile === profile) {
        try {
          await request("/api/sessions/resume", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ app: pending.app, resume_id: pending.resumeId }),
          });
          notify(result.warning ? `${t("sessionResumeLaunched")} · ${t("operationCompletedWithWarning")}` : t("sessionResumeLaunched"), result.warning ? "warning" : "success");
        } catch (error) {
          notify(t("sessionNotOpenedAfterSwitch", { reason: localizedFailureMessage(error, t, t("unknownError")) }), "warning");
        }
        return;
      }
      const warning = result.restart_warning || result.activity_warning || result.warning;
      const client = snapshot.apps.find((item) => item.name === app);
      const environmentSelection = result.status === "selected" || result.status === "already_selected";
      const completed = environmentSelection
        ? t("environmentSelected")
        : clientCapabilityAvailable(client, "process.control")
          ? t(result.status === "already_active" ? "clientOpened" : "switchCompletedAndClientOpened")
          : t("switchCompleted");
      notify(warning ? `${completed} · ${t("operationCompletedWithWarning")}` : completed, warning ? "warning" : "success");
    } catch (error) {
      if (error instanceof ApiError && error.code === "MIX_ACCOUNT_REAUTH_REQUIRED") {
        await enrollAccount(app, profile);
      } else {
        const summary = localizedFailureMessage(error, t, t("unknownError"));
        const restoredProfile = error instanceof ApiError && error.code === "MIX_SWITCH_ROLLED_BACK"
          ? error.details.from_profile
          : undefined;
        const restored = typeof restoredProfile === "string"
          ? snapshot.apps
            .find((item) => item.name === app)
            ?.profiles.find((item) => item.name === restoredProfile)
          : undefined;
        notify(
          restored
            ? t("switchRolledBackToProfile", { summary, profile: profileDisplayLabel(restored) })
            : summary,
          "error",
        );
      }
    } finally {
      switchInFlight.current = false;
      setSwitchingTarget(null);
      setBusy(false);
    }
  }
  traySwitchAction.current = (app, profile) => { void switchAccount(app, profile); };

  async function copy(value: string | undefined, confirmation: CopyKey = "pathCopied") {
    if (!value) return;
    try {
      await writeClipboardText(value);
      notify(t(confirmation));
    } catch {
      notify(t("unknownError"), "error");
    }
  }

  async function copyDiagnostics() {
    try {
      const report = await request<Record<string, unknown>>("/api/diagnostics");
      const supportReport = {
        ...report,
        product: { name: "mix", version: __MIX_VERSION__, interface: isDesktopShell() ? "mac_app" : "local_web" },
        interface_preferences: { language, theme },
      };
      await writeClipboardText(JSON.stringify(supportReport, null, 2));
      notify(t("diagnosticsCopied"));
    } catch (error) {
      notify(`${t("diagnosticsFailed")}: ${error instanceof Error ? error.message : t("unknownError")}`, "error");
    }
  }

  async function openLicenses() {
    try {
      await openThirdPartyLicenses();
    } catch (error) {
      notify(`${t("licensesFailed")}: ${error instanceof Error ? error.message : t("unknownError")}`, "error");
    }
  }

  async function checkForUpdates(silent = false) {
    if (!isDesktopShell() || updatePhase !== "idle") return;
    setUpdatePhase("checking");
    try {
      const update = await checkForTauriUpdate({ timeout: 20_000 });
      if (update) {
        setAvailableUpdate(update);
        setUpdateDialogOpen(true);
        setUpdateResult("available");
        setUpdatePhase("available");
      } else {
        setUpdateResult("current");
        setUpdatePhase("idle");
        if (!silent) notify(t("updateCurrent"));
      }
    } catch (error) {
      setUpdateResult("failed");
      setUpdatePhase("idle");
      if (!silent) notify(`${t("updateCheckFailed")}: ${failureMessage(error, t("updateCheckRetry"))}`, "error");
    }
  }

  function closeUpdateDialog() {
    setUpdateDialogOpen(false);
  }

  function openOrCheckUpdates() {
    if (availableUpdate && updatePhase === "available") {
      setUpdateDialogOpen(true);
      return;
    }
    void checkForUpdates(false);
  }

  async function openReleaseDownload() {
    try {
      await openLatestRelease();
    } catch (error) {
      notify(`${t("releasePageFailed")}: ${error instanceof Error ? error.message : t("unknownError")}`, "error");
    }
  }

  const navigation: { id: View; label: CopyKey; icon: IconName }[] = [
    { id: "environments", label: "environments", icon: "user" },
    { id: "sessions", label: "sessions", icon: "history" },
    { id: "workspaces", label: "workspaces", icon: "folder" },
  ];
  const effectiveHealthStatus = serviceError ? "attention" : snapshot.health.status;
  return <><div className="app-shell">
    <aside className="sidebar" aria-label="mix">
      <div className="brand"><div className="brand-mark" aria-hidden="true">m</div><div><strong>mix</strong><span>{t("productSubtitle")}</span></div></div>
      <nav aria-label={t("primaryNavigation")}>
        {navigation.map((item) => <button key={item.id} aria-label={t(item.label)} title={t(item.label)} aria-current={view === item.id ? "page" : undefined} className={view === item.id ? "nav-item active" : "nav-item"} onClick={() => setView(item.id)}><Icon name={item.icon} /><span>{t(item.label)}</span></button>)}
      </nav>
      <div className="sidebar-spacer" />
      <button aria-label={t("settings")} title={t("settings")} aria-current={view === "settings" ? "page" : undefined} className={view === "settings" ? "nav-item active" : "nav-item"} onClick={() => setView("settings")}><Icon name="settings"/><span>{t("settings")}</span></button>
      <div className="local-boundary" role="note" aria-label={`${t("localOnly")} · ${effectiveHealthStatus === "healthy" ? t("healthy") : t("attention")}`} title={`${t("localOnly")} · ${effectiveHealthStatus === "healthy" ? t("healthy") : t("attention")}`}><span className="pulse-dot"/><div><strong>{t("localOnly")}</strong><small>{effectiveHealthStatus === "healthy" ? t("healthy") : t("attention")}</small></div></div>
    </aside>

    <div className="workspace-shell">
      <header className="topbar">
        <div className="topbar-context"><Icon name="shield"/><span>{t("localOnly")}</span></div>
        <div className="topbar-spacer"/>
        {currentAccountClient && <button className="current-account-chip" onClick={() => setView("environments")} aria-label={t("manageCurrentAccount", { client: adapterName(currentAccountClient.adapter) })} title={t("manageCurrentAccount", { client: adapterName(currentAccountClient.adapter) })}><ClientGlyph adapter={currentAccountClient.adapter}/><span><small>{adapterName(currentAccountClient.adapter)} · {t("activeAccount")}</small><strong>{currentAccountProfile ? profileDisplayLabel(currentAccountProfile) : currentAccountIdentity ? currentAccountLabel(currentAccountClient, t) : t("setupRequired")}</strong></span><Icon name="chevron"/></button>}
        <button className="icon-button topbar-action" onClick={() => setPalette(true)} aria-label={t("searchActions")} title={t("searchActions")}><Icon name="search"/></button>
        <button className={effectiveHealthStatus === "healthy" ? "health-button healthy" : "health-button attention"} onClick={() => setView("settings")} aria-label={effectiveHealthStatus === "healthy" ? t("healthy") : t("attention")}><Icon name={effectiveHealthStatus === "healthy" ? "shield" : "warning"}/></button>
        <button className="language-toggle" onClick={() => setLanguage(language === "zh" ? "en" : "zh")} aria-label={language === "zh" ? "Switch to English" : "Switch to Chinese"}>{language === "zh" ? "EN" : "中"}</button>
      </header>

      {serviceError && <div className="service-banner" role="alert"><Icon name="warning"/><div><strong>{t("localServiceOffline")}</strong><span>{serviceError}</span></div><button className="button secondary small" onClick={() => void load()}>{t("retry")}</button></div>}
      {!loading && interruptedSwitch.required && <div className="recovery-banner" role="alert" aria-live="assertive"><Icon name="warning"/><div><strong>{t("interruptedSwitchTitle")}</strong><p>{t(interruptedSwitch.status === "invalid" ? "interruptedSwitchInvalidDetail" : interruptedSwitch.status === "cleanup_pending" ? "transactionCleanupDetail" : "interruptedSwitchDetail")}</p>{interruptedSwitch.from_profile && interruptedSwitch.to_profile && <span>{t("interruptedSwitchScope", { from: interruptedSwitch.from_profile, to: interruptedSwitch.to_profile })}</span>}</div><button className="button danger" disabled={busy || !["pending", "cleanup_pending"].includes(interruptedSwitch.status || "")} onClick={() => void recoverInterruptedSwitch()}><Icon name="refresh"/>{busy ? t("recoveringPreviousState") : interruptedSwitch.status === "cleanup_pending" ? t("cleanupTransaction") : interruptedSwitch.status === "invalid" ? t("recoveryUnavailable") : t("recoverPreviousState")}</button></div>}

      <main className="content" aria-busy={loading}>
        {loading ? <LoadingView/> : <>
          {view === "workspaces" && <WorkspacesView
            t={t}
            language={language}
            snapshot={snapshot}
            sessions={sessions}
            loading={sessionsLoading}
            busy={clientActionsBlocked}
            onAdd={() => setDialog({ kind: "workspace" })}
            onEdit={(workspace) => setDialog({ kind: "workspace", workspace })}
            onDelete={(workspace) => setDialog({ kind: "workspace-delete", workspace })}
            onResume={resumeSession}
            onRun={runProject}
          />}
          {view === "environments" && <EnvironmentsView
            t={t}
            snapshot={snapshot}
            canAddClient={discovery.some((client) => !client.configured)}
            busy={clientActionsBlocked}
            switchingTarget={switchingTarget}
            onRefresh={() => load(true)}
            onAddClient={() => setDialog({ kind: "client" })}
            onAddProfile={(app) => setDialog({ kind: "profile", app })}
            onEditProfile={(app, profile) => setDialog({ kind: "profile-edit", app, profile })}
            onDeleteProfile={(app, profile) => setDialog({ kind: "profile-delete", app, profile })}
            onSwitch={(app, profile) => { void switchAccount(app, profile); }}
          />}
          {view === "sessions" && <SessionsView
            t={t}
            language={language}
            sessions={sessions}
            query={sessionQuery}
            client={sessionClient}
            recovery={sessionRecovery}
            hasMore={sessionsHasMore}
            loading={sessionsLoading}
            busy={clientActionsBlocked}
            clients={snapshot.apps}
            workspaces={snapshot.workspaces}
            onCopy={copy}
            onResume={resumeSession}
            onQuery={setSessionQuery}
            onClient={setSessionClient}
            onRecovery={setSessionRecovery}
            onLoadMore={() => loadSessions(200, { offset: sessions.length, append: true, query: sessionQuery, adapter: sessionClient, recovery: sessionRecovery })}
          />}
          {view === "activity" && <ActivityView
            t={t}
            language={language}
            activity={snapshot.activity}
            clients={snapshot.apps}
            busy={clientActionsBlocked}
            onSwitchBack={(item) => setDialog({ kind: "switch-back", activity: item })}
          />}
          {view === "settings" && <SettingsView t={t} language={language} theme={theme} snapshot={snapshot} serviceError={serviceError} updaterCapability={updaterCapability} automaticUpdateChecks={automaticUpdateChecks} updatePhase={updatePhase} updateResult={updateResult} onAutomaticUpdateChecks={setAutomaticUpdateChecks} onCheckUpdates={openOrCheckUpdates} onLanguage={setLanguage} onTheme={setTheme} onRefresh={async () => { await load(true); notify(t("refreshed")); }} onDiagnostics={copyDiagnostics} onLicenses={openLicenses} onActivity={() => setView("activity")}/>}
        </>}
      </main>
    </div>
  </div>

    {dialog?.kind === "client" && <ClientDialog
      t={t}
      discovery={discovery}
      onClose={() => { if (!busy) setDialog(null); }}
      onCreate={createClient}
    />}
    {dialog?.kind === "profile" && <ProfileDialog
      t={t}
      apps={snapshot.apps}
      initialApp={dialog.app}
      credentialStoreAvailable={credentialStore.available}
      onClose={() => { if (!busy) setDialog(null); }}
      onCreate={createProfile}
      onEnroll={enrollAccount}
    />}
    {dialog?.kind === "profile-edit" && <ProfileEditDialog
      t={t}
      app={snapshot.apps.find((item) => item.name === dialog.app)}
      profileName={dialog.profile}
      onClose={() => { if (!busy) setDialog(null); }}
      onSave={(payload) => updateProfile(dialog.app, dialog.profile, payload)}
    />}
    {dialog?.kind === "profile-delete" && <ProfileDeleteDialog
      t={t}
      app={snapshot.apps.find((item) => item.name === dialog.app)}
      profileName={dialog.profile}
      workspaces={snapshot.workspaces}
      busy={busy}
      onClose={() => { if (!busy) setDialog(null); }}
      onConfirm={(replacement) => void deleteProfile(dialog.app, dialog.profile, replacement)}
    />}
    {dialog?.kind === "workspace" && <WorkspaceDialog
      t={t}
      apps={snapshot.apps}
      initial={snapshot.workspaces.find((workspace) => workspace.id === dialog.workspace)}
      onClose={() => { if (!busy) setDialog(null); }}
      onCreate={createWorkspace}
    />}
    {dialog?.kind === "workspace-delete" && <WorkspaceDeleteDialog
      t={t}
      workspace={snapshot.workspaces.find((workspace) => workspace.id === dialog.workspace)}
      busy={busy}
      onClose={() => { if (!busy) setDialog(null); }}
      onConfirm={() => void deleteWorkspace(dialog.workspace)}
    />}
    {dialog?.kind === "switch-back" && <SwitchBackDialog
      t={t}
      activity={dialog.activity}
      busy={busy}
      onClose={() => { if (!busy) setDialog(null); }}
      onConfirm={() => void switchBack(dialog.activity)}
    />}
    {availableUpdate && updateDialogOpen && <UpdateDialog t={t} update={availableUpdate} onClose={closeUpdateDialog} onDownload={openReleaseDownload}/>}
    {palette && <CommandPalette t={t} language={language} theme={theme} onClose={() => setPalette(false)} onView={setView} onDialog={setDialog} onLanguage={setLanguage} onTheme={setTheme}/>}
    {toast && <div className={`toast ${toast.tone}`} role={toast.tone === "error" ? "alert" : "status"} aria-live={toast.tone === "error" ? "assertive" : "polite"}><Icon name={toast.tone === "error" || toast.tone === "warning" ? "warning" : "check"}/><span>{toast.message}</span></div>}
  </>;
}

function PageHeader({ eyebrow, title, detail, actions }: { eyebrow: string; title: string; detail: string; actions?: ReactNode }) {
  return <div className="page-header"><div><p className="eyebrow">{eyebrow}</p><h1>{title}</h1><p>{detail}</p></div>{actions && <div className="page-actions">{actions}</div>}</div>;
}

function WorkspacesView({ t, language, snapshot, sessions, loading, busy, onAdd, onEdit, onDelete, onResume, onRun }: { t: Translate; language: Language; snapshot: Snapshot; sessions: Session[]; loading: boolean; busy: boolean; onAdd: () => void; onEdit: (workspace: string) => void; onDelete: (workspace: string) => void; onResume: (session: Session) => void; onRun: (project: ProjectSummary, app: string, profile: string) => void }) {
  const [query, setQuery] = useState("");
  const projects = useMemo(() => buildProjects(snapshot.workspaces, sessions), [snapshot.workspaces, sessions]);
  const filtered = projects.filter((project) => `${project.name} ${project.path} ${project.sessions.map((session) => session.title || "").join(" ")}`.toLowerCase().includes(query.toLowerCase()));
  function launchOptions(project: ProjectSummary) {
    const choices: { app: Client; profile: Profile }[] = [];
    const used = new Set<string>();
    const add = (appName: string | undefined, profileName: string | undefined) => {
      if (appName && project.binding_conflicts?.[appName]?.length) return;
      const app = snapshot.apps.find((item) => item.name === appName);
      const profile = app?.profiles.find((item) => item.name === profileName);
      if (!app || !profile || used.has(app.name)) return;
      used.add(app.name);
      choices.push({ app, profile });
    };
    for (const app of snapshot.apps) add(app.name, project.bindings[app.name] || app.active || undefined);
    return choices;
  }
  return <div className="page projects-page"><PageHeader eyebrow={t("workspaces")} title={t("workspaceTitle")} detail={t("workspaceDetail")} actions={<button className="button secondary" onClick={onAdd}><Icon name="plus"/>{t("chooseProjectFolderAction")}</button>}/>
    <div className="project-discovery-note" role="note"><Icon name="shield"/><div><strong>{t("projectsFoundAutomatically")}</strong><span>{t("projectsFoundAutomaticallyDetail")}</span></div></div>
    {!loading && (projects.length > 4 || query) && <div className="toolbar"><label className="search-field"><Icon name="search"/><input aria-label={t("searchWorkspace")} value={query} onChange={(event) => setQuery(event.target.value)} placeholder={t("searchWorkspace")}/></label><div className="filter-summary">{filtered.length} / {projects.length}</div></div>}
    {!loading && <div className="project-list">{filtered.map((project) => {
      const latest = project.latest;
      const options = launchOptions(project);
      const primary = options[0];
      const latestClient = latest && snapshot.apps.find((item) => item.name === latest.app);
      const latestProfile = latest && latestClient?.profiles.find((item) => item.name === latest.profile);
      const latestTargetProfile = latest && project.registered ? project.bindings[latest.app] : undefined;
      const latestNeedsAccountConfirmation = Boolean(
        latest && latestClient && isAccountClient(latestClient) && latestTargetProfile && latestClient.active !== latestTargetProfile,
      );
      const canContinue = latest?.recoverability === "A" && Boolean(latest.resume_id);
      const conflictClients = Object.keys(project.binding_conflicts || {}).map((name) => adapterName(snapshot.apps.find((app) => app.name === name)?.adapter || name));
      return <article className="project-card" key={project.id}>
        <div className="project-card-head"><div className="workspace-icon"><Icon name="folder"/></div><div className="project-identity"><div className="workspace-title"><h2>{project.name}</h2>{!project.registered && <span className="tag neutral">{t("autoDetected")}</span>}{!project.exists && <span className="tag danger">{t("missing")}</span>}{conflictClients.length > 0 && <span className="tag danger">{t("bindingConflict")}</span>}</div><p className="mono">{compactPath(project.path)}</p></div><div className="project-actions">{conflictClients.length > 0 ? <button className="button primary" disabled={busy || !project.exists} title={t("bindingConflictDetail", { clients: conflictClients.join(", ") })} onClick={() => onEdit(project.path)}><Icon name="warning"/>{t("resolveBindingConflict")}</button> : canContinue ? <button className="button primary" disabled={busy || !project.exists} onClick={() => latest && onResume(latest)}><Icon name="play"/>{t("continueWorking")}</button> : latestNeedsAccountConfirmation && latest && latestTargetProfile ? <button className="button primary" disabled={busy || !project.exists} onClick={() => onRun(project, latest.app, latestTargetProfile)}><Icon name="refresh"/>{t("switchToProjectAccount")}</button> : primary ? <button className="button primary" disabled={busy || !project.exists} onClick={() => onRun(project, primary.app.name, primary.profile.name)}><Icon name="play"/>{t(clientCapabilityAvailable(primary.app, "project.open_native") ? "openInClientApp" : "openInClientTerminal", { client: adapterName(primary.app.adapter) })}</button> : <button className="button primary" disabled><Icon name="warning"/>{t("noAccountReady")}</button>}
          {options.length > 1 && <details className="project-more"><summary role="button" className="icon-button" aria-label={t("moreProjectActions")} title={t("moreProjectActions")}><Icon name="more"/></summary><div>{options.map(({ app, profile }) => <button key={app.name} disabled={busy || !project.exists} onClick={() => onRun(project, app.name, profile.name)}><ClientGlyph adapter={app.adapter}/><span><strong>{t(clientCapabilityAvailable(app, "project.open_native") ? "openInClientApp" : "openInClientTerminal", { client: adapterName(app.adapter) })}</strong><small>{profileDisplayLabel(profile)}</small></span></button>)}</div></details>}
        </div></div>
          {conflictClients.length > 0 && <p className="project-blocked-reason"><Icon name="warning"/>{t("bindingConflictDetail", { clients: conflictClients.join(", ") })}</p>}{latest ? <div className="project-latest"><div className="project-latest-label"><span>{t("lastTask")}</span><time>{formatTimestamp(latest.updated_at, language)}</time></div><div className="project-task"><ClientGlyph adapter={latestClient?.adapter || latest.app}/><div><strong>{sessionDisplayTitle(latest, t)}</strong><span>{[adapterName(latestClient?.adapter || latest.app), latestProfile ? profileDisplayLabel(latestProfile) : latest.profile].filter(Boolean).join(" · ")}</span></div><RecoveryBadge level={latest.recoverability || "B"} t={t}/></div>{latest.recoverability === "B" && <p className="project-blocked-reason"><Icon name="shield"/>{recoveryReasonLabel(latest.recovery_reason, t)}</p>}</div> : <div className="project-no-task"><Icon name="plus"/><span>{t("noProjectTasks")}</span></div>}
        <footer className="project-card-foot"><span><Icon name="history"/>{t("sessionCount", { count: project.sessionCount })}</span><span><Icon name="shield"/>{t("nativeHistoryUntouchedShort")}</span>{project.registered && <div className="project-management"><button className="text-button" disabled={busy} onClick={() => onEdit(project.path)}><Icon name="settings"/>{t("projectSettings")}</button><button className="text-button danger-text" disabled={busy} onClick={() => onDelete(project.path)}><Icon name="trash"/>{t("remove")}</button></div>}</footer>
      </article>;
    })}</div>}
    {loading && <div className="session-loading" role="status"><span className="spinner"/>{t("discoveringProjects")}</div>}
    {!loading && !filtered.length && <EmptyState
      icon="folder"
      title={query ? t("noProjectMatches") : t("noWorkspacesTitle")}
      detail={query ? t("adjustProjectSearch") : t("noWorkspacesDetail")}
      action={!query ? <button className="button primary" onClick={onAdd}><Icon name="plus"/>{t("chooseProjectFolderAction")}</button> : undefined}
    />}
  </div>;
}

function EnvironmentsView({ t, snapshot, canAddClient, busy, switchingTarget, onRefresh, onAddClient, onAddProfile, onEditProfile, onDeleteProfile, onSwitch }: { t: Translate; snapshot: Snapshot; canAddClient: boolean; busy: boolean; switchingTarget: string | null; onRefresh: () => Promise<void>; onAddClient: () => void; onAddProfile: (app: string) => void; onEditProfile: (app: string, profile: string) => void; onDeleteProfile: (app: string, profile: string) => void; onSwitch: (app: string, profile: string) => void }) {
  const credentialStoreAvailable = credentialStoreFor(snapshot).available;
  return <div className="page accounts-page"><PageHeader eyebrow={t("environments")} title={t("environmentsTitle")} detail={t("environmentsDetail")} actions={snapshot.apps.length > 0 && canAddClient ? <button className="button secondary" onClick={onAddClient}><Icon name="plus"/>{t("addClient")}</button> : undefined}/>
    <div className="account-client-list">{snapshot.apps.map((app) => {
      const active = app.profiles.find((profile) => profile.name === app.active);
      const managedAccountCount = app.profiles.filter(isManagedAccountProfile).length;
      const environmentProfileCount = app.profiles.length - managedAccountCount;
      const mixedProfiles = managedAccountCount > 0 && environmentProfileCount > 0;
      const supportsManagedAccounts = isAccountClient(app);
      const accountLikeClient = managedAccountCount > 0 || (supportsManagedAccounts && app.profiles.length === 0);
      const profileCountLabel = mixedProfiles
        ? t("profileMixCount", { accounts: managedAccountCount, environments: environmentProfileCount })
        : t(accountLikeClient ? "accountCount" : "environmentCount", { count: app.profiles.length });
      const savedProfilesLabel = t(mixedProfiles ? "savedAccountsAndEnvironments" : accountLikeClient ? "savedAccounts" : "savedEnvironments");
      const accountSwitchReady = !supportsManagedAccounts || managedAccountSwitchReady(app, credentialStoreAvailable);
      const capabilityDetail = accountSwitchCapabilityDetail(app, t);
      const currentIdentity = app.account_switch?.identity;
      const currentIsAccount = managedAccountCount > 0 || Boolean(currentIdentity);
      const currentLabel = active
        ? profileDisplayLabel(active)
        : supportsManagedAccounts
          ? currentIdentity ? currentAccountLabel(app, t) : undefined
          : undefined;
      const currentDetail = active?.identity?.email
        || (active ? profileConnection(active, t) : supportsManagedAccounts ? currentIdentity?.plan || app.account_switch?.provider?.name : undefined);
      return <section className="account-client-section" key={app.name}>
        <header className="account-client-header"><ClientGlyph adapter={app.adapter} large/><div className="account-client-name"><div className="title-line"><h2>{adapterName(app.adapter)}</h2><StatusBadge status={app.status} t={t}/></div><span>{profileCountLabel}</span></div><div className="current-account-summary"><small>{t(currentIsAccount ? "activeAccount" : "activeEnvironment")}</small><strong>{currentLabel || t(accountLikeClient && app.account_switch?.status === "sign_in_required" ? "signedOut" : "setupRequired")}</strong><span>{currentDetail || t(accountLikeClient && app.account_switch?.status === "sign_in_required" ? "chooseSavedAccount" : "setupRequired")}</span></div><div className="account-trust"><span><Icon name="history"/>{t("nativeHistoryKept")}</span><span><Icon name="shield"/>{managedAccountCount > 0 ? (credentialStoreAvailable ? t("credentialStorageReady") : t("credentialStorageUnavailable")) : t("clientOwnedAuth")}</span></div><button className="button primary" disabled={busy} onClick={() => onAddProfile(app.name)}><Icon name="plus"/>{t(supportsManagedAccounts ? "addProfile" : "addEnvironment")}</button></header>
        {supportsManagedAccounts && (!accountSwitchReady || app.account_switch?.status === "sign_in_required") && <div className="inline-warning account-warning"><Icon name="warning"/><span>{capabilityDetail}</span><button className="button secondary small" disabled={busy} onClick={() => void onRefresh()}>{t("recheckAccountStatus")}</button></div>}
        {app.issues.filter((issue) => issue.code !== "no_profiles").length > 0 && <div className="inline-warning account-warning"><Icon name="warning"/><span>{issueLabel(app.issues.find((issue) => issue.code !== "no_profiles")!, t)}</span></div>}
        <div className="saved-accounts-head"><h3>{savedProfilesLabel}</h3><span>{profileCountLabel}</span></div>
        <div className="saved-account-list">{[...app.profiles].sort((left, right) => Number(right.name === app.active) - Number(left.name === app.active)).map((profile) => {
          const isCurrent = profile.name === app.active;
          const accountFlow = isManagedAccountFlow(app, profile.name);
          const supported = profileSwitchSupported(app, profile);
          const switchReady = supported && (!accountFlow || (accountSwitchReady && credentialStoreAvailable));
          const switchTitle = !supported ? t("unsupportedAccountProfileDetail") : !switchReady ? (!credentialStoreAvailable ? t("credentialStorageUnavailableAction") : capabilityDetail) : undefined;
          const displayLabel = profileDisplayLabel(profile);
          const identityDetail = profile.identity?.email || profile.identity?.name || (profile.managed_account ? t("accountIdentity") : profile.provider?.official === false ? t("customProvider") : profileConnection(profile, t));
          const isSwitching = switchingTarget === `${app.name}/${profile.name}`;
          return <article className={isCurrent ? "saved-account-row current" : "saved-account-row"} key={profile.name}><div className="profile-avatar">{displayLabel.slice(0, 1).toUpperCase()}</div><div className="saved-account-copy"><h3>{displayLabel}</h3><span>{identityDetail}</span></div><div className="saved-account-meta"><span>{profileConnection(profile, t)}</span><small><Icon name="history"/>{t("nativeHistoryKept")}</small></div><div className="saved-account-actions">{isCurrent ? <span className="status-badge ready"><span className="status-dot ready"/>{t("current")}</span> : <button className="button primary compact" disabled={busy || !switchReady} title={switchTitle} onClick={() => onSwitch(app.name, profile.name)}>{isSwitching ? <><span className="spinner" aria-hidden="true"/>{t("switchingAccount")}</> : <>{t(accountFlow ? "switchToAccount" : "switchEnvironment")}<Icon name="arrow"/></>}</button>}<details className="account-more"><summary role="button" className="icon-button" aria-label={t(accountFlow ? "moreAccountActions" : "moreActions")} title={t(accountFlow ? "moreAccountActions" : "moreActions")}><Icon name="more"/></summary><div><button onClick={() => onEditProfile(app.name, profile.name)}>{t("edit")}</button><button className="danger-text" onClick={() => onDeleteProfile(app.name, profile.name)}>{t("remove")}</button></div></details></div></article>;
        })}</div>
        {!app.profiles.length && <div className="account-empty"><Icon name={supportsManagedAccounts ? "key" : "layers"}/><div><strong>{t("noProfiles")}</strong><span>{t(supportsManagedAccounts ? "createProfileDetail" : "createEnvironmentDetail")}</span></div></div>}
        <details className="technical"><summary>{t("technicalDetails")}<Icon name="chevron"/></summary><dl><div><dt>{t("nativeDirectory")}</dt><dd className="mono">{compactPath(app.live_dir)}</dd></div><div><dt>{t("command")}</dt><dd className="mono">{app.command || "—"}</dd></div></dl></details>
      </section>;
    })}</div>
    {!snapshot.apps.length && <EmptyState
      icon="terminal"
      title={t("connectClientTitle")}
      detail={t("connectClientDetail")}
      action={<button className="button primary" onClick={onAddClient}><Icon name="plus"/>{t("addClient")}</button>}
    />}
  </div>;
}

function SessionsView({ t, language, sessions, query, client, recovery, hasMore, loading, busy, clients, workspaces, onCopy, onResume, onQuery, onClient, onRecovery, onLoadMore }: { t: Translate; language: Language; sessions: Session[]; query: string; client: string; recovery: string; hasMore: boolean; loading: boolean; busy: boolean; clients: Client[]; workspaces: Workspace[]; onCopy: (value: string | undefined, confirmation?: CopyKey) => void; onResume: (session: Session) => void; onQuery: (value: string) => void; onClient: (value: string) => void; onRecovery: (value: string) => void; onLoadMore: () => void }) {
  const [grouping, setGrouping] = useState<"project" | "time">(() => readPreference("mix.sessions.grouping", ["project", "time"] as const, "project"));
  useEffect(() => {
    try { window.localStorage.setItem("mix.sessions.grouping", grouping); } catch { /* optional preference */ }
  }, [grouping]);
  // Core/Web already filter the complete native catalogs before pagination.
  // Re-filtering the current page here would both hide valid Unicode
  // case-fold matches and make the UI disagree with server-side pagination.
  const filtered = sessions;
  const groups = new Map<string, { name: string; path?: string; sessions: Session[] }>();
  for (const session of filtered) {
    const workspace = workspaces.find((item) => item.id === session.workspace) || workspaces.find((item) => pathContains(item.path, session.cwd));
    const id = session.project_id || session.project_path || workspace?.path || "__unknown__";
    const group = groups.get(id) || { name: session.project_name || workspace?.name || t("unclassifiedSessions"), path: session.project_path || workspace?.path, sessions: [] };
    group.sessions.push(session);
    groups.set(id, group);
  }
  const grouped = [...groups.entries()].sort((left, right) => {
    if (left[0] === "__unknown__") return 1;
    if (right[0] === "__unknown__") return -1;
    return sessionTimestamp(right[1].sessions[0]) - sessionTimestamp(left[1].sessions[0]);
  });
  const renderSession = (session: Session, showProject: boolean) => {
    const workspace = workspaces.find((item) => item.id === session.workspace) || workspaces.find((item) => pathContains(item.path, session.cwd));
    const sessionClient = clients.find((item) => item.name === session.app);
    const boundProfileName = session.profile ? undefined : workspace?.bindings[session.app];
    const boundProfile = sessionClient?.profiles.find((profile) => profile.name === boundProfileName);
    const accountContext = session.profile
      ? undefined
      : boundProfileName
        ? t("sessionUsesProjectAccount", { profile: boundProfile ? profileDisplayLabel(boundProfile) : boundProfileName })
        : t("sessionUsesCurrentAccount");
    const copyValue = session.native_command || session.transcript || session.state_dir;
    const copyLabel = session.native_command ? t("copyResumeCommand") : t("copyPath");
    const level = session.recoverability || "B";
    const title = sessionDisplayTitle(session, t);
    const canResume = level === "A" && Boolean(session.resume_id);
    return <article className="session-row" key={`${session.state_dir}/${session.app}/${session.id}`}><ClientGlyph adapter={sessionClient?.adapter || session.app}/><div className="session-copy"><h3>{title}</h3><div className="session-meta"><span>{adapterName(sessionClient?.adapter || session.app)}</span>{showProject && <span>{session.project_name || workspace?.name || t("unclassifiedSessions")}</span>}{session.profile && <span>{sessionClient?.profiles.find((profile) => profile.name === session.profile) ? profileDisplayLabel(sessionClient.profiles.find((profile) => profile.name === session.profile)) : session.profile}</span>}{session.provider && <span>{session.provider}</span>}{accountContext && <span>{accountContext}</span>}</div><p className="mono">{compactPath(session.cwd || session.state_dir)}</p>{level === "B" && <p className="session-recovery-detail"><Icon name="shield"/>{recoveryReasonLabel(session.recovery_reason, t)}</p>}</div><RecoveryBadge level={level} t={t} verbose/><time>{formatTimestamp(session.updated_at, language)}</time><div className="session-actions">{canResume && <button className="button primary small" disabled={busy} onClick={() => onResume(session)} aria-label={t("continueSessionNamed", { title })}><Icon name="play"/>{t("continueSession")}</button>}<button className="icon-button" disabled={busy} onClick={() => onCopy(copyValue, session.native_command ? "resumeCommandCopied" : "pathCopied")} aria-label={copyLabel} title={copyLabel}><Icon name="copy"/></button></div></article>;
  };
  const flatten = Boolean(query) || grouping === "time";
  return <div className="page"><PageHeader eyebrow={t("sessions")} title={t("sessionsTitle")} detail={t("sessionsDetail")}/>
    <div className="toolbar session-toolbar"><label className="search-field"><Icon name="search"/><input aria-label={t("searchSessions")} value={query} maxLength={512} onChange={(event) => onQuery(event.target.value)} placeholder={t("searchSessions")}/></label><select aria-label={t("filterClient")} value={client} onChange={(event) => onClient(event.target.value)}><option value="all">{t("anyClient")}</option>{[...new Set(clients.map((item) => item.adapter))].map((adapter) => <option key={adapter} value={adapter}>{adapterName(adapter)}</option>)}</select><select aria-label={t("filterRecovery")} value={recovery} onChange={(event) => onRecovery(event.target.value)}><option value="all">{t("anyRecovery")}</option><option value="A">{t("recoveryA")}</option><option value="B">{t("recoveryB")}</option></select><div className="segmented-control" aria-label={t("sessionGrouping")}><button className={grouping === "project" ? "active" : ""} aria-pressed={grouping === "project"} onClick={() => setGrouping("project")}>{t("groupByProject")}</button><button className={grouping === "time" ? "active" : ""} aria-pressed={grouping === "time"} onClick={() => setGrouping("time")}>{t("groupByTime")}</button></div></div>
    {flatten ? <div className="session-list">{filtered.map((session) => renderSession(session, true))}</div> : <div className="session-groups">{grouped.map(([id, group], index) => <details className="session-group" key={id} open={index === 0}><summary><div className="session-group-title"><div className="workspace-icon"><Icon name="folder"/></div><div><strong>{group.name}</strong><span className="mono">{compactPath(group.path)}</span></div></div><span>{t("sessionCount", { count: group.sessions[0]?.project_session_count || group.sessions.length })}</span><Icon name="chevron"/></summary><div className="session-list">{group.sessions.map((session) => renderSession(session, false))}</div></details>)}</div>}
    {loading && !filtered.length && <div className="session-loading" role="status"><span className="spinner"/>{t("loadingSessions")}</div>}
    {!loading && !filtered.length && <EmptyState icon="history" title={t("noSessionsTitle")} detail={t("noSessionsDetail")}/>}
    {(hasMore || sessions.length > 0) && <div className="session-pagination"><span>{t("loadedSessionCount", { count: sessions.length })}</span>{hasMore && <button className="button secondary" disabled={loading} onClick={onLoadMore}><Icon name="refresh"/>{loading ? t("loadingSessions") : t("loadMoreSessions")}</button>}</div>}
  </div>;
}

function ActivityView({ t, language, activity, clients, busy, onSwitchBack }: { t: Translate; language: Language; activity: Activity[]; clients: Client[]; busy: boolean; onSwitchBack: (activity: Activity) => void }) {
  return <div className="page"><PageHeader eyebrow={t("activity")} title={t("activityTitle")} detail={t("activityDetail")}/>
    <div className="timeline">{activity.map((item) => {
      const client = clients.find((candidate) => candidate.name === item.app);
      const canSwitchBack = item.kind === "switch" && Boolean(item.from_profile) && client?.active !== item.from_profile && client?.profiles.some((profile) => profile.name === item.from_profile);
      return <article className="timeline-item" key={item.id || `${item.kind}/${item.at}`}><div className={`timeline-icon ${item.kind}`}><Icon name={item.kind === "run" ? "play" : item.kind === "switch" ? "layers" : "activity"}/></div><div className="timeline-card"><div className="timeline-head"><div><h3>{activityLabel(item.kind, t)}</h3><p>{[item.app, item.from_profile && item.to_profile ? `${item.from_profile} → ${item.to_profile}` : item.profile, compactPath(item.workspace)].filter(Boolean).join(" · ")}</p></div><time>{formatTimestamp(item.at, language)}</time></div><div className="timeline-tags">{item.backup && <span><Icon name="shield"/>{t("backupCreated")}</span>}{item.files && <span><Icon name="layers"/>{item.files.length} {t("files")}</span>}</div>{canSwitchBack && <button className="button secondary small" disabled={busy} onClick={() => onSwitchBack(item)}><Icon name="refresh"/>{t("switchBack")}</button>}</div></article>;
    })}</div>
    {!activity.length && <EmptyState icon="activity" title={t("noActivityTitle")} detail={t("noActivityDetail")}/>}
  </div>;
}

function SettingsView({ t, language, theme, snapshot, serviceError, updaterCapability, automaticUpdateChecks, updatePhase, updateResult, onAutomaticUpdateChecks, onCheckUpdates, onLanguage, onTheme, onRefresh, onDiagnostics, onLicenses, onActivity }: { t: Translate; language: Language; theme: Theme; snapshot: Snapshot; serviceError: string | null; updaterCapability: UpdaterCapability; automaticUpdateChecks: boolean; updatePhase: UpdatePhase; updateResult: UpdateResult; onAutomaticUpdateChecks: (value: boolean) => void; onCheckUpdates: () => void; onLanguage: (value: Language) => void; onTheme: (value: Theme) => void; onRefresh: () => Promise<void>; onDiagnostics: () => Promise<void>; onLicenses: () => Promise<void>; onActivity: () => void }) {
  const desktop = isDesktopShell();
  const credentialStore = credentialStoreFor(snapshot);
  const healthGroups = Object.entries(snapshot.health.issues.reduce<Record<string, Snapshot["health"]["issues"]>>((groups, issue) => {
    (groups[issue.app] ||= []).push(issue);
    return groups;
  }, {}));
  const updateDetail = !desktop ? t("updatesWebBody") : updaterCapability.enabled ? t("updatesEnabledBody") : t("updatesDisabledBody");
  const healthy = !serviceError && snapshot.health.status === "healthy";
  return <div className="page settings-page"><PageHeader eyebrow={t("settings")} title={t("settingsTitle")} detail={t("settingsDetail")}/>
    <section className="settings-section"><div className="settings-label"><h2>{t("general")}</h2></div><div className="settings-card"><SettingRow icon="globe" title={t("language")} detail={language === "zh" ? t("chinese") : t("english")}><Segmented value={language} options={[{ value: "zh", label: t("chinese") }, { value: "en", label: t("english") }]} onChange={(value) => onLanguage(value as Language)}/></SettingRow><SettingRow icon={theme === "dark" ? "moon" : "sun"} title={t("appearance")} detail={t(theme)}><Segmented value={theme} options={[{ value: "system", label: t("system") }, { value: "light", label: t("light") }, { value: "dark", label: t("dark") }]} onChange={(value) => onTheme(value as Theme)}/></SettingRow></div></section>
    <section className="settings-section"><div className="settings-label"><h2>{t("security")}</h2></div><div className="settings-card"><SettingRow icon="shield" title={t("securityTitle")} detail={t("securityBody")}><span className="status-badge ready"><span className="status-dot ready"/>{t("localOnly")}</span></SettingRow><SettingRow icon="key" title={t("credentialStorageTitle")} detail={credentialStore.available ? t("credentialStorageBody", { backend: credentialStore.backend || t("credentialStorageTitle") }) : credentialStoreUnavailableDetail(credentialStore, t)}><span className={credentialStore.available ? "status-badge ready" : "status-badge warning"}><span className={credentialStore.available ? "status-dot ready" : "status-dot warning"}/>{t(credentialStore.available ? "credentialStorageReady" : "credentialStorageUnavailable")}</span></SettingRow><SettingRow icon="settings" title={t("privacyTitle")} detail={t("privacyBody")}><span className="status-badge ready"><span className="status-dot ready"/>{t("telemetryOff")}</span></SettingRow><SettingRow icon="history" title={t("uninstallDataTitle")} detail={t("uninstallDataBody")}><span className="tag">{t("uninstallKeepsHistory")}</span></SettingRow></div></section>
    <section className="settings-section"><div className="settings-label"><h2>{t("healthCenter")}</h2></div><div className="settings-card"><div className="health-summary"><div className={healthy ? "health-orb healthy" : "health-orb attention"}><Icon name={healthy ? "check" : "warning"}/></div><div><h3>{serviceError ? t("localServiceOffline") : healthy ? t("healthy") : t("healthTitle", { count: healthGroups.length })}</h3><p>{serviceError || (healthy ? t("noIssues") : t("healthDetail"))}</p></div><button className="button secondary" onClick={() => void onRefresh()}><Icon name="refresh"/>{t("runChecks")}</button></div>{!serviceError && healthGroups.map(([appName, issues]) => <details className="issue-group" key={appName}><summary><ClientGlyph adapter={snapshot.apps.find((app) => app.name === appName)?.adapter || appName}/><strong>{adapterName(snapshot.apps.find((app) => app.name === appName)?.adapter || appName)}</strong><span>{t("issueCount", { count: issues?.length || 0 })}</span><Icon name="chevron"/></summary><div>{(issues || []).map((issue, index) => <p key={`${appName}/${index}`}>{issueLabel(issue.issue, t)}</p>)}</div></details>)}</div></section>
    <section className="settings-section"><div className="settings-label"><h2>{t("softwareUpdates")}</h2></div><div className="settings-card"><SettingRow icon="refresh" title={t("signedUpdates")} detail={updateDetail}>{updaterCapability.enabled ? <div className="update-controls">{updateResult === "current" && <span className="status-badge ready"><span className="status-dot ready"/>{t("upToDate")}</span>}{updateResult === "available" && <span className="status-badge warning"><span className="status-dot warning"/>{t("updateAvailable")}</span>}{updateResult === "failed" && <span className="status-badge warning"><span className="status-dot warning"/>{t("lastCheckFailed")}</span>}<button className="button secondary" disabled={updatePhase === "checking"} onClick={() => void onCheckUpdates()}><Icon name={updateResult === "available" ? "download" : "refresh"}/>{updatePhase === "checking" ? t("checkingForUpdates") : updateResult === "available" ? t("reviewUpdate") : t("checkForUpdates")}</button></div> : <span className="tag warning">{desktop ? t("localBuild") : t("macAppOnly")}</span>}</SettingRow>{updaterCapability.enabled && <SettingRow icon="settings" title={t("automaticChecks")} detail={t("automaticChecksBody")}><Segmented value={automaticUpdateChecks ? "on" : "off"} options={[{ value: "on", label: t("automatic") }, { value: "off", label: t("manual") }]} onChange={(value) => onAutomaticUpdateChecks(value === "on")}/></SettingRow>}</div></section>
    <section className="settings-section"><div className="settings-label"><h2>{t("support")}</h2></div><div className="settings-card"><SettingRow icon="shield" title={t("installedVersion")} detail={t("installedVersionDetail")}><span className="tag mono">v{__MIX_VERSION__}</span></SettingRow><SettingRow icon="activity" title={t("activityTitle")} detail={t("activityDetail")}><button className="button secondary" onClick={onActivity}><Icon name="activity"/>{t("activity")}</button></SettingRow><SettingRow icon="copy" title={t("diagnosticsTitle")} detail={t("diagnosticsBody")}><button className="button secondary" onClick={() => void onDiagnostics()}><Icon name="copy"/>{t("copyDiagnostics")}</button></SettingRow><SettingRow icon="layers" title={t("thirdPartyLicenses")} detail={t("licenseEvidenceBody")}>{desktop ? <button className="button secondary" onClick={() => void onLicenses()}><Icon name="layers"/>{t("openLicenses")}</button> : <span className="tag">{t("macAppOnly")}</span>}</SettingRow><div className="diagnostic-boundary"><Icon name="shield"/><span>{t("diagnosticsRedaction")}</span></div></div></section>
  </div>;
}

function SettingRow({ icon, title, detail, children }: { icon: IconName; title: string; detail: string; children: ReactNode }) { return <div className="setting-row"><div className="setting-icon"><Icon name={icon}/></div><div><h3>{title}</h3><p>{detail}</p></div><div className="setting-control">{children}</div></div>; }

function Segmented({ value, options, onChange }: { value: string; options: { value: string; label: string }[]; onChange: (value: string) => void }) { return <div className="segmented" role="group">{options.map((option) => <button key={option.value} aria-pressed={value === option.value} className={value === option.value ? "active" : ""} onClick={() => onChange(option.value)}>{option.label}</button>)}</div>; }

function ClientDialog({ t, discovery, onClose, onCreate }: { t: Translate; discovery: DiscoveredClient[]; onClose: () => void; onCreate: (payload: Record<string, unknown>) => Promise<void> }) {
  const available = discovery.find((item) => !item.configured) || discovery[0];
  const [selected, setSelected] = useState(available?.adapter || "");
  const [advanced, setAdvanced] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const current = discovery.find((item) => item.adapter === selected) || available;
  const [liveDir, setLiveDir] = useState(available?.live_dir || "");
  useEffect(() => {
    if (current) setLiveDir(current.live_dir);
  }, [current]);
  async function chooseConfigDirectory() {
    try {
      const selectedPath = await pickDirectory(t("chooseFolder"), liveDir);
      if (selectedPath) setLiveDir(selectedPath);
    } catch {
      setError(t("folderPickerFailed"));
    }
  }
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); setSaving(true); setError(null);
    if (!current) { setError(t("unknownError")); setSaving(false); return; }
    const form = new FormData(event.currentTarget);
    try {
      await onCreate({
        name: current.name, adapter: current.adapter, live_dir: form.get("live_dir"),
      });
    } catch (failure) { setError(localizedFailureMessage(failure, t, t("unknownError"))); setSaving(false); }
  }
  return <Modal t={t} title={t("connectClientTitle")} detail={t("connectClientDetail")} onClose={onClose} closeDisabled={saving}><form key={current?.adapter || "empty"} onSubmit={submit}>
    <div className="choice-grid client-choices">{discovery.map((item) => <button type="button" key={item.adapter} disabled={item.configured} aria-pressed={selected === item.adapter && !item.configured} className={selected === item.adapter ? "choice-card selected" : "choice-card"} onClick={() => setSelected(item.adapter)}><ClientGlyph adapter={item.adapter} large/><div><strong>{item.label}</strong><span>{item.configured ? t("alreadyConnected") : item.installed ? t("detected") : t("notInstalled")}</span></div>{selected === item.adapter && !item.configured && <Icon name="check"/>}<div className="choice-tags">{item.config_exists && <span>{t("existingConfig")}</span>}{item.sessions_detected && <span>{t("existingSessions")}</span>}</div></button>)}</div>
    {current && <div className="form-stack">{isAccountClient(current) && <div className="continuity-option"><Icon name="history"/><div><strong>{t("historyContinuityLabel")}</strong><span>{t("historyContinuityDetail")}</span></div><span className="tag accent">{t("enabled")}</span></div>}{isAccountClient(current) && current.account_switch?.available === false && <div className="inline-warning"><Icon name="warning"/><span>{accountSwitchCapabilityDetail(current, t)}</span></div>}<button type="button" className="disclosure" aria-expanded={advanced} onClick={() => setAdvanced(!advanced)}>{t("advancedSettings")}<Icon name="chevron"/></button>{advanced && <div className="advanced-fields"><label>{t("configDirectory")}<div className="field-with-action"><input name="live_dir" value={liveDir} onChange={(event) => setLiveDir(event.target.value)}/>{isDesktopShell() && <button type="button" className="button secondary" onClick={() => void chooseConfigDirectory()}><Icon name="folder"/>{t("chooseFolder")}</button>}</div></label></div>}{!advanced && <input type="hidden" name="live_dir" value={liveDir}/>}</div>}
    {error && <FormError message={error}/>}<ModalActions t={t} saving={saving} submitLabel={t("connect")} onClose={onClose}/>
  </form></Modal>;
}

function ProfileDialog({ t, apps, initialApp, credentialStoreAvailable, onClose, onCreate, onEnroll }: { t: Translate; apps: Client[]; initialApp?: string; credentialStoreAvailable: boolean; onClose: () => void; onCreate: (path: string, payload: Record<string, unknown>) => Promise<void>; onEnroll: (app: string) => Promise<void> }) {
  const accountNameHintId = useId();
  const initialAppName = initialApp || apps[0]?.name || "";
  const [appName, setAppName] = useState(initialAppName);
  const initialAppState = apps.find((item) => item.name === initialAppName);
  const initialAccountClient = isAccountClient(initialAppState);
  const initialCaptureAvailable = initialAccountClient && credentialStoreAvailable && clientCapabilityAvailable(initialAppState, "identity.capture");
  const initialCurrentAlreadyAdded = Boolean(initialAppState?.account_switch?.current_profile);
  const [mode, setMode] = useState<"current" | "interactive" | "import" | "advanced">(initialAccountClient ? "current" : "interactive");
  const [customize, setCustomize] = useState(!initialAccountClient);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const app = apps.find((item) => item.name === appName);
  const currentAlreadyAdded = Boolean(app?.account_switch?.current_profile);
  const [accountPath, setAccountPath] = useState<"current" | "another">(initialCaptureAvailable && !initialCurrentAlreadyAdded ? "current" : "another");
  const accountFlow = isAccountClient(app);
  const captureAvailable = accountFlow && credentialStoreAvailable && clientCapabilityAvailable(app, "identity.capture") && !currentAlreadyAdded;
  const enrollmentSupported = accountFlow && clientCapabilityAvailable(app, "identity.enroll");
  const enrollmentAvailable = enrollmentSupported && credentialStoreAvailable;
  const importAvailable = !accountFlow && Boolean(app?.import_files.length);
  const defaults = nextProfileDefaults(app, t);
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); setSaving(true); setError(null);
    const form = new FormData(event.currentTarget);
    const accountLabel = String(form.get("label") || defaults.label).trim();
    if (!accountLabel) { setError(t("accountNameRequired")); setSaving(false); return; }
    const common = {
      app: appName,
      name: form.get("name") || defaults.name,
      label: accountLabel,
    };
    try {
      if ((mode === "current" || mode === "advanced") && !credentialStoreAvailable) throw new Error(t("credentialStorageUnavailableAction"));
      if (mode === "current" && !captureAvailable) throw new Error(accountSwitchCapabilityDetail(app, t));
      if (mode === "current") {
        await onCreate("/api/accounts/capture", common);
      } else if (mode === "import") {
        if (!importAvailable) throw new Error(t("importUnavailable"));
        await onCreate("/api/profiles/import", { ...common, files: app?.import_files });
      } else if (mode === "interactive") {
        if (accountFlow) throw new Error(t("accountAddCurrentFirst"));
        await onCreate("/api/profiles", { ...common, files: {}, auth_strategy: "interactive" });
      } else {
        await onCreate("/api/profiles", { ...common, files: parseBindings(String(form.get("files") || ""), t("invalidBinding")), env: parseBindings(String(form.get("env") || ""), t("invalidBinding")), secrets: parseSecretBindings(String(form.get("secret_env") || ""), t("invalidBinding"), t("invalidSecret")), auth_strategy: "local-file" });
      }
    } catch (failure) { setError(localizedFailureMessage(failure, t, t("unknownError"))); setSaving(false); }
  }
  const addingAnotherAccount = accountFlow && accountPath === "another" && !customize;
  return <Modal t={t} title={t(accountFlow ? "createProfileTitle" : "createEnvironmentTitle")} detail={t(accountFlow ? "createProfileDetail" : "createEnvironmentDetail")} onClose={onClose} closeDisabled={saving}><form onSubmit={submit} noValidate>
    {accountFlow && enrollmentSupported && !customize && <div className="account-add-tabs" role="tablist" aria-label={t("addMethod")}><button type="button" role="tab" aria-selected={accountPath === "current"} className={accountPath === "current" ? "active" : ""} disabled={!captureAvailable} onClick={() => setAccountPath("current")}><Icon name="check"/><span><strong>{t("addCurrentAccount")}</strong>{currentAlreadyAdded && <small>{t("currentAccountAlreadyAdded")}</small>}</span></button><button type="button" role="tab" aria-selected={accountPath === "another"} className={accountPath === "another" ? "active" : ""} onClick={() => setAccountPath("another")}><Icon name="key"/><span><strong>{t("enrollAnotherAccount")}</strong><small>{t("nativeClientSignIn")}</small></span></button></div>}
    {!customize && mode === "current" && !addingAnotherAccount && <div className="quick-account-save"><div className="quick-account-identity"><ClientGlyph adapter={app?.adapter || "client"} large/><div><small>{t("detectedCurrentLogin")}</small><strong>{currentAccountLabel(app, t)}</strong><span>{[app?.account_switch?.identity?.plan, app?.account_switch?.identity?.suffix && `ID ${app.account_switch.identity.suffix}`].filter(Boolean).join(" · ") || t("nameCanChangeLater")}</span></div><span className="status-badge ready"><span className="status-dot ready"/>{t("ready")}</span></div><div className="quick-account-name"><label><span>{t("displayName")}</span><input name="label" defaultValue={defaults.label} placeholder={defaults.label} aria-describedby={accountNameHintId} required/></label><small id={accountNameHintId}>{t("accountNameHint")}</small></div><input type="hidden" name="name" value={defaults.name}/></div>}
    {addingAnotherAccount && <div className="another-account-flow"><div className="another-account-icon"><Icon name="key"/></div><h3>{t("enrollAnotherAccount")}</h3><p>{t("enrollAnotherAccountDetail")}</p><div className="login-safety-list"><span><Icon name="check"/>{t("isolatedLoginHome")}</span><span><Icon name="check"/>{t("currentAccountUnaffected")}</span><span><Icon name="check"/>{t("nativeHistoryKept")}</span></div></div>}
    {customize && <><div className="form-grid"><label>{t("client")}<select value={appName} onChange={(event) => { const next = event.target.value; const nextApp = apps.find((item) => item.name === next); const nextCapture = isAccountClient(nextApp) && credentialStoreAvailable && clientCapabilityAvailable(nextApp, "identity.capture"); setAppName(next); setMode(nextCapture ? "current" : "interactive"); }} required>{apps.map((item) => <option key={item.name} value={item.name}>{adapterName(item.adapter)}</option>)}</select></label><label>{t(accountFlow ? "displayName" : "environmentName")}<input name="label" defaultValue={defaults.label}/></label></div>
    <fieldset className="source-mode"><legend>{t("sourceMode")}</legend>{accountFlow ? <SourceChoice active={mode === "current"} disabled={!captureAvailable} icon="key" title={t("addCurrentAccount")} detail={t("addCurrentAccountDetail")} onClick={() => setMode("current")}/> : <><SourceChoice active={mode === "interactive"} icon="play" title={t("signInLater")} detail={t("signInLaterDetail")} onClick={() => setMode("interactive")}/>{importAvailable && <SourceChoice active={mode === "import"} icon="arrow" title={t("copyCurrentConfig")} detail={t("copyCurrentConfigDetail")} onClick={() => setMode("import")}/>}<SourceChoice active={mode === "advanced"} disabled={!credentialStoreAvailable} icon="settings" title={t("advancedReference")} detail={t("advancedReferenceDetail")} onClick={() => setMode("advanced")}/></>}</fieldset></>}
    {!customize && !addingAnotherAccount && <button type="button" className="disclosure account-customize" aria-expanded={customize} onClick={() => setCustomize(true)}>{t("customizeAccount")}<Icon name="chevron"/></button>}
    {!credentialStoreAvailable && (accountFlow || mode === "advanced") && <div className="inline-warning"><Icon name="warning"/><span>{t("credentialStorageUnavailableAction")}</span></div>}
    {accountFlow && credentialStoreAvailable && !captureAvailable && !currentAlreadyAdded && !addingAnotherAccount && <div className="inline-warning"><Icon name="warning"/><span>{accountSwitchCapabilityDetail(app, t)} {t("accountAddCurrentFirst")}</span></div>}
    {mode === "import" && importAvailable && <div className="inline-warning"><Icon name="warning"/><span>{t("profileImportCaution")}</span></div>}
    {mode === "advanced" && <div className="advanced-profile"><label>{t("ordinaryFiles")}<textarea name="files" placeholder="settings.json=/absolute/path/to/settings.json"/></label><label>{t("ordinaryEnv")}<textarea name="env" placeholder="ANTHROPIC_BASE_URL=https://gateway.example"/></label><label>{t("secretEnv")}<textarea name="secret_env" placeholder="ANTHROPIC_API_KEY=mix/provider-company"/></label></div>}
    {mode === "current" && !addingAnotherAccount && <div className="continuity-option"><Icon name="shield"/><div><strong>{t("credentialCaptureTitle")}</strong><span>{t("credentialCaptureDetail")}</span></div><span className="tag accent">{t("credentialStorageReady")}</span></div>}
    {error && <FormError message={error}/>} {addingAnotherAccount ? <div className="modal-actions"><button type="button" className="button secondary" onClick={onClose}>{t("cancel")}</button><button type="button" className="button primary" disabled={!enrollmentAvailable} onClick={() => void onEnroll(appName)}>{t("continueToSignIn")}<Icon name="arrow"/></button></div> : <ModalActions t={t} saving={saving} disabled={mode === "current" && !captureAvailable} submitLabel={mode === "current" ? t("saveAccount") : t("createEnvironment")} onClose={onClose}/>}</form></Modal>;
}

function ProfileEditDialog({ t: baseT, app, profileName, onClose, onSave }: { t: Translate; app?: Client; profileName: string; onClose: () => void; onSave: (payload: Record<string, unknown>) => Promise<void> }) {
  const profile = app?.profiles.find((item) => item.name === profileName);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  if (!app || !profile) return null;
  const accountFlow = isManagedAccountFlow(app, profile.name);
  const t: Translate = (key, values) => baseT(
    !accountFlow && key === "displayName" ? "environmentName"
      : key,
    values,
  );
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); setSaving(true); setError(null);
    const form = new FormData(event.currentTarget);
    try {
      await onSave({ label: form.get("label") });
    } catch (failure) { setError(localizedFailureMessage(failure, t, t("unknownError"))); setSaving(false); }
  }
  return <Modal t={t} title={t(accountFlow ? "editProfileTitle" : "editEnvironmentTitle")} detail={t(accountFlow ? "editProfileDetail" : "editEnvironmentDetail")} onClose={onClose}><form onSubmit={submit}><div className="quick-account-identity"><ClientGlyph adapter={app.adapter} large/><div><small>{adapterName(app.adapter)}</small><strong>{profileDisplayLabel(profile)}</strong><span>{profile.identity?.email || profileConnection(profile, t)}</span></div>{profile.name === app.active && <span className="status-badge ready"><span className="status-dot ready"/>{t("current")}</span>}</div><div className="form-stack"><label>{t("displayName")}<input name="label" defaultValue={profile.label} required autoFocus/></label></div><div className="continuity-option"><Icon name="history"/><div><strong>{t("editProfileSafeTitle")}</strong><span>{t("editProfileSafeDetail")}</span></div></div>{error && <FormError message={error}/>}<ModalActions t={t} saving={saving} submitLabel={t("saveChanges")} onClose={onClose}/></form></Modal>;
}

function ProfileDeleteDialog({ t, app, profileName, workspaces, busy, onClose, onConfirm }: { t: Translate; app?: Client; profileName: string; workspaces: Workspace[]; busy: boolean; onClose: () => void; onConfirm: (replacementProfile?: string) => void }) {
  const profile = app?.profiles.find((item) => item.name === profileName);
  const alternatives = app?.profiles.filter((item) => item.name !== profileName) || [];
  const isActive = app?.active === profileName;
  const [replacement, setReplacement] = useState(alternatives[0]?.name || "");
  const boundWorkspaces = app ? workspaces.filter((workspace) => workspace.bindings[app.name] === profileName) : [];
  if (!app || !profile) return null;
  const accountFlow = isManagedAccountFlow(app, profile.name);
  return <Modal t={t} title={t(accountFlow ? "removeProfileTitle" : "removeEnvironmentTitle")} detail={t(accountFlow ? "removeProfileDetail" : "removeEnvironmentDetail", { profile: profileDisplayLabel(profile) })} onClose={onClose}><div className="delete-summary"><ClientGlyph adapter={app.adapter} large/><div><strong>{profileDisplayLabel(profile)}</strong><span>{profile.identity?.email || profileConnection(profile, t)}</span></div></div>{isActive && alternatives.length > 0 && <label className="remove-replacement"><span>{t("removeActiveReplacement")}</span><select value={replacement} onChange={(event) => setReplacement(event.target.value)}>{alternatives.map((item) => <option value={item.name} key={item.name}>{profileDisplayLabel(item)}</option>)}</select></label>}{isActive && !alternatives.length && <div className="inline-warning"><Icon name="warning"/><span>{t("removeActiveOnly")}</span></div>}{boundWorkspaces.length > 0 && <div className="inline-warning"><Icon name="folder"/><span>{t("removeWorkspaceBindings", { count: boundWorkspaces.length })}</span></div>}<div className="continuity-option"><Icon name="history"/><div><strong>{t("removeKeepsHistory")}</strong><span>{t("removeProfileImpact")}</span></div></div><div className="modal-actions"><button className="button secondary" onClick={onClose}>{t("cancel")}</button><button className="button danger" disabled={busy || (isActive && alternatives.length > 0 && !replacement)} onClick={() => onConfirm(isActive && alternatives.length > 0 ? replacement : undefined)}>{t("confirmRemove")}</button></div></Modal>;
}

function SourceChoice({ active, disabled = false, icon, title, detail, onClick }: { active: boolean; disabled?: boolean; icon: IconName; title: string; detail: string; onClick: () => void }) { return <button type="button" disabled={disabled} aria-pressed={active} className={active ? "source-choice active" : "source-choice"} onClick={onClick}><span><Icon name={icon}/></span><div><strong>{title}</strong><p>{detail}</p></div><span className="radio">{active && <span/>}</span></button>; }

function WorkspaceDialog({ t, apps, initial, onClose, onCreate }: { t: Translate; apps: Client[]; initial?: Workspace; onClose: () => void; onCreate: (payload: Record<string, unknown>) => Promise<void> }) {
  const [saving, setSaving] = useState(false); const [error, setError] = useState<string | null>(null);
  const [name, setName] = useState(initial?.name || "");
  const [projectPath, setProjectPath] = useState(initial?.path || "");
  const [customize, setCustomize] = useState(Boolean(initial));
  async function chooseProjectDirectory() {
    try {
      const selectedPath = await pickDirectory(t("chooseProjectFolder"), projectPath);
      if (!selectedPath) return;
      setProjectPath(selectedPath);
      if (!name.trim()) setName(selectedPath.replace(/\/+$/, "").split("/").pop() || "workspace");
    } catch {
      setError(t("folderPickerFailed"));
    }
  }
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); setSaving(true); setError(null); const form = new FormData(event.currentTarget); const bindings: Record<string, string> = {};
    if (!projectPath.trim()) { setError(t("chooseProjectRequired")); setSaving(false); return; }
    apps.forEach((app) => {
      const selected = customize ? String(form.get(`binding.${app.name}`) || "") : (app.active || app.profiles[0]?.name || "");
      if (selected) bindings[app.name] = selected;
    });
    const derivedName = projectPath.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "workspace";
    try { await onCreate({ name: customize ? (form.get("name") || derivedName) : derivedName, path: projectPath, bindings }); } catch (failure) { setError(localizedFailureMessage(failure, t, t("unknownError"))); setSaving(false); }
  }
  return <Modal t={t} title={t(initial ? "editWorkspaceTitle" : "simpleWorkspaceTitle")} detail={t(initial ? "editWorkspaceDetail" : "simpleWorkspaceDetail")} onClose={onClose}><form onSubmit={submit} noValidate><div className="form-stack"><label>{t("projectPath")}<div className="field-with-action"><input name="path" value={projectPath} disabled={Boolean(initial)} onChange={(event) => { setProjectPath(event.target.value); if (!name.trim()) setName(event.target.value.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || ""); }} placeholder="~/work/payment-service"/>{!initial && isDesktopShell() && <button type="button" className="button secondary" onClick={() => void chooseProjectDirectory()}><Icon name="folder"/>{t("chooseProjectFolder")}</button>}</div></label>{!initial && <div className="auto-binding-note"><Icon name="layers"/><div><strong>{t("automaticBinding")}</strong><span>{t("automaticBindingDetail")}</span></div></div>}{!initial && <button type="button" className="disclosure" aria-expanded={customize} onClick={() => setCustomize(!customize)}>{t("customizeWorkspace")}<Icon name="chevron"/></button>}</div>{customize && <><div className="form-grid workspace-customize"><label>{t("workspaceName")}<input name="name" value={name} onChange={(event) => setName(event.target.value)} placeholder="payment-service"/></label></div><div className="binding-choices">{apps.map((app) => <label key={app.name}><ClientGlyph adapter={app.adapter}/><div><strong>{t("useClient", { client: adapterName(app.adapter) })}</strong><span>{app.profiles.length ? t("profiles") : t("noProfiles")}</span></div><select aria-label={t("useClient", { client: adapterName(app.adapter) })} name={`binding.${app.name}`} disabled={!app.profiles.length} defaultValue={initial?.bindings[app.name] || app.active || app.profiles[0]?.name || ""}><option value="">{t("doNotUse")}</option>{app.profiles.map((profile) => <option key={profile.name} value={profile.name}>{profileDisplayLabel(profile)}</option>)}</select></label>)}</div></>}{error && <FormError message={error}/>}<ModalActions t={t} saving={saving} submitLabel={t(initial ? "saveChanges" : "addProject")} onClose={onClose}/></form></Modal>;
}

function WorkspaceDeleteDialog({ t, workspace, busy, onClose, onConfirm }: { t: Translate; workspace?: Workspace; busy: boolean; onClose: () => void; onConfirm: () => void }) {
  if (!workspace) return null;
  return <Modal t={t} title={t("removeWorkspaceTitle")} detail={t("removeWorkspaceDetail", { workspace: workspace.name })} onClose={onClose} closeDisabled={busy}><div className="delete-summary"><div className="workspace-icon"><Icon name="folder"/></div><div><strong>{workspace.name}</strong><span className="mono">{compactPath(workspace.path)}</span></div></div><div className="continuity-option"><Icon name="history"/><div><strong>{t("removeKeepsHistory")}</strong><span>{t("removeWorkspaceKeepsSessions")}</span></div></div><div className="modal-actions"><button className="button secondary" disabled={busy} onClick={onClose}>{t("cancel")}</button><button className="button danger" disabled={busy} onClick={onConfirm}><Icon name="trash"/>{t("confirmRemove")}</button></div></Modal>;
}

function SwitchBackDialog({ t, activity, busy, onClose, onConfirm }: { t: Translate; activity: Activity; busy: boolean; onClose: () => void; onConfirm: () => void }) { return <Modal t={t} title={t("switchBackTitle")} detail={t("switchBackDetail", { profile: activity.from_profile || "—" })} onClose={onClose} closeDisabled={busy}><div className="switch-back-summary"><Icon name="refresh"/><div><strong>{activity.app}</strong><span>{activity.to_profile} → {activity.from_profile}</span></div></div><div className="inline-warning"><Icon name="warning"/><span>{t("switchBackImpact")}</span></div><div className="modal-actions"><button className="button secondary" disabled={busy} onClick={onClose}>{t("cancel")}</button><button className="button danger" disabled={busy} onClick={onConfirm}><Icon name="refresh"/>{t("confirmSwitchBack")}</button></div></Modal>; }

function UpdateDialog({ t, update, onClose, onDownload }: { t: Translate; update: Update; onClose: () => void; onDownload: () => Promise<void> }) {
  return <Modal t={t} title={t("updateAvailableTitle")} detail={t("updateAvailableDetail", { version: update.version })} onClose={onClose}>
    <div className="update-route"><div><small>{t("currentVersion")}</small><strong>{update.currentVersion}</strong></div><Icon name="arrow"/><div><small>{t("newVersion")}</small><strong>{update.version}</strong></div></div>
    {update.body && <section className="update-notes"><h3>{t("releaseNotes")}</h3><p>{update.body.slice(0, 4000)}</p></section>}
    <div className="continuity-note"><Icon name="shield"/><span>{t("manualUpdateSafetyBody")}</span></div>
    <div className="modal-actions"><button className="button secondary" onClick={onClose}>{t("later")}</button><button className="button primary" onClick={() => void onDownload()}><Icon name="download"/>{t("openReleasePage")}</button></div>
  </Modal>;
}

function CommandPalette({ t, language, theme, onClose, onView, onDialog, onLanguage, onTheme }: { t: Translate; language: Language; theme: Theme; onClose: () => void; onView: (view: View) => void; onDialog: (dialog: DialogState) => void; onLanguage: (value: Language) => void; onTheme: (value: Theme) => void }) {
  const [query, setQuery] = useState(""); const input = useRef<HTMLInputElement>(null); const palette = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const restoreShell = isolateAppShell();
    input.current?.focus();
    const trap = (event: KeyboardEvent) => {
      if (event.key !== "Tab") return;
      const items = Array.from(palette.current?.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])') || []);
      if (!items.length) return;
      const first = items[0]; const last = items[items.length - 1];
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
    };
    palette.current?.addEventListener("keydown", trap);
    return () => { palette.current?.removeEventListener("keydown", trap); restoreShell(); previous?.focus(); };
  }, []);
  const actions: { label: string; detail: string; icon: IconName; run: () => void }[] = [
    ...(["environments", "sessions", "workspaces", "activity", "settings"] as View[]).map((view) => ({ label: t(view), detail: t("navigateTo"), icon: view === "workspaces" ? "folder" as IconName : view === "environments" ? "user" as IconName : view === "sessions" ? "history" as IconName : view === "activity" ? "activity" as IconName : "settings" as IconName, run: () => onView(view) })),
    { label: t("addWorkspace"), detail: t("createNew"), icon: "plus", run: () => onDialog({ kind: "workspace" }) },
    { label: t("addClient"), detail: t("createNew"), icon: "terminal", run: () => onDialog({ kind: "client" }) },
    { label: t("switchLanguage"), detail: language === "zh" ? "English" : "中文", icon: "globe", run: () => onLanguage(language === "zh" ? "en" : "zh") },
    { label: t("switchTheme"), detail: t(theme), icon: theme === "dark" ? "moon" : "sun", run: () => onTheme(theme === "system" ? "light" : theme === "light" ? "dark" : "system") },
  ];
  const filtered = actions.filter((action) => `${action.label} ${action.detail}`.toLowerCase().includes(query.toLowerCase())).slice(0, 10);
  function execute(action: typeof actions[number]) { action.run(); onClose(); }
  return <div className="palette-backdrop" onMouseDown={(event) => event.target === event.currentTarget && onClose()}><div ref={palette} className="palette" role="dialog" aria-modal="true" aria-label={t("paletteTitle")}><div className="palette-input"><Icon name="search"/><input ref={input} aria-label={t("palettePlaceholder")} value={query} onChange={(event) => setQuery(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter" && filtered[0]) execute(filtered[0]); }} placeholder={t("palettePlaceholder")}/><kbd>esc</kbd></div><div className="palette-results">{filtered.map((action, index) => <button key={`${action.label}/${index}`} onClick={() => execute(action)}><span className="palette-icon"><Icon name={action.icon}/></span><div><strong>{action.label}</strong><span>{action.detail}</span></div>{index === 0 && query && <kbd>↵</kbd>}</button>)}{!filtered.length && <MiniEmpty icon="search" text={t("noCommands")}/>}</div></div></div>;
}

function Modal({ t, title, detail, onClose, closeDisabled = false, children }: { t: Translate; title: string; detail: string; onClose: () => void; closeDisabled?: boolean; children: ReactNode }) {
  const modal = useRef<HTMLElement>(null);
  const titleId = useId();
  const detailId = useId();
  useEffect(() => {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const root = modal.current;
    const restoreShell = isolateAppShell();
    const focusable = () => Array.from(root?.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])') || []);
    focusable()[0]?.focus();
    const trap = (event: KeyboardEvent) => {
      if (event.key !== "Tab") return;
      const items = focusable();
      if (!items.length) return;
      const first = items[0]; const last = items[items.length - 1];
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
    };
    root?.addEventListener("keydown", trap);
    return () => { root?.removeEventListener("keydown", trap); restoreShell(); previous?.focus(); };
  }, []);
  return <div className="modal-backdrop" onMouseDown={(event) => event.target === event.currentTarget && !closeDisabled && onClose()}><section ref={modal} className="modal" role="dialog" aria-modal="true" aria-labelledby={titleId} aria-describedby={detailId}><div className="modal-head"><div><h2 id={titleId}>{title}</h2><p id={detailId}>{detail}</p></div><button className="icon-button" disabled={closeDisabled} onClick={onClose} aria-label={t("close")}>×</button></div><div className="modal-body">{children}</div></section></div>;
}

function isolateAppShell(): () => void {
  const shell = document.querySelector<HTMLElement>(".app-shell");
  if (!shell) return () => undefined;
  const hadInert = shell.hasAttribute("inert");
  const previousAriaHidden = shell.getAttribute("aria-hidden");
  shell.setAttribute("inert", "");
  shell.setAttribute("aria-hidden", "true");
  return () => {
    if (!hadInert) shell.removeAttribute("inert");
    if (previousAriaHidden === null) shell.removeAttribute("aria-hidden");
    else shell.setAttribute("aria-hidden", previousAriaHidden);
  };
}

function ModalActions({ t, saving, disabled = false, submitLabel, onClose }: { t: Translate; saving: boolean; disabled?: boolean; submitLabel: string; onClose: () => void }) { return <div className="modal-actions"><button type="button" className="button secondary" disabled={saving} onClick={onClose}>{t("cancel")}</button><button className="button primary" disabled={saving || disabled}>{saving ? t("saving") : submitLabel}<Icon name="arrow"/></button></div>; }
function FormError({ message }: { message: string }) { return <div className="form-error" role="alert"><Icon name="warning"/><span>{message}</span></div>; }

function ClientGlyph({ adapter, large = false }: { adapter: string; large?: boolean }) { return <div aria-hidden="true" className={`client-glyph ${adapter} ${large ? "large" : ""}`}>{adapter === "codex" ? "C" : adapter === "claude" ? "A" : adapter.slice(0, 1).toUpperCase()}</div>; }
function StatusBadge({ status, t }: { status: Client["status"]; t: Translate }) { return <span className={`status-badge ${status}`}><span className={`status-dot ${status}`}/>{statusCopy(status, t)}</span>; }
function RecoveryBadge({ level, t, verbose = false }: { level: "A" | "B"; t: Translate; verbose?: boolean }) { return <span className={`recovery-badge level-${level.toLowerCase()}`} title={t(level === "A" ? "recoveryAHelp" : "recoveryBHelp")}>{t(level === "A" ? verbose ? "recoveryA" : "recoveryAShort" : verbose ? "recoveryB" : "recoveryBShort")}</span>; }
function EmptyState({ icon, title, detail, action }: { icon: IconName; title: string; detail: string; action?: ReactNode }) { return <div className="empty-state"><span className="empty-icon"><Icon name={icon}/></span><h2>{title}</h2><p>{detail}</p>{action}</div>; }
function MiniEmpty({ icon, text }: { icon: IconName; text: string }) { return <div className="mini-empty"><Icon name={icon}/><span>{text}</span></div>; }
function LoadingView() { return <div className="loading-view"><div className="skeleton title"/><div className="skeleton hero"/><div className="skeleton row"/><div className="two-column"><div className="skeleton panel"/><div className="skeleton panel"/></div></div>; }
