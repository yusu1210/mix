import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";
import packageJson from "./package.json" with { type: "json" };

export default defineConfig({
  plugins: [react()],
  define: { __MIX_VERSION__: JSON.stringify(packageJson.version) },
  test: {
    environment: "jsdom",
    environmentOptions: {
      jsdom: { url: "http://127.0.0.1:17666/" },
    },
    clearMocks: true,
    restoreMocks: true,
  },
});
