import { beforeEach, describe, expect, it, vi } from "vitest";

const tokenKey = "mix.local-web-token";

beforeEach(() => {
  vi.restoreAllMocks();
  vi.resetModules();
  window.sessionStorage.clear();
  window.history.replaceState({}, "", "/");
});

describe("Local Web token lifecycle", () => {
  it("uses a new fragment token when an existing tab receives a new launch URL", async () => {
    window.sessionStorage.setItem(tokenKey, "stale-token");
    const api = await import("./api");
    window.history.replaceState({}, "", "/#token=fresh-token");
    const fetch = vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response("{}", {
      status: 200,
      headers: { "content-type": "application/json" },
    }));

    await api.request("/api/health");

    const headers = new Headers(fetch.mock.calls[0][1]?.headers);
    expect(headers.get("x-mix-token")).toBe("fresh-token");
    expect(window.sessionStorage.getItem(tokenKey)).toBe("fresh-token");
    expect(window.location.hash).toBe("");
  });

  it("forgets a token rejected by the local service", async () => {
    window.history.replaceState({}, "", "/#token=expired-token");
    const api = await import("./api");
    vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(JSON.stringify({
      code: "MIX_AUTH_REQUIRED",
      error: "local service authentication failed",
    }), {
      status: 401,
      headers: { "content-type": "application/json" },
    }));

    await expect(api.request("/api/health")).rejects.toMatchObject({ code: "MIX_AUTH_REQUIRED" });

    expect(window.sessionStorage.getItem(tokenKey)).toBeNull();
  });
});
