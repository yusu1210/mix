import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import packageJson from "./package.json" with { type: "json" };

export default defineConfig({
  plugins: [react()],
  define: { __MIX_VERSION__: JSON.stringify(packageJson.version) },
  clearScreen: false,
  server: { port: 1420, strictPort: true },
});
