import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// ADR-0055 §`Hot reload`: dev server lives on 127.0.0.1:5174. The ork
// dev-server proxy lives in the follow-up reverse-proxy ADR; for v1
// the Vite build output is consumed via rust-embed and served by the
// Rust embed module.
export default defineConfig({
  plugins: [react()],
  base: "/studio/",
  build: {
    outDir: "dist",
    sourcemap: false,
    target: "es2022",
  },
  server: {
    port: 5174,
    strictPort: true,
    host: "127.0.0.1",
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/setupTests.ts"],
    globals: true,
  },
});
