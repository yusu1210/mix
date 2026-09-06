import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import axe from "axe-core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App, { buildProjects } from "./App";
import { ApiError } from "./lib/api";
import type { Session, Snapshot } from "./lib/types";

const mocks = vi.hoisted(() => ({
  request: vi.fn(),
  writeClipboardText: vi.fn(),
  openLatestRelease: vi.fn(),
  openThirdPartyLicenses: vi.fn(),
  isDesktopShell: vi.fn().mockReturnValue(false),
  listenTrayActions: vi.fn().mockResolvedValue(() => undefined),
  updateTrayState: vi.fn().mockResolvedValue(undefined),
  checkForUpdate: vi.fn(),
}));

vi.mock("./lib/api", async (importOriginal) => ({
  ...await importOriginal<typeof import("./lib/api")>(),
  request: mocks.request,
}));

vi.mock("./lib/native", () => ({
  getUpdaterCapability: vi.fn().mockResolvedValue({ enabled: false, automatic_checks: false }),
  isDesktopShell: mocks.isDesktopShell,
  listenTrayActions: mocks.listenTrayActions,
  pickDirectory: vi.fn().mockResolvedValue(null),
  openLatestRelease: mocks.openLatestRelease,
  openThirdPartyLicenses: mocks.openThirdPartyLicenses,
  updateTrayState: mocks.updateTrayState,
  writeClipboardText: mocks.writeClipboardText,
}));

vi.mock("@tauri-apps/plugin-updater", () => ({ check: mocks.checkForUpdate }));

const snapshot: Snapshot = {
  apps: [{
    name: "codex",
    adapter: "codex",
    profile_category: "account",
    live_dir: "/Users/test/.codex",
    active: "personal",
    status: "ready",
    command: "codex",
    command_found: true,
    issues: [],
    capabilities: {
      "identity.capture": { available: true, support: "native_files_readonly", authority: "credential_projection" },
      "identity.enroll": { available: true, support: "native_cli", authority: "credential_projection" },
      "project.open_native": { available: true, support: "native_cli", authority: "launch" },
      "process.control": { available: true, support: "configured", authority: "launch" },
    },
    import_files: [],
    account_switch: {
      available: true,
      status: "ready",
      storage: "file",
      current_profile: "personal",
      identity: { email: "personal@example.com", plan: "personal", suffix: "A1B2C3" },
    },
    profiles: [
      { name: "personal", label: "Personal", managed_account: true, has_credentials: true, auth_strategy: "local-file", file_count: 1, secret_count: 1 },
      { name: "team", label: "Team", managed_account: true, has_credentials: true, auth_strategy: "local-file", file_count: 1, secret_count: 1 },
    ],
  }],
  workspaces: [{ id: "/Users/test/payments", name: "Payments", path: "/Users/test/payments", bindings: { codex: "team" }, exists: true }],
  activity: [],
  needs_setup: false,
  health: { status: "healthy", issues: [], local_only: true },
  security: {
    credential_store: { supported: true, available: true, backend: "Mix private files", kind: "local-files", reason: null },
  },
  recovery: { interrupted_switch: { required: false } },
};

const sessions: Session[] = [{
  id: "session-1",
  title: "Continue payments",
  app: "codex",
  profile: "team",
  workspace: "payments",
  cwd: "/Users/test/payments",
  state_dir: "/Users/test/.codex/sessions",
  native_command: "codex resume session-1",
  resume_id: `mix-session-v1-${"a".repeat(64)}`,
  recoverability: "A",
  recovery_reason: "verified",
  updated_at: "2026-08-31T02:00:00Z",
}];

function installApiFixture() {
  mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
    if (path === "/api/state") return structuredClone(snapshot);
    if (path === "/api/discovery") return [];
    if (path === "/api/sessions/resume") return { status: "terminal_opened" };
    if (path.startsWith("/api/sessions")) return structuredClone(sessions);
    if (path === "/api/switch") {
      const request = JSON.parse(String(init?.body));
      return { status: request.profile === "personal" ? "already_active" : "switched" };
    }
    if (path === "/api/profiles" && (init?.method === "PATCH" || init?.method === "DELETE")) return {};
    throw new Error(`Unexpected API path: ${path}`);
  });
}

async function waitForAccountsPage() {
  const navigation = await screen.findByRole("button", { name: "账号" });
  if (!screen.queryByRole("heading", { name: "账号", level: 1 })) fireEvent.click(navigation);
  return screen.findByRole("heading", { name: "账号", level: 1 });
}

beforeEach(() => {
  mocks.isDesktopShell.mockReturnValue(false);
  mocks.listenTrayActions.mockResolvedValue(() => undefined);
  mocks.updateTrayState.mockResolvedValue(undefined);
  const values = new Map<string, string>();
  Object.defineProperty(window, "localStorage", {
    configurable: true,
    value: {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, String(value)),
      removeItem: (key: string) => values.delete(key),
      clear: () => values.clear(),
      key: (index: number) => [...values.keys()][index] ?? null,
      get length() { return values.size; },
    } satisfies Storage,
  });
  installApiFixture();
});

afterEach(() => cleanup());

