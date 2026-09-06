import { describe, expect, it } from "vitest";
import { parseBindings, parseSecretBindings } from "./api";

describe("advanced environment parsing", () => {
  it("preserves special keys without mutating the object prototype", () => {
    const result = parseBindings("__proto__=literal\nMODEL=sonnet", "invalid");
    expect(Object.getPrototypeOf(result)).toBeNull();
    expect(result.__proto__).toBe("literal");
    expect(result.MODEL).toBe("sonnet");
  });

  it("rejects duplicate keys instead of silently replacing a value", () => {
    expect(() => parseBindings("MODEL=one\nMODEL=two", "invalid")).toThrow("invalid");
  });

  it("keeps the complete credential account after the first separator", () => {
    expect(parseSecretBindings("TOKEN=service/team/account", "invalid", "reference")).toEqual({
      TOKEN: { service: "service", account: "team/account" },
    });
  });
});
