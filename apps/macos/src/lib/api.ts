import { invoke } from "@tauri-apps/api/core";

type ControlPlane = { base_url: string; token?: string | null };

export class ApiError extends Error {
  code: string;
  details: Record<string, unknown>;

  constructor(message: string, code = "MIX_REQUEST_FAILED", details: Record<string, unknown> = {}) {
    super(message);
    this.name = "ApiError";
    this.code = code;
    this.details = details;
  }
}

const localHosts = new Set(["127.0.0.1", "localhost", "::1", "[::1]"]);
const isLocalWeb = (window.location.protocol === "http:" || window.location.protocol === "https:")
  && localHosts.has(window.location.hostname)
  && Boolean(window.location.port)
  && window.location.port !== "1420";

const LOCAL_WEB_TOKEN_KEY = "mix.local-web-token";
let localWebToken = isLocalWeb ? sessionStorage.getItem(LOCAL_WEB_TOKEN_KEY) : null;

function consumeLocalWebFragment(): void {
  if (!isLocalWeb) return;
  const fragment = new URLSearchParams(window.location.hash.replace(/^#/, ""));
  const token = fragment.get("token");
  if (!token) return;
  localWebToken = token;
  sessionStorage.setItem(LOCAL_WEB_TOKEN_KEY, token);
  const cleanUrl = new URL(window.location.href);
  fragment.delete("token");
  cleanUrl.hash = fragment.toString();
  window.history.replaceState({}, document.title, `${cleanUrl.pathname}${cleanUrl.search}${cleanUrl.hash}`);
}

consumeLocalWebFragment();

let connectionPromise: Promise<ControlPlane> | null = null;

async function controlPlane(): Promise<ControlPlane> {
  if (isLocalWeb) {
    consumeLocalWebFragment();
    return { base_url: window.location.origin, token: localWebToken };
  }
  if (!connectionPromise) {
    connectionPromise = invoke<ControlPlane>("control_plane").catch((error) => {
      connectionPromise = null;
      throw error;
    });
  }
  return connectionPromise;
}

export async function request<T = unknown>(path: string, options?: RequestInit): Promise<T> {
  const method = String(options?.method || "GET").toUpperCase();
  const attempts = method === "GET" ? 8 : 1;
  let lastError: unknown;
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      const connection = await controlPlane();
      const headers = new Headers(options?.headers || {});
      if (connection.token) headers.set("x-mix-token", connection.token);
      const response = await fetch(connection.base_url + path, { ...options, headers });
      const text = await response.text();
      let body: unknown = {};
      try {
        body = text ? JSON.parse(text) : {};
      } catch {
        throw new ApiError("The local service returned an invalid response", "MIX_INVALID_RESPONSE");
      }
      if (!response.ok) {
        const errorBody = body && typeof body === "object" && !Array.isArray(body)
          ? body as Record<string, unknown>
          : {};
        const code = typeof errorBody.code === "string" ? errorBody.code : "MIX_REQUEST_FAILED";
        if (isLocalWeb && code === "MIX_AUTH_REQUIRED") {
          localWebToken = null;
          sessionStorage.removeItem(LOCAL_WEB_TOKEN_KEY);
        }
        throw new ApiError(
          typeof errorBody.error === "string" ? errorBody.error : `Request failed (${response.status})`,
          code,
          errorBody.details && typeof errorBody.details === "object" && !Array.isArray(errorBody.details)
            ? errorBody.details as Record<string, unknown>
            : {},
        );
      }
      return body as T;
    } catch (error) {
      lastError = error;
      if (!(error instanceof TypeError) || attempt + 1 >= attempts) break;
      await new Promise((resolve) => window.setTimeout(resolve, 150));
    }
  }
  throw lastError instanceof Error ? lastError : new ApiError("Local service unavailable", "MIX_SERVICE_UNAVAILABLE");
}

export function parseBindings(value: string, errorMessage: string): Record<string, string> {
  const rows = value.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
  const result = Object.create(null) as Record<string, string>;
  for (const row of rows) {
    const index = row.indexOf("=");
    if (index < 1 || !row.slice(index + 1).trim()) throw new Error(errorMessage);
    const key = row.slice(0, index).trim();
    if (!key || Object.hasOwn(result, key)) throw new Error(errorMessage);
    result[key] = row.slice(index + 1).trim();
  }
  return result;
}

export function parseSecretBindings(value: string, formatError: string, referenceError: string): Record<string, { service: string; account: string }> {
  const plain = parseBindings(value, formatError);
  return Object.fromEntries(Object.entries(plain).map(([key, reference]) => {
    const index = reference.indexOf("/");
    if (index < 1 || !reference.slice(index + 1)) throw new Error(referenceError);
    return [key, { service: reference.slice(0, index), account: reference.slice(index + 1) }];
  }));
}
