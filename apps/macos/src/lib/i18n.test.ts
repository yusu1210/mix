import { describe, expect, it } from "vitest";
import { COPY, createTranslator } from "./i18n";

const placeholders = (value: string) => [...value.matchAll(/\{([A-Za-z0-9_]+)\}/g)].map((match) => match[1]).sort();

describe("bilingual product copy", () => {
  it("keeps every Chinese and English entry non-empty with matching placeholders", () => {
    for (const key of Object.keys(COPY.zh) as (keyof typeof COPY.zh)[]) {
      expect(COPY.zh[key].trim(), `empty Chinese copy: ${key}`).not.toBe("");
      expect(COPY.en[key].trim(), `empty English copy: ${key}`).not.toBe("");
      expect(placeholders(COPY.en[key]), `placeholder mismatch: ${key}`).toEqual(placeholders(COPY.zh[key]));
    }
  });

  it("interpolates user-visible values in either language", () => {
    expect(createTranslator("zh")("healthTitle", { count: 2 })).toBe("有 2 个客户端需要检查");
    expect(createTranslator("en")("workspaceAccountDifferent", { profile: "Team" })).toContain("Team");
  });

  it("does not describe Claude environment isolation as account switching", () => {
    expect(COPY.zh.workspaceDetail).toContain("Codex 账号或 Claude Code 隔离环境");
    expect(COPY.en.workspaceDetail).toContain("Codex account or Claude Code isolated environment");
  });
});