describe("Mix product shell", () => {
  it("counts a server-reported project total once across catalog rows", () => {
    const rows = [
      { ...structuredClone(sessions[0]), id: "session-1", project_id: "/Users/test/payments", project_path: "/Users/test/payments", project_session_count: 2 },
      { ...structuredClone(sessions[0]), id: "session-2", project_id: "/Users/test/payments", project_path: "/Users/test/payments", project_session_count: 2 },
    ];

    expect(buildProjects([], rows)[0].sessionCount).toBe(2);
  });

  it("explains how to recover an expired Local Web connection", async () => {
    mocks.request.mockRejectedValue(new ApiError(
      "local service authentication failed",
      "MIX_AUTH_REQUIRED",
    ));

    render(<App/>);

    expect(await screen.findByRole("alert")).toHaveProperty(
      "textContent",
      expect.stringContaining("浏览器请重新运行 mix web 并使用新链接"),
    );
  });

  it("hides add-client when discovery has no unconnected client", async () => {
    render(<App/>);
    await waitForAccountsPage();
    expect(screen.queryByRole("button", { name: "添加客户端" })).toBeNull();
  });

  it("does not present a stale saved account as current when the live Codex login is unmanaged", async () => {
    const unmanaged = structuredClone(snapshot);
    unmanaged.apps[0].active = null;
    unmanaged.apps[0].configured_active = "personal";
    unmanaged.apps[0].account_switch!.current_profile = null;
    unmanaged.apps[0].account_switch!.identity = {
      name: "Outside account",
      email: "outside@example.com",
      suffix: "A1B2C3",
    };
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(unmanaged);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();

    expect(screen.getAllByText(/Codex 当前登录尚未添加到 Mix/).length).toBeGreaterThan(0);
    expect(screen.getByRole("button", { name: "查看和切换 Codex 当前账号" })).toHaveProperty("textContent", expect.stringContaining("outside@example.com"));
    expect(screen.getAllByRole("button", { name: "切换到此账号" }).every((button) => (button as HTMLButtonElement).disabled)).toBe(true);
    expect((screen.getAllByRole("button", { name: "添加账号" })[0] as HTMLButtonElement).disabled).toBe(false);
    expect(screen.queryByText("Personal", { selector: ".account-current strong" })).toBeNull();
  });

  it("starts with a zero-typing Codex account add and keeps workspaces optional", async () => {
    const firstRun = structuredClone(snapshot);
    firstRun.apps[0].profiles = [];
    firstRun.apps[0].active = null;
    firstRun.apps[0].account_switch!.current_profile = null;
    firstRun.workspaces = [];
    firstRun.needs_setup = true;
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(firstRun);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/accounts/capture") return { profile: "account-1" };
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    expect(screen.queryByText("绑定项目")).toBeNull();
    await user.click(screen.getAllByRole("button", { name: "添加账号" })[0]);

    const dialog = await screen.findByRole("dialog", { name: "添加账号" });
    const accountName = within(dialog).getByRole("textbox", { name: "账号名称" });
    expect((accountName as HTMLInputElement).value).toBe("personal@example.com");
    expect(within(dialog).queryByRole("combobox")).toBeNull();
    await user.click(within(dialog).getByRole("button", { name: "添加账号" }));

    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/accounts/capture")).toBe(true));
    const capture = mocks.request.mock.calls.find(([path]) => path === "/api/accounts/capture");
    expect(JSON.parse(String((capture?.[1] as RequestInit | undefined)?.body))).toEqual({
      app: "codex",
      name: "account-1",
      label: "personal@example.com",
    });
  });

  it("keeps Codex primary and gives Claude environment-specific onboarding", async () => {
    const clients = structuredClone(snapshot);
    clients.apps.push({
      name: "claude",
      adapter: "claude",
      profile_category: "environment",
      live_dir: "/Users/test/.claude",
      active: null,
      status: "setup_required",
      command: "claude",
      command_found: true,
      issues: [{ code: "no_profiles" }],
      capabilities: {},
      import_files: ["settings.json"],
      profiles: [],
    });
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(clients);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();

    expect(screen.getAllByRole("heading", { level: 2 }).map((heading) => heading.textContent)).toEqual(["Codex", "Claude Code"]);
    expect(screen.getByText("为客户端添加一套独立配置。客户端原生登录和会话历史仍由客户端管理。")).toBeTruthy();
    expect(screen.queryByText("已检测到 Codex。先添加当前登录，之后选择账号即可切换。")).toBeNull();
  });

  it("describes Claude environment selection without account-switch language", async () => {
    const clients = structuredClone(snapshot);
    const claude = clients.apps[0];
    claude.name = "claude";
    claude.adapter = "claude";
    claude.profile_category = "environment";
    claude.active = "environment-1";
    claude.account_switch = { available: false, status: "unsupported" };
    claude.capabilities = {};
    claude.profiles = [
      { ...claude.profiles[0], name: "environment-1", label: "Work", managed_account: false, has_credentials: false, auth_strategy: "interactive" },
      { ...claude.profiles[1], name: "environment-2", label: "Personal", managed_account: false, has_credentials: false, auth_strategy: "interactive" },
    ];
    clients.workspaces = [];
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(clients);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch") {
        expect(JSON.parse(String(init?.body))).toEqual({ app: "claude", profile: "environment-2" });
        return { status: "selected" };
      }
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    const personal = screen.getAllByRole("article").find((row) => row.textContent?.includes("Personal"));
    await user.click(within(personal as HTMLElement).getByRole("button", { name: "切换环境" }));

    expect(await screen.findByRole("status")).toHaveProperty("textContent", "已选择默认环境");
  });

  it("imports adapter-declared environment files without client-name branches", async () => {
    const generic = structuredClone(snapshot);
    const client = generic.apps[0];
    client.name = "agent";
    client.adapter = "nova";
    client.profile_category = "environment";
    client.active = null;
    client.account_switch = { available: false, status: "unsupported" };
    client.capabilities = {};
    client.import_files = ["preferences.yaml", "rules.json"];
    client.profiles = [];
    generic.workspaces = [];
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(generic);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/profiles/import" && init?.method === "POST") return { status: "created" };
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    await user.click(screen.getByRole("button", { name: "添加环境" }));
    const dialog = await screen.findByRole("dialog", { name: "添加环境" });
    await user.click(within(dialog).getByRole("button", { name: /复制当前配置/ }));
    expect(within(dialog).getByText(/仅复制此客户端明确允许/)).toBeTruthy();
    await user.click(within(dialog).getByRole("button", { name: "添加" }));

    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/profiles/import")).toBe(true));
    const request = mocks.request.mock.calls.find(([path]) => path === "/api/profiles/import");
    expect(JSON.parse(String((request?.[1] as RequestInit).body))).toMatchObject({
      app: "agent",
      files: ["preferences.yaml", "rules.json"],
    });
  });

  it("opens returning users on the account-first home while keeping projects one click away", async () => {
    const returning = structuredClone(snapshot);
    returning.workspaces[0].bindings = { codex: "personal" };
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return returning;
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return structuredClone(sessions);
      if (path === "/api/sessions/resume") return { status: "terminal_opened" };
      throw new Error(`Unexpected API path: ${path}`);
    });
    const user = userEvent.setup();
    render(<App/>);

    expect(await screen.findByRole("heading", { name: "账号", level: 1 })).toBeTruthy();
    expect(screen.getByRole("button", { name: "查看和切换 Codex 当前账号" })).toHaveProperty("textContent", expect.stringContaining("Personal"));
    await user.click(screen.getByRole("button", { name: "项目" }));
    expect(await screen.findByRole("heading", { name: "项目", level: 1 })).toBeTruthy();
    expect(screen.getByText("项目无需手工维护")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "打开最近会话" }));
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/sessions/resume")).toBe(true));
    const resume = mocks.request.mock.calls.find(([path]) => path === "/api/sessions/resume");
    expect(JSON.parse(String((resume?.[1] as RequestInit).body))).toEqual({ app: "codex", resume_id: sessions[0].resume_id });

    await user.click(screen.getByRole("button", { name: "查看和切换 Codex 当前账号" }));
    expect(await screen.findByRole("heading", { name: "账号", level: 1 })).toBeTruthy();
  });

  it("groups sessions by project by default and offers a chronological view", async () => {
    const user = userEvent.setup();
    render(<App/>);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await user.click(screen.getByRole("button", { name: "会话" }));

    await screen.findByRole("heading", { name: "会话", level: 1 });
    expect(screen.getByRole("button", { name: "按项目" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByText("Payments")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "按时间" }));
    expect(screen.getByRole("button", { name: "按时间" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByText("Continue payments")).toBeTruthy();
  });

  it("uses a localized title when native metadata has no real user request", async () => {
    const unnamed = [{ ...structuredClone(sessions[0]), id: "opaque-session-id", title: "opaque-session-id" }];
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return unnamed;
      throw new Error(`Unexpected API path: ${path}`);
    });
    const user = userEvent.setup();
    render(<App/>);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await user.click(screen.getByRole("button", { name: "会话" }));

    expect(await screen.findByText("未命名会话")).toBeTruthy();
    expect(screen.queryByText("opaque-session-id")).toBeNull();
  });

  it("keeps activity and recovery reachable from settings", async () => {
    const user = userEvent.setup();
    render(<App/>);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await user.click(screen.getByRole("button", { name: "设置" }));
    const settings = await screen.findByRole("heading", { name: "偏好与安全", level: 1 });
    expect(settings).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "活动与恢复" }));
    expect(
      await screen.findByRole("heading", { name: "可解释、可恢复的本机操作", level: 1 }),
    ).toBeTruthy();
  });

  it("paginates the native catalog and searches beyond the first session page", async () => {
    const firstPage = Array.from({ length: 200 }, (_, index): Session => ({
      ...structuredClone(sessions[0]),
      id: `session-${index}`,
      title: `Recent session ${index}`,
      resume_id: `mix-session-v1-${index.toString(16).padStart(64, "0")}`,
    }));
    const archived: Session = {
      ...structuredClone(sessions[0]),
      id: "archived-session",
      title: "Archived Straße customer migration",
      resume_id: `mix-session-v1-${"f".repeat(64)}`,
    };
    const pageProbe: Session = {
      ...structuredClone(sessions[0]),
      id: "page-probe",
      title: "Hidden page probe",
      resume_id: `mix-session-v1-${"e".repeat(64)}`,
    };
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.includes("query=Archived%20STRASSE%20customer")) return [structuredClone(archived)];
      if (path.includes("offset=200")) return [structuredClone(archived)];
      if (path.startsWith("/api/sessions")) return structuredClone([...firstPage, pageProbe]);
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await user.click(screen.getByRole("button", { name: "会话" }));
    expect(await screen.findByText("已加载 200 个会话")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "加载更多会话" }));
    expect(await screen.findByText("已加载 201 个会话")).toBeTruthy();
    expect(screen.getByText("Archived Straße customer migration")).toBeTruthy();

    const search = screen.getByRole("textbox", { name: "搜索标题、项目、账号或路径" });
    await user.clear(search);
    await user.type(search, "Archived STRASSE customer");
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => String(path).includes("query=Archived%20STRASSE%20customer"))).toBe(true));
    expect(await screen.findByText("已加载 1 个会话")).toBeTruthy();
    expect(screen.getByText("Archived Straße customer migration")).toBeTruthy();
  });

  it("applies client and recovery filters on the complete server catalog", async () => {
    const filteredSnapshot = structuredClone(snapshot);
    filteredSnapshot.apps.push({
      ...structuredClone(snapshot.apps[0]),
      name: "claude",
      adapter: "claude",
      profile_category: "environment",
      active: null,
      account_switch: { available: false, status: "unsupported" },
      capabilities: {},
      profiles: [],
    });
    const claudeOnly: Session = {
      ...structuredClone(sessions[0]),
      app: "claude",
      id: "claude-older",
      title: "Claude result beyond the global first page",
      recoverability: "B",
      resume_id: undefined,
    };
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(filteredSnapshot);
      if (path === "/api/discovery") return [];
      if (path.includes("adapter=claude") && path.includes("recovery=B")) return [structuredClone(claudeOnly)];
      if (path.includes("adapter=claude")) return [structuredClone(claudeOnly)];
      if (path.startsWith("/api/sessions")) return [];
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await user.click(screen.getByRole("button", { name: "会话" }));
    await user.selectOptions(screen.getByRole("combobox", { name: "按客户端筛选" }), "claude");
    expect(await screen.findByText("Claude result beyond the global first page")).toBeTruthy();
    await user.selectOptions(screen.getByRole("combobox", { name: "按可用状态筛选" }), "B");
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => String(path).includes("adapter=claude") && String(path).includes("recovery=B"))).toBe(true));
    expect(screen.getByText("已加载 1 个会话")).toBeTruthy();
  });

  it("refreshes sessions with the current page context after starting on accounts", async () => {
    const fullPage = Array.from({ length: 200 }, (_, index): Session => ({
      ...structuredClone(sessions[0]),
      id: `focus-session-${index}`,
      title: `Focus session ${index}`,
    }));
    const probe: Session = { ...structuredClone(sessions[0]), id: "focus-probe", title: "Focus probe" };
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.includes("limit=25")) return structuredClone([...fullPage.slice(0, 24), probe]);
      if (path.includes("limit=201")) return structuredClone([...fullPage, probe]);
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await user.click(screen.getByRole("button", { name: "会话" }));
    expect(await screen.findByText("已加载 200 个会话")).toBeTruthy();
    mocks.request.mockClear();

    fireEvent.focus(window);
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => String(path).includes("limit=201"))).toBe(true));
    expect(screen.getByText("已加载 200 个会话")).toBeTruthy();
    expect(mocks.request.mock.calls.some(([path]) => String(path).includes("limit=25"))).toBe(false);
  });

  it("turns an automatically discovered project into a usable new-task entry", async () => {
    const discovered = structuredClone(snapshot);
    discovered.workspaces = [];
    const discoveredSessions: Session[] = [{
      ...structuredClone(sessions[0]),
      workspace: undefined,
      project_id: "/Users/test/customer-portal/packages/web",
      project_name: "web",
      project_path: "/Users/test/customer-portal/packages/web",
      project_source: "directory",
      project_registered: false,
      project_exists: true,
      cwd: "/Users/test/customer-portal/packages/web",
      recoverability: "B",
      recovery_reason: "transcript_missing",
      resume_id: undefined,
    }];
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(discovered);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return structuredClone(discoveredSessions);
      if (path === "/api/native/open") return { status: "desktop_opened" };
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await user.click(await screen.findByRole("button", { name: "项目" }));
    expect(await screen.findByRole("heading", { name: "web" })).toBeTruthy();
    expect(screen.getByText("自动发现")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "在 Codex 中打开" }));
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/native/open")).toBe(true));
    const open = mocks.request.mock.calls.find(([path]) => path === "/api/native/open");
    expect(JSON.parse(String((open?.[1] as RequestInit).body))).toEqual({ app: "codex", cwd: "/Users/test/customer-portal/packages/web" });
  });

  it("honors a registered project binding by switching before opening the project", async () => {
    const bound = structuredClone(snapshot);
    bound.workspaces[0].bindings = { codex: "team" };
    const nonResumable = structuredClone(sessions);
    nonResumable[0].recoverability = "B";
    nonResumable[0].resume_id = undefined;
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(bound);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return structuredClone(nonResumable);
      if (path === "/api/switch") return { status: "switched" };
      if (path === "/api/native/open") return { status: "desktop_opened" };
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await user.click(await screen.findByRole("button", { name: "项目" }));
    const project = await screen.findByRole("heading", { name: "Payments" });
    const card = project.closest("article");
    expect(card).toBeTruthy();
    await user.click(within(card as HTMLElement).getByRole("button", { name: "切换项目账号" }));

    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/native/open")).toBe(true));
    const switchIndex = mocks.request.mock.calls.findIndex(([path]) => path === "/api/switch");
    const openIndex = mocks.request.mock.calls.findIndex(([path]) => path === "/api/native/open");
    expect(openIndex).toBeGreaterThan(switchIndex);
  });

  it("switches to a project's Codex account and then resumes its latest native session", async () => {
    const bound = structuredClone(snapshot);
    bound.workspaces[0].bindings = { codex: "team" };
    const nativeSessions = structuredClone(sessions);
    nativeSessions[0].profile = undefined;
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(bound);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions") && path !== "/api/sessions/resume") return structuredClone(nativeSessions);
      if (path === "/api/switch") return {};
      if (path === "/api/sessions/resume") return { status: "terminal_opened" };
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await user.click(await screen.findByRole("button", { name: "项目" }));
    const card = (await screen.findByRole("heading", { name: "Payments" })).closest("article");
    expect(card).toBeTruthy();
    await user.click(within(card as HTMLElement).getByRole("button", { name: "打开最近会话" }));
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/sessions/resume")).toBe(true));
    const switchIndex = mocks.request.mock.calls.findIndex(([path]) => path === "/api/switch");
    const openIndex = mocks.request.mock.calls.findIndex(([path]) => path === "/api/sessions/resume");
    expect(switchIndex).toBeGreaterThan(-1);
    expect(openIndex).toBeGreaterThan(switchIndex);
    const open = mocks.request.mock.calls[openIndex];
    expect(JSON.parse(String((open?.[1] as RequestInit).body))).toEqual({ app: "codex", resume_id: nativeSessions[0].resume_id });
    expect(await screen.findByRole("status")).toHaveProperty("textContent", "已在 Terminal 中安全打开原生会话");
  });

  it("resumes a verified Codex session through the official CLI", async () => {
    const user = userEvent.setup();
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return structuredClone(sessions);
      if (path === "/api/sessions/resume") return { status: "terminal_opened" };
      throw new Error(`Unexpected API path: ${path}`);
    });
    render(<App/>);
    await user.click(await screen.findByRole("button", { name: "会话" }));
    await user.click(await screen.findByRole("button", { name: '继续会话“Continue payments”' }));
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/sessions/resume")).toBe(true));
    const resume = mocks.request.mock.calls.find(([path]) => path === "/api/sessions/resume");
    expect(JSON.parse(String((resume?.[1] as RequestInit).body))).toEqual({ app: "codex", resume_id: sessions[0].resume_id });
  });

  it("switches to the bound project account before resuming an unattributed live session", async () => {
    const liveSessions = structuredClone(sessions);
    liveSessions[0].profile = undefined;
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions") && path !== "/api/sessions/resume") return structuredClone(liveSessions);
      if (path === "/api/switch") return {};
      if (path === "/api/sessions/resume") return { status: "terminal_opened" };
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await user.click(await screen.findByRole("button", { name: "会话" }));
    expect(await screen.findByText("项目账号：Team")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: '继续会话“Continue payments”' }));
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/sessions/resume")).toBe(true));
    const switchIndex = mocks.request.mock.calls.findIndex(([path]) => path === "/api/switch");
    const resumeIndex = mocks.request.mock.calls.findIndex(([path]) => path === "/api/sessions/resume");
    expect(resumeIndex).toBeGreaterThan(switchIndex);
  });

  it("does not open a live Codex session when its required account switch fails", async () => {
    const nativeSessions = structuredClone(sessions);
    nativeSessions[0].profile = undefined;
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return structuredClone(nativeSessions);
      if (path === "/api/switch") {
        throw new ApiError("switch failed", "MIX_SWITCH_ROLLED_BACK", {
          cause_code: "MIX_CREDENTIAL_STORE_UNAVAILABLE",
          cleanup_pending: false,
        });
      }
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await user.click(await screen.findByRole("button", { name: "项目" }));
    const card = (await screen.findByRole("heading", { name: "Payments" })).closest("article");
    expect(card).toBeTruthy();
    await user.click(within(card as HTMLElement).getByRole("button", { name: "打开最近会话" }));
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/switch")).toBe(true));
    expect(mocks.request.mock.calls.some(([path]) => path === "/api/sessions/resume")).toBe(false);
    expect(await screen.findByRole("alert")).toHaveProperty(
      "textContent",
      expect.stringContaining("Mix 无法访问本地凭证目录"),
    );
  });

  it("adds another Codex account after isolated native login completes", async () => {
    const enrollmentId = "b".repeat(32);
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/accounts/enroll" && init?.method === "POST") {
        return { enrollment_id: enrollmentId, state: "pending" };
      }
      if (path === `/api/accounts/enroll/status?id=${enrollmentId}`) {
        return { enrollment_id: enrollmentId, state: "completed", profile: "account-2", label: "Work" };
      }
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();
    fireEvent.click(screen.getByRole("button", { name: "添加账号" }));
    const dialog = await screen.findByRole("dialog", { name: "添加账号" });
    expect(within(dialog).getByRole("tab", { name: /另一个账号/ }).getAttribute("aria-selected")).toBe("true");

    vi.useFakeTimers();
    try {
      fireEvent.click(within(dialog).getByRole("button", { name: "继续登录" }));
      fireEvent.click(within(dialog).getByRole("button", { name: "继续登录" }));
      await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
    } finally {
      vi.useRealTimers();
    }

    expect(mocks.request).toHaveBeenCalledWith("/api/accounts/enroll", expect.objectContaining({ method: "POST", body: JSON.stringify({ app: "codex", repair_profile: null }) }));
    expect(mocks.request.mock.calls.filter(([path]) => path === "/api/accounts/enroll")).toHaveLength(1);
    expect(mocks.request).toHaveBeenCalledWith(`/api/accounts/enroll/status?id=${enrollmentId}`);
    expect(await screen.findByRole("status")).toHaveProperty("textContent", "账号已添加");
  });

  it("reports a failed isolated Codex login without refreshing or claiming an account", async () => {
    const enrollmentId = "c".repeat(32);
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/accounts/enroll" && init?.method === "POST") {
        return { enrollment_id: enrollmentId, state: "pending" };
      }
      if (path === `/api/accounts/enroll/status?id=${enrollmentId}`) {
        return { enrollment_id: enrollmentId, state: "failed", error: "Codex login was cancelled" };
      }
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();
    fireEvent.click(screen.getByRole("button", { name: "添加账号" }));
    const dialog = await screen.findByRole("dialog", { name: "添加账号" });

    vi.useFakeTimers();
    try {
      fireEvent.click(within(dialog).getByRole("button", { name: "继续登录" }));
      await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
    } finally {
      vi.useRealTimers();
    }

    expect(await screen.findByRole("alert")).toHaveProperty("textContent", "另一个 Codex 账号没有添加成功");
    expect(mocks.request.mock.calls.filter(([path]) => path === "/api/state")).toHaveLength(1);
  });

  it("explains how to fix a credential-bearing Codex configuration", async () => {
    const enrollmentId = "e".repeat(32);
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/accounts/enroll" && init?.method === "POST") {
        return { enrollment_id: enrollmentId, state: "pending" };
      }
      if (path === `/api/accounts/enroll/status?id=${enrollmentId}`) {
        return { enrollment_id: enrollmentId, state: "failed", code: "MIX_SENSITIVE_DATA_REJECTED" };
      }
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();
    fireEvent.click(screen.getByRole("button", { name: "添加账号" }));
    const dialog = await screen.findByRole("dialog", { name: "添加账号" });

    vi.useFakeTimers();
    try {
      fireEvent.click(within(dialog).getByRole("button", { name: "继续登录" }));
      await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
    } finally {
      vi.useRealTimers();
    }

    expect(await screen.findByRole("alert")).toHaveProperty(
      "textContent",
      "检测到可能的明文凭据。Mix 未保存或复制它；普通配置只应包含非敏感值，密钥请在“机密环境”中引用本地凭证。",
    );
  });

  it("adds a recognizable account name without changing the stable internal id", async () => {
    const firstRun = structuredClone(snapshot);
    firstRun.apps[0].profiles = [];
    firstRun.apps[0].active = null;
    firstRun.apps[0].account_switch!.current_profile = null;
    firstRun.needs_setup = true;
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(firstRun);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/accounts/capture") return { profile: "account-1" };
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await user.click((await screen.findAllByRole("button", { name: "添加账号" }))[0]);
    const dialog = await screen.findByRole("dialog", { name: "添加账号" });
    const accountName = within(dialog).getByRole("textbox", { name: "账号名称" });
    await user.clear(accountName);
    await user.type(accountName, "公司");
    await user.click(within(dialog).getByRole("button", { name: "添加账号" }));

    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/accounts/capture")).toBe(true));
    const capture = mocks.request.mock.calls.find(([path]) => path === "/api/accounts/capture");
    expect(JSON.parse(String((capture?.[1] as RequestInit | undefined)?.body))).toMatchObject({
      name: "account-1",
      label: "公司",
    });
  });

  it("shows real Codex identity and custom provider names instead of generated account ids", async () => {
    const recognized = structuredClone(snapshot);
    recognized.apps[0].profiles = [
      {
        ...recognized.apps[0].profiles[0],
        label: "Codex 账号 1",
        display_label: "Owner",
        identity: { name: "Owner", email: "owner@example.com", plan: "pro", suffix: "11352F" },
        provider: { id: "openai", name: "OpenAI", official: true },
      },
      {
        ...recognized.apps[0].profiles[1],
        label: "Codex · A1B2C3",
        display_label: "Acme Gateway · ID A1B2C3",
        identity: { suffix: "A1B2C3" },
        provider: { id: "custom", name: "Acme Gateway", official: false },
      },
    ];
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(recognized);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();

    expect(screen.getAllByText("Owner").length).toBeGreaterThan(0);
    expect(screen.getAllByText("owner@example.com").length).toBeGreaterThan(0);
    expect(screen.getByRole("heading", { name: "Acme Gateway · ID A1B2C3", level: 3 })).toBeTruthy();
    expect(screen.getAllByText("Acme Gateway").length).toBeGreaterThan(0);
    expect(screen.getAllByText("账号").length).toBeGreaterThan(1);
    expect(screen.queryByText("Codex 账号 1")).toBeNull();
    expect(screen.queryByRole("heading", { name: "Codex · A1B2C3", level: 3 })).toBeNull();
  });

  it("keeps a recognizable Codex account above its custom provider label", async () => {
    const recognized = structuredClone(snapshot);
    recognized.apps[0].profiles = [{
      ...recognized.apps[0].profiles[0],
      label: "Codex 账号 1",
      display_label: "Alex",
      identity: { name: "Alex", email: "alex@example.com", suffix: "A1B2C3" },
      provider: { id: "acme-gateway", name: "Acme Gateway", official: false },
    }];
    recognized.apps[0].active = "personal";
    recognized.apps[0].account_switch!.current_profile = "personal";
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(recognized);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();

    expect(screen.getByRole("heading", { name: "Alex", level: 3 })).toBeTruthy();
    expect(screen.getAllByText("alex@example.com").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Acme Gateway").length).toBeGreaterThan(0);
  });

  it("identifies a custom-provider Codex login when only its stable fingerprint is available", async () => {
    const customProvider = structuredClone(snapshot);
    customProvider.apps[0].active = null;
    customProvider.apps[0].account_switch!.current_profile = null;
    customProvider.apps[0].account_switch!.identity = { suffix: "A1B2C3", suggested_label: "Codex · A1B2C3" };
    customProvider.apps[0].account_switch!.provider = { id: "acme-gateway", name: "Acme Gateway", official: false };
    customProvider.apps[0].profiles = [structuredClone(snapshot.apps[0].profiles[1])];
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(customProvider);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    const currentAccountChip = screen.getByRole("button", { name: "查看和切换 Codex 当前账号" });
    expect(within(currentAccountChip).getByText("Acme Gateway · ID A1B2C3")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "添加账号" }));
    const dialog = await screen.findByRole("dialog", { name: "添加账号" });

    expect(within(dialog).getByText("Acme Gateway · ID A1B2C3")).toBeTruthy();
    expect((within(dialog).getByRole("textbox", { name: "账号名称" }) as HTMLInputElement).value).toBe("Acme Gateway · ID A1B2C3");
    expect(within(dialog).queryByText("官方账号")).toBeNull();
  });

  it("adds a workspace from one path and derives its name and current bindings", async () => {
    const noWorkspace = structuredClone(snapshot);
    noWorkspace.workspaces = [];
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(noWorkspace);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/workspaces") return { id: "payments" };
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    await user.click(screen.getByRole("button", { name: "项目" }));
    await screen.findByRole("heading", { name: "项目" });
    await user.click(screen.getAllByRole("button", { name: "选择文件夹" })[0]);
    const dialog = await screen.findByRole("dialog", { name: "添加项目" });
    expect(within(dialog).getAllByRole("textbox")).toHaveLength(1);
    expect(within(dialog).getByText("自动使用当前账号")).toBeTruthy();

    await user.click(within(dialog).getByRole("button", { name: "添加项目" }));
    expect(await within(dialog).findByRole("alert")).toHaveProperty("textContent", "请选择项目目录。");
    await user.type(within(dialog).getByRole("textbox"), "/Users/test/projects/billing");
    await user.click(within(dialog).getByRole("button", { name: "添加项目" }));

    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/workspaces")).toBe(true));
    const create = mocks.request.mock.calls.find(([path]) => path === "/api/workspaces");
    expect(JSON.parse(String((create?.[1] as RequestInit | undefined)?.body))).toEqual({
      name: "billing",
      path: "/Users/test/projects/billing",
      bindings: { codex: "personal" },
    });
  });

  it("edits and removes project metadata without implying that native data is deleted", async () => {
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return structuredClone(sessions);
      if (path === "/api/workspaces" && (init?.method === "POST" || init?.method === "DELETE")) return {};
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await user.click(await screen.findByRole("button", { name: "项目" }));
    const projectHeading = await screen.findByRole("heading", { name: "Payments" });
    const projectCard = projectHeading.closest("article");
    expect(projectCard).not.toBeNull();

    await user.click(within(projectCard!).getByRole("button", { name: "项目设置" }));
    const editDialog = await screen.findByRole("dialog", { name: "编辑项目" });
    const name = within(editDialog).getByRole("textbox", { name: "工作区名称" });
    await user.clear(name);
    await user.type(name, "Payment Platform");
    await user.selectOptions(within(editDialog).getByRole("combobox", { name: "在此项目使用 Codex" }), "personal");
    await user.click(within(editDialog).getByRole("button", { name: "保存更改" }));

    await waitFor(() => expect(mocks.request.mock.calls.some(([path, init]) => path === "/api/workspaces" && (init as RequestInit | undefined)?.method === "POST")).toBe(true));
    const update = mocks.request.mock.calls.find(([path, init]) => path === "/api/workspaces" && (init as RequestInit | undefined)?.method === "POST");
    expect(JSON.parse(String((update?.[1] as RequestInit).body))).toEqual({
      name: "Payment Platform",
      path: "/Users/test/payments",
      bindings: { codex: "personal" },
    });

    await user.click(within(projectCard!).getByRole("button", { name: "移除" }));
    const removeDialog = await screen.findByRole("dialog", { name: "从 Mix 移除此项目？" });
    expect(within(removeDialog).getByText(/项目目录、代码及原生会话仍保留原位/)).toBeTruthy();
    await user.click(within(removeDialog).getByRole("button", { name: "从 Mix 移除" }));

    await waitFor(() => expect(mocks.request.mock.calls.some(([path, init]) => path === "/api/workspaces" && (init as RequestInit | undefined)?.method === "DELETE")).toBe(true));
    const remove = mocks.request.mock.calls.find(([path, init]) => path === "/api/workspaces" && (init as RequestInit | undefined)?.method === "DELETE");
    expect(JSON.parse(String((remove?.[1] as RequestInit).body))).toEqual({ workspace: "/Users/test/payments" });
  });

  it("groups a nested session under the longest matching registered project", async () => {
    const nested = structuredClone(sessions);
    nested[0].cwd = "/Users/test/payments/packages/web";
    nested[0].project_path = "/Users/test/payments/packages/web";
    nested[0].project_name = "web";
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return structuredClone(nested);
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await user.click(await screen.findByRole("button", { name: "项目" }));
    expect(await screen.findByRole("heading", { name: "Payments" })).toBeTruthy();
    expect(screen.queryByRole("heading", { name: "web" })).toBeNull();
    expect(screen.getByText("1 个会话")).toBeTruthy();
  });

  it("continues only a verified session using its opaque reference", async () => {
    const user = userEvent.setup();
    const visibleSessions: Session[] = [
      ...structuredClone(sessions),
      {
        id: "global-session",
        title: "Global history",
        app: "codex",
        cwd: "/Users/test/legacy",
        state_dir: "/Users/test/.codex",
        native_command: "codex resume global-session",
        recoverability: "B",
        recovery_reason: "working_directory_missing",
      },
    ];
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path === "/api/sessions/resume") return { status: "terminal_opened" };
      if (path.startsWith("/api/sessions")) return structuredClone(visibleSessions);
      throw new Error(`Unexpected API path: ${path}`);
    });
    render(<App/>);
    await waitForAccountsPage();
    await user.click(screen.getByRole("button", { name: "会话" }));
    await screen.findByRole("heading", { name: "会话" });
    expect(screen.getByText(/原工作目录已移动、丢失或不再属于该项目/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: /继续会话“Global history”/ })).toBeNull();

    await user.click(screen.getByRole("button", { name: "继续会话“Continue payments”" }));
    const resumeCall = mocks.request.mock.calls.find(([path]) => path === "/api/sessions/resume");
    expect(resumeCall).toBeTruthy();
    expect(JSON.parse(String((resumeCall?.[1] as RequestInit | undefined)?.body))).toEqual({
      app: "codex",
      resume_id: sessions[0].resume_id,
    });
    expect(await screen.findByRole("status")).toHaveProperty("textContent", "已在 Terminal 中安全打开原生会话");
  });

  it("keeps the native menu bar bilingual, non-sensitive, and one-click", async () => {
    let handleTrayAction: ((action: { kind: "view"; view: "sessions" } | { kind: "switch"; app: string; profile: string }) => void) | undefined;
    mocks.isDesktopShell.mockReturnValue(true);
    mocks.listenTrayActions.mockImplementation(async (handler) => {
      handleTrayAction = handler;
      return () => undefined;
    });
    render(<App/>);

    await waitForAccountsPage();
    await waitFor(() => expect(mocks.updateTrayState).toHaveBeenCalled());
    const trayState = mocks.updateTrayState.mock.calls.at(-1)?.[0];
    expect(trayState).toEqual({
      language: "zh",
      health_status: "healthy",
      clients: [{
        name: "codex",
        label: "Codex",
        active_profile: "Personal",
        profiles: [
          { name: "personal", label: "Personal", active: true },
          { name: "team", label: "Team", active: false },
        ],
        switchable: true,
      }],
    });
    expect(JSON.stringify(trayState)).not.toContain("/Users/test");
    expect(JSON.stringify(trayState)).not.toContain("auth.json");

    expect(handleTrayAction).toBeTypeOf("function");
    act(() => handleTrayAction?.({ kind: "view", view: "sessions" }));
    expect(await screen.findByRole("heading", { name: "会话" })).toBeTruthy();

    act(() => handleTrayAction?.({ kind: "switch", app: "codex", profile: "personal" }));
    await waitFor(() => expect(mocks.request.mock.calls.filter(([path]) => path === "/api/switch")).toHaveLength(1));
    expect(JSON.parse(String((mocks.request.mock.calls.find(([path]) => path === "/api/switch")?.[1] as RequestInit | undefined)?.body))).toEqual({ app: "codex", profile: "personal" });
    expect(await screen.findByRole("status")).toHaveProperty("textContent", "已打开客户端；当前账号和原生历史未改变");

    act(() => handleTrayAction?.({ kind: "switch", app: "codex", profile: "team" }));
    act(() => handleTrayAction?.({ kind: "switch", app: "codex", profile: "team" }));
    await waitFor(() => expect(mocks.request.mock.calls.filter(([path]) => path === "/api/switch")).toHaveLength(2));
    expect(screen.queryByRole("dialog", { name: /切换到“Team”/ })).toBeNull();
    expect(await screen.findByRole("status")).toHaveProperty("textContent", "切换完成，已重新打开客户端；原生会话仍保留");

    await userEvent.setup().click(screen.getByRole("button", { name: "Switch to English" }));
    await waitFor(() => expect(mocks.updateTrayState.mock.calls.at(-1)?.[0]?.language).toBe("en"));

    mocks.request.mockRejectedValue(new Error("isolated core offline"));
    fireEvent.focus(window);
    await screen.findByRole("alert");
    expect(screen.getByRole("note", { name: "Data stays on this computer · Needs attention" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Needs attention" })).toBeTruthy();
    await waitFor(() => {
      const offlineTray = mocks.updateTrayState.mock.calls.at(-1)?.[0];
      expect(offlineTray?.health_status).toBe("attention");
      expect(offlineTray?.clients[0]?.switchable).toBe(false);
    });
    act(() => handleTrayAction?.({ kind: "switch", app: "codex", profile: "team" }));
    expect(screen.queryByRole("dialog", { name: /Switch account · Codex/ })).toBeNull();
  });

  it("keeps current-account activation enabled when it is the only saved account", async () => {
    const singleAccount = structuredClone(snapshot);
    singleAccount.apps[0].profiles = [singleAccount.apps[0].profiles[0]];
    let handleTrayAction: ((action: { kind: "view"; view: "sessions" } | { kind: "switch"; app: string; profile: string }) => void) | undefined;
    mocks.isDesktopShell.mockReturnValue(true);
    mocks.listenTrayActions.mockImplementation(async (handler) => {
      handleTrayAction = handler;
      return () => undefined;
    });
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(singleAccount);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch") {
        expect(JSON.parse(String(init?.body))).toEqual({ app: "codex", profile: "personal" });
        return { status: "already_active" };
      }
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();
    await waitFor(() => expect(mocks.updateTrayState).toHaveBeenCalled());
    const trayClient = mocks.updateTrayState.mock.calls.at(-1)?.[0]?.clients[0];
    expect(trayClient).toMatchObject({
      profiles: [{ name: "personal", label: "Personal", active: true }],
      switchable: true,
    });

    act(() => handleTrayAction?.({ kind: "switch", app: "codex", profile: "personal" }));
    expect(await screen.findByRole("status")).toHaveProperty("textContent", "已打开客户端；当前账号和原生历史未改变");
  });

  it("switches a Codex account with one click and without export or import", async () => {
    const user = userEvent.setup();
    render(<App/>);

    await waitForAccountsPage();
    const primaryNavigation = screen.getByRole("navigation", { name: "主导航" });
    expect(within(primaryNavigation).queryByRole("button", { name: "概览" })).toBeNull();
    expect(within(primaryNavigation).getAllByRole("button").map((button) => button.textContent)).toEqual(["账号", "会话", "项目"]);
    expect(screen.getByRole("note", { name: "数据仅在本机 · 运行正常" })).toBeTruthy();
    expect(screen.getAllByText("原生目录保留").length).toBeGreaterThanOrEqual(1);

    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    expect(teamRow).toBeTruthy();
    await user.click(within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }));
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/switch")).toBe(true));
    const switchCall = mocks.request.mock.calls.find(([path]) => path === "/api/switch");
    const body = JSON.parse(String((switchCall?.[1] as RequestInit | undefined)?.body));
    expect(body).toEqual({ app: "codex", profile: "team" });
    expect(screen.queryByRole("dialog", { name: /切换到/ })).toBeNull();
    expect(await screen.findByRole("status")).toHaveProperty("textContent", "切换完成，已重新打开客户端；原生会话仍保留");
  });

  it.each([
    { active: "account-1", targetLabel: "momo@example.com", target: "account-3" },
    { active: "account-2", targetLabel: "yusu@example.com", target: "account-1" },
  ])("submits the immutable account id for $targetLabel even when the active row changes order", async ({ active, targetLabel, target }) => {
    const accounts = structuredClone(snapshot);
    accounts.apps[0].active = active;
    accounts.apps[0].configured_active = "account-3";
    const accountSwitch = accounts.apps[0].account_switch!;
    accounts.apps[0].account_switch = {
      ...accountSwitch,
      current_profile: active,
    };
    accounts.apps[0].profiles = [
      { name: "account-1", label: "yusu@example.com", managed_account: true, has_credentials: true, auth_strategy: "local-file", file_count: 1, secret_count: 1, provider: { id: "openai", name: "OpenAI", official: true } },
      { name: "account-2", label: "Hair Free", managed_account: true, has_credentials: true, auth_strategy: "local-file", file_count: 1, secret_count: 1, provider: { id: "custom", name: "Hair Free", official: false } },
      { name: "account-3", label: "momo@example.com", managed_account: true, has_credentials: true, auth_strategy: "local-file", file_count: 1, secret_count: 1, provider: { id: "openai", name: "OpenAI", official: true } },
    ];
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(accounts);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch") return { status: "switched", request: JSON.parse(String(init?.body)) };
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    const targetRow = screen.getAllByRole("article").find((row) => row.textContent?.includes(targetLabel));
    expect(targetRow).toBeTruthy();
    await user.click(within(targetRow as HTMLElement).getByRole("button", { name: "切换到此账号" }));

    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/switch")).toBe(true));
    const switchCall = mocks.request.mock.calls.find(([path]) => path === "/api/switch");
    expect(JSON.parse(String((switchCall?.[1] as RequestInit | undefined)?.body))).toEqual({ app: "codex", profile: target });
  });

  it("drives account switching from capabilities instead of a client name", async () => {
    const generic = structuredClone(snapshot);
    generic.apps[0].name = "agent";
    generic.apps[0].adapter = "nova";
    generic.workspaces = [];
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(generic);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch") {
        expect(JSON.parse(String(init?.body))).toEqual({ app: "agent", profile: "team" });
        return { status: "switched" };
      }
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();

    expect(screen.getByRole("button", { name: "查看和切换 nova 当前账号" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "添加账号" })).toBeTruthy();
    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    await user.click(within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }));
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/switch")).toBe(true));
  });

  it("does not show switch success when target projection verification fails", async () => {
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch" && init?.method === "POST") {
        throw new ApiError("raw backend detail", "MIX_SWITCH_VERIFICATION_FAILED", { observed_profile: "personal" });
      }
      throw new Error(`Unexpected API path: ${path}`);
    });
    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    await user.click(within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }));

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("客户端未能以目标账号或环境稳定运行");
    expect(screen.queryByText("切换完成；原生历史未被移动或改写")).toBeNull();
  });

  it("directs an incomplete rollback cleanup to recovery without exposing backend text", async () => {
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch" && init?.method === "POST") {
        throw new ApiError("private backend path", "MIX_SWITCH_ROLLED_BACK", {
          cause_code: "MIX_LOCAL_FAILURE",
          cleanup_pending: true,
          from_profile: "personal",
          to_profile: "team",
        });
      }
      throw new Error(`Unexpected API path: ${path}`);
    });
    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    await user.click(within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }));

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("临时恢复数据尚未清理");
    expect(alert.textContent).toContain("当前已恢复为“Personal”");
    expect(alert.textContent).not.toContain("private backend path");
  });

  it("deduplicates repeated account clicks while a switch is running", async () => {
    let finishSwitch: ((value: object) => void) | undefined;
    const pendingSwitch = new Promise<object>((resolve) => { finishSwitch = resolve; });
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch" && init?.method === "POST") return pendingSwitch;
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    const switchButton = within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" });
    await user.click(switchButton);
    expect(switchButton).toHaveProperty("textContent", "正在切换…");
    expect(switchButton).toHaveProperty("disabled", true);
    fireEvent.click(switchButton);
    expect(mocks.request.mock.calls.filter(([path]) => path === "/api/switch")).toHaveLength(1);

    finishSwitch?.({});
    await waitFor(() => expect(screen.getByRole("status")).toHaveProperty("textContent", "切换完成，已重新打开客户端；原生会话仍保留"));
  });

  it("syncs only the managed active Codex login and explains the result", async () => {
    render(<App/>);

    await waitForAccountsPage();
    expect(screen.queryByRole("button", { name: "同步当前登录" })).toBeNull();
  });

  it("fails closed when Codex uses its native keyring instead of file login", async () => {
    const unsafe = structuredClone(snapshot);
    unsafe.apps[0].account_switch = { available: false, status: "file_auth_required", storage: "keyring" };
    unsafe.apps[0].capabilities["identity.capture"].available = false;
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(unsafe);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      throw new Error(`Unexpected API path: ${path}`);
    });
    const user = userEvent.setup();
    render(<App/>);

    await waitForAccountsPage();
    expect(screen.getByText(/Codex 当前使用 keyring 凭证/)).toBeTruthy();
    const addAccount = screen.getByRole("button", { name: "添加账号" });
    expect(addAccount.hasAttribute("disabled")).toBe(false);
    await user.click(addAccount);
    const addDialog = await screen.findByRole("dialog", { name: "添加账号" });
    expect((within(addDialog).getByRole("tab", { name: /添加当前账号/ }) as HTMLButtonElement).disabled).toBe(true);
    expect(within(addDialog).getByRole("tab", { name: /另一个账号/ }).getAttribute("aria-selected")).toBe("true");
    expect((within(addDialog).getByRole("button", { name: "继续登录" }) as HTMLButtonElement).disabled).toBe(false);
    await user.click(within(addDialog).getByRole("button", { name: "取消" }));
    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    expect((within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }) as HTMLButtonElement).disabled).toBe(true);
    const stateCalls = () => mocks.request.mock.calls.filter(([path]) => path === "/api/state").length;
    const beforeRecheck = stateCalls();
    await user.click(screen.getByRole("button", { name: "重新检查" }));
    await waitFor(() => expect(stateCalls()).toBeGreaterThan(beforeRecheck));

    expect((within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("does not advertise an unverifiable legacy Codex profile as switchable", async () => {
    const configOnly = structuredClone(snapshot);
    configOnly.apps[0].account_switch = { available: false, status: "sign_in_required", storage: "default" };
    configOnly.apps[0].profiles.forEach((profile) => {
      profile.managed_account = false;
      profile.has_credentials = false;
      profile.auth_strategy = "configuration";
      profile.secret_count = 0;
    });
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(configOnly);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();
    expect(screen.getByText("当前环境")).toBeTruthy();
    expect(screen.getAllByText("2 个环境")).toHaveLength(2);
    expect((screen.getByRole("button", { name: "添加账号" }) as HTMLButtonElement).disabled).toBe(false);
    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    const switchEnvironment = within(teamRow as HTMLElement).getByRole("button", { name: "切换环境" }) as HTMLButtonElement;
    expect(switchEnvironment.disabled).toBe(true);
    expect(switchEnvironment.title).toContain("没有可验证账号身份");
  });

  it("opens the requested account directly from the account card", async () => {
    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    await user.click(within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }));
    await waitFor(() => expect(mocks.request.mock.calls.filter(([path]) => path === "/api/switch")).toHaveLength(1));
    expect(screen.queryByRole("dialog", { name: /切换到/ })).toBeNull();
  });

  it("repairs through identity-checked native login without switching automatically", async () => {
    const enrollmentId = "d".repeat(32);
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch") {
        throw new ApiError("private credential reference must not be displayed", "MIX_ACCOUNT_REAUTH_REQUIRED", { app: "codex", profile: "team" });
      }
      if (path === "/api/accounts/repair/active-login" && init?.method === "POST") {
        throw new ApiError("no safe local match", "MIX_ACCOUNT_LOCAL_REPAIR_UNAVAILABLE");
      }
      if (path === "/api/accounts/enroll" && init?.method === "POST") {
        return { enrollment_id: enrollmentId, state: "pending", mode: "repair", repair_profile: "team" };
      }
      if (path === `/api/accounts/enroll/status?id=${enrollmentId}`) {
        return { enrollment_id: enrollmentId, state: "completed", mode: "repair", profile: "team", repair_profile: "team" };
      }
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();
    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    vi.useFakeTimers();
    try {
      fireEvent.click(within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }));
      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(1000);
      });
    } finally {
      vi.useRealTimers();
    }

    const enrollmentCall = mocks.request.mock.calls.find(([path]) => path === "/api/accounts/enroll");
    expect(JSON.parse(String((enrollmentCall?.[1] as RequestInit).body))).toEqual({ app: "codex", repair_profile: "team" });
    expect(mocks.request.mock.calls.filter(([path]) => path === "/api/switch")).toHaveLength(1);
    expect(document.body.textContent).not.toContain("private credential reference");
    expect(await screen.findByRole("status")).toHaveProperty("textContent", "账号登录已修复；再次点击即可切换");
  });

  it("repairs from the matching active login without switching or opening login", async () => {
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch") {
        throw new ApiError("damaged", "MIX_ACCOUNT_REAUTH_REQUIRED");
      }
      if (path === "/api/accounts/repair/active-login" && init?.method === "POST") {
        return { repaired: true, source: "active_login" };
      }
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();
    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    fireEvent.click(within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }));

    await waitFor(() => expect(screen.getByRole("status")).toHaveProperty("textContent", "已使用当前 Codex 的同一账号登录修复凭证；未切换当前账号"));
    expect(mocks.request.mock.calls.filter(([path]) => path === "/api/switch")).toHaveLength(1);
    expect(mocks.request.mock.calls.some(([path]) => path === "/api/accounts/enroll")).toBe(false);
  });

  it("keeps the current account and does not open login when refresh is temporarily unavailable", async () => {
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch") {
        throw new ApiError("network detail", "MIX_ACCOUNT_REFRESH_FAILED");
      }
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();
    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    fireEvent.click(within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }));

    expect(await screen.findByRole("alert")).toHaveProperty(
      "textContent",
      "无法刷新目标账号的登录状态；当前账号没有改变。请检查网络后重试。",
    );
    expect(mocks.request.mock.calls.some(([path]) => path === "/api/accounts/enroll")).toBe(false);
  });

  it("renames an account without changing its login or stable identifier", async () => {
    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    expect(teamRow).toBeTruthy();

    await user.click(within(teamRow as HTMLElement).getByRole("button", { name: "更多账号操作" }));
    await user.click(within(teamRow as HTMLElement).getByRole("button", { name: "重命名" }));
    const dialog = await screen.findByRole("dialog", { name: "编辑账号" });
    const accountName = within(dialog).getByRole("textbox", { name: "账号名称" });
    await user.clear(accountName);
    await user.type(accountName, "公司账号");
    await user.click(within(dialog).getByRole("button", { name: "保存更改" }));

    await waitFor(() => expect(mocks.request.mock.calls.some(([path, init]) => path === "/api/profiles" && (init as RequestInit)?.method === "PATCH")).toBe(true));
    const updateCall = mocks.request.mock.calls.find(([path, init]) => path === "/api/profiles" && (init as RequestInit)?.method === "PATCH");
    expect(JSON.parse(String((updateCall?.[1] as RequestInit).body))).toEqual({
      app: "codex",
      profile: "team",
      label: "公司账号",
    });
  });

  it("removes an active account only after selecting a safe replacement", async () => {
    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    const cards = screen.getAllByRole("article");
    const personalCard = cards.find((card) => card.textContent?.includes("Personal"));
    expect(personalCard).toBeTruthy();
    await user.click(within(personalCard as HTMLElement).getByRole("button", { name: "更多账号操作" }));
    await user.click(within(personalCard as HTMLElement).getByRole("button", { name: "移除" }));
    const dialog = await screen.findByRole("dialog", { name: /从 Mix 移除/ });
    expect(within(dialog).queryByText(/有 .* 个项目使用此账号/)).toBeNull();
    expect((within(dialog).getByRole("combobox") as HTMLSelectElement).value).toBe("team");
    await user.click(within(dialog).getByRole("button", { name: "从 Mix 移除" }));
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/switch")).toBe(true));
    const deleteCall = mocks.request.mock.calls.find(([path, init]) => path === "/api/profiles" && (init as RequestInit)?.method === "DELETE");
    expect(deleteCall).toBeTruthy();
    expect(JSON.parse(String((deleteCall?.[1] as RequestInit).body))).toMatchObject({ app: "codex", profile: "personal", detach_workspaces: true });
    expect(JSON.parse(String((deleteCall?.[1] as RequestInit).body))).not.toHaveProperty("allow_active");
    expect(await screen.findByRole("status")).toHaveProperty("textContent", expect.stringContaining("账号已从 Mix 移除"));
  });

  it("removes an active Claude environment without sending Codex history options", async () => {
    const claude = structuredClone(snapshot);
    const client = claude.apps[0];
    client.name = "claude";
    client.adapter = "claude";
    client.profile_category = "environment";
    client.capabilities = {};
    client.account_switch = { available: false, status: "unsupported", storage: null };
    client.profiles.forEach((profile) => {
      profile.managed_account = false;
      profile.auth_strategy = "configuration";
    });
    claude.workspaces = [{ id: "payments", name: "Payments", path: "/Users/test/payments", bindings: { claude: "team" }, exists: true }];
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return claude;
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch") return {};
      if (path === "/api/profiles" && init?.method === "DELETE") return {};
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    expect(screen.getAllByText("2 个环境").length).toBeGreaterThan(0);
    expect(screen.getByText("当前环境")).toBeTruthy();
    expect(screen.getByText("已添加的环境")).toBeTruthy();
    expect((screen.getByRole("button", { name: "添加环境" }) as HTMLButtonElement).disabled).toBe(false);
    await user.click(screen.getByRole("button", { name: "添加环境" }));
    const addDialog = await screen.findByRole("dialog", { name: "添加环境" });
    expect(within(addDialog).getByText("环境名称")).toBeTruthy();
    expect(within(addDialog).getByText("创建空白环境")).toBeTruthy();
    expect(within(addDialog).queryByText(/Codex 原生登录流程/)).toBeNull();
    await user.click(within(addDialog).getByRole("button", { name: "关闭" }));
    const personalCard = screen.getAllByRole("article").find((card) => card.textContent?.includes("Personal"));
    expect(personalCard).toBeTruthy();
    await user.click(within(personalCard as HTMLElement).getByRole("button", { name: "更多操作" }));
    await user.click(within(personalCard as HTMLElement).getByRole("button", { name: "移除" }));
    const dialog = await screen.findByRole("dialog", { name: /从 Mix 移除/ });
    await user.click(within(dialog).getByRole("button", { name: "从 Mix 移除" }));
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/switch")).toBe(true));
    const switchCall = mocks.request.mock.calls.find(([path]) => path === "/api/switch");
    expect(JSON.parse(String((switchCall?.[1] as RequestInit).body))).toEqual({ app: "claude", profile: "team" });
    expect(await screen.findByRole("status")).toHaveProperty("textContent", expect.stringContaining("环境已移除"));
  });

  it("explains a partially completed active-account removal", async () => {
    mocks.request.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/state") return structuredClone(snapshot);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch") return {};
      if (path === "/api/profiles" && init?.method === "DELETE") {
        throw new ApiError("delete conflict", "MIX_PROFILE_ACTIVE", { status: 409 });
      }
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    const personalCard = screen.getAllByRole("article").find((card) => card.textContent?.includes("Personal"));
    expect(personalCard).toBeTruthy();
    await user.click(within(personalCard as HTMLElement).getByRole("button", { name: "更多账号操作" }));
    await user.click(within(personalCard as HTMLElement).getByRole("button", { name: "移除" }));
    const dialog = await screen.findByRole("dialog", { name: /从 Mix 移除/ });
    await user.click(within(dialog).getByRole("button", { name: "从 Mix 移除" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveProperty("textContent", expect.stringContaining("已切换到替代账号或环境，但原条目未能移除"));
  });

  it("allows a signed-out Codex client to restore a saved managed account", async () => {
    const mixed = structuredClone(snapshot);
    mixed.apps[0].account_switch = { available: false, status: "sign_in_required", storage: "default" };
    mixed.apps[0].capabilities["identity.capture"].available = false;
    mixed.apps[0].configured_active = "personal";
    mixed.apps[0].active = null;
    mixed.apps[0].profiles[0].managed_account = false;
    mixed.apps[0].profiles[0].has_credentials = false;
    mixed.apps[0].profiles[0].auth_strategy = "configuration";
    mixed.apps[0].profiles.push({
      name: "staging", label: "Staging",
      managed_account: false, has_credentials: false, auth_strategy: "configuration",
      file_count: 1, secret_count: 0,
    });
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(mixed);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      if (path === "/api/switch") return { status: "switched" };
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    expect(screen.getAllByText("1 个账号 · 2 个环境").length).toBeGreaterThan(0);
    expect(screen.getByText("已添加的账号与环境")).toBeTruthy();
    expect(screen.getByRole("button", { name: "查看和切换 Codex 当前账号" })).toBeTruthy();
    const rows = screen.getAllByRole("article");
    const teamRow = rows.find((row) => row.textContent?.includes("Team"));
    const stagingRow = rows.find((row) => row.textContent?.includes("Staging"));
    expect(screen.getByText("已退出登录")).toBeTruthy();
    expect(screen.getByText(/可直接选择下方已保存账号重新登录/)).toBeTruthy();
    expect((within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }) as HTMLButtonElement).disabled).toBe(false);
    expect((within(stagingRow as HTMLElement).getByRole("button", { name: "切换环境" }) as HTMLButtonElement).disabled).toBe(true);
    await user.click(within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }));
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/switch")).toBe(true));
    await user.click(screen.getByRole("button", { name: "添加账号" }));
    const addDialog = await screen.findByRole("dialog", { name: "添加账号" });
    expect(within(addDialog).getByRole("tab", { name: /另一个账号/ }).getAttribute("aria-selected")).toBe("true");
    expect((within(addDialog).getByRole("button", { name: "继续登录" }) as HTMLButtonElement).disabled).toBe(false);
  });

  it("switches to English and keeps Local Web updates explicitly desktop-only", async () => {
    const user = userEvent.setup();
    const { container } = render(<App/>);
    await waitForAccountsPage();

    await user.click(screen.getByRole("button", { name: "Switch to English" }));
    await screen.findByRole("heading", { name: "Accounts", level: 1 });
    expect(document.documentElement.lang).toBe("en");
    expect(screen.getByRole("button", { name: "Switch to Chinese" })).toBeTruthy();

    await user.click(screen.getByRole("button", { name: "Settings" }));
    await screen.findByRole("heading", { name: "Preferences & security" });
    expect(screen.getByText("Local Web cannot install desktop updates. Check from the signed Mac App.")).toBeTruthy();
    expect(screen.getAllByText("Mac App only")).toHaveLength(2);
    expect(screen.getByText("Open-source licenses")).toBeTruthy();
    expect(screen.getByText(/full Rust and npm dependency license texts/)).toBeTruthy();
    expect(screen.getByText("Uninstall & data retention")).toBeTruthy();
    expect(screen.getByText("Uninstall keeps history")).toBeTruthy();
    expect(screen.getByText(/isolated runtimes can also contain native sessions/)).toBeTruthy();
    expect(screen.getByText("Running version")).toBeTruthy();
    expect(screen.getByText("v0.1.2")).toBeTruthy();

    const audit = await axe.run(container, { rules: { "color-contrast": { enabled: false } } });
    expect(audit.violations.map((violation) => `${violation.id}: ${violation.help}`)).toEqual([]);
  });

  it("explains an inaccessible local credential directory while keeping ordinary setup reachable", async () => {
    const user = userEvent.setup();
    const linux = structuredClone(snapshot);
    linux.security.credential_store = {
      supported: true,
      available: false,
      backend: "Mix private files",
      kind: "local-files",
      reason: "permission_denied",
    };
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(linux);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return structuredClone(sessions);
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    await waitForAccountsPage();
    const addAccount = screen.getByRole("button", { name: "添加账号" });
    expect((addAccount as HTMLButtonElement).disabled).toBe(false);
    await user.click(addAccount);
    const addDialog = await screen.findByRole("dialog", { name: "添加账号" });
    expect(within(addDialog).getByRole("tab", { name: /另一个账号/ }).getAttribute("aria-selected")).toBe("true");
    expect((within(addDialog).getByRole("button", { name: "继续登录" }) as HTMLButtonElement).disabled).toBe(true);
    await user.click(within(addDialog).getByRole("button", { name: "取消" }));
    await user.click(screen.getByRole("button", { name: "Switch to English" }));
    await user.click(screen.getByRole("button", { name: "Settings" }));
    expect(await screen.findByText(/cannot access its local credential directory/)).toBeTruthy();
    expect(screen.getByText("Local directory unavailable")).toBeTruthy();
  });

  it("blocks client actions and restores an interrupted account switch", async () => {
    const user = userEvent.setup();
    const pending = structuredClone(snapshot);
    pending.health = {
      status: "attention",
      issues: [{ app: "codex", issue: { code: "interrupted_switch" } }],
      local_only: true,
    };
    pending.recovery.interrupted_switch = {
      required: true,
      status: "pending",
      id: "transaction-1",
      app: "codex",
      from_profile: "personal",
      to_profile: "team",
      created_at: "2026-08-31T00:00:00Z",
    };
    let recovered = false;
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(recovered ? snapshot : pending);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return structuredClone(sessions);
      if (path === "/api/recovery/interrupted") {
        recovered = true;
        return { recovered: true };
      }
      throw new Error(`Unexpected API path: ${path}`);
    });

    render(<App/>);
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("检测到中断的账号切换"));
    await waitForAccountsPage();
    const teamRow = screen.getAllByRole("article").find((row) => row.textContent?.includes("Team"));
    expect((within(teamRow as HTMLElement).getByRole("button", { name: "切换到此账号" }) as HTMLButtonElement).disabled).toBe(true);
    await user.click(screen.getByRole("button", { name: "恢复切换前状态" }));
    await waitFor(() => expect(mocks.request.mock.calls.some(([path]) => path === "/api/recovery/interrupted")).toBe(true));
    await waitFor(() => expect(screen.queryByText("检测到中断的账号切换")).toBeNull());
    expect(await screen.findByRole("status")).toHaveProperty("textContent", "已恢复到切换前状态；原生历史和凭据均已复原");
  });

  it("localizes structured Core health issues when the language changes", async () => {
    const warning = structuredClone(snapshot);
    warning.apps[0].status = "warning";
    warning.apps[0].issues = [{ code: "unsupported_account_profile" }];
    warning.health = {
      status: "attention",
      issues: [{ app: "codex", issue: { code: "unsupported_account_profile" } }],
      local_only: true,
    };
    mocks.request.mockImplementation(async (path: string) => {
      if (path === "/api/state") return structuredClone(warning);
      if (path === "/api/discovery") return [];
      if (path.startsWith("/api/sessions")) return [];
      throw new Error(`Unexpected API path: ${path}`);
    });

    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    expect(screen.getByText("存在无法验证身份的旧账号；请移除后通过原生登录重新添加。")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Switch to English" }));
    expect(screen.getByText("An older account has no verifiable identity; remove it and add it again through native sign-in.")).toBeTruthy();
  });

  it("opens the command palette from the keyboard and restores focus on Escape", async () => {
    const user = userEvent.setup();
    render(<App/>);
    await waitForAccountsPage();
    const trigger = screen.getByRole("button", { name: "搜索与快捷操作" });
    await user.click(trigger);
    const palette = await screen.findByRole("dialog", { name: "快速操作" });
    expect(within(palette).getByRole("textbox", { name: "输入命令或工作区名称" })).toBe(document.activeElement);

    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "快速操作" })).toBeNull());
    expect(document.activeElement).toBe(trigger);
  });
});
