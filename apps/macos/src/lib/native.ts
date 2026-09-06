import { open } from "@tauri-apps/plugin-dialog";
import { writeText as writeNativeClipboardText } from "@tauri-apps/plugin-clipboard-manager";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

type TauriWindow = Window & { __TAURI_INTERNALS__?: unknown };
export type UpdaterCapability = { enabled: boolean; automatic_checks: boolean };
export type TrayState = {
  language: "zh" | "en";
  health_status: "healthy" | "attention";
  clients: {
    name: string;
    label: string;
    active_profile?: string | null;
    profiles: { name: string; label: string; active: boolean }[];
    switchable: boolean;
  }[];
};
export type TrayAction =
  | { kind: "view"; view: "sessions" }
  | { kind: "switch"; app: string; profile: string };

export function isDesktopShell(): boolean {
  return Boolean((window as TauriWindow).__TAURI_INTERNALS__);
}

export async function pickDirectory(title: string, defaultPath?: string): Promise<string | null> {
  if (!isDesktopShell()) return null;
  const selection = await open({
    title,
    defaultPath: defaultPath || undefined,
    directory: true,
    multiple: false,
    canCreateDirectories: true,
  });
  return typeof selection === "string" ? selection : null;
}

export async function getUpdaterCapability(): Promise<UpdaterCapability> {
  if (!isDesktopShell()) return { enabled: false, automatic_checks: false };
  return invoke<UpdaterCapability>("updater_capability");
}

export async function updateTrayState(state: TrayState): Promise<void> {
  if (!isDesktopShell()) return;
  await invoke("update_tray", { state });
}

export async function listenTrayActions(handler: (action: TrayAction) => void): Promise<UnlistenFn> {
  if (!isDesktopShell()) return () => undefined;
  return listen<TrayAction>("mix-tray-action", (event) => handler(event.payload));
}

export async function openThirdPartyLicenses(): Promise<void> {
  if (!isDesktopShell()) throw new Error("Third-party licenses are available in the installed Mac App");
  await invoke("open_third_party_licenses");
}

export async function openLatestRelease(): Promise<void> {
  if (!isDesktopShell()) throw new Error("Release downloads are available in the installed Mac App");
  await invoke("open_latest_release");
}

export async function writeClipboardText(value: string): Promise<void> {
  if (isDesktopShell()) {
    await writeNativeClipboardText(value);
    return;
  }
  let clipboardError: unknown;
  try {
    if (!navigator.clipboard?.writeText) throw new Error("Clipboard API is unavailable");
    await navigator.clipboard.writeText(value);
    return;
  } catch (error) {
    clipboardError = error;
  }

  // WKWebView can deny navigator.clipboard even for a user-triggered action.
  // Keep a local DOM fallback so Tauri and ordinary browsers share one copy path.
  const active = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const selection = window.getSelection();
  const ranges = selection ? Array.from({ length: selection.rangeCount }, (_, index) => selection.getRangeAt(index).cloneRange()) : [];
  const textarea = document.createElement("textarea");
  textarea.value = value;
  textarea.readOnly = true;
  textarea.setAttribute("aria-hidden", "true");
  textarea.style.position = "fixed";
  textarea.style.inset = "0 auto auto -10000px";
  textarea.style.opacity = "0";
  document.body.appendChild(textarea);
  textarea.focus({ preventScroll: true });
  textarea.select();
  let copied = false;
  try {
    copied = document.execCommand("copy");
  } finally {
    textarea.remove();
    active?.focus({ preventScroll: true });
    if (selection) {
      selection.removeAllRanges();
      ranges.forEach((range) => selection.addRange(range));
    }
  }
  if (!copied) {
    throw clipboardError instanceof Error ? clipboardError : new Error("Clipboard write failed");
  }
}
