import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  // Bind IPv4 loopback so `http://127.0.0.1:5173` matches ork's WEBUI_DEV_PROXY
  // (default `localhost` can listen on ::1 only on some macOS setups).
  server: { port: 5173, strictPort: true, host: "127.0.0.1" },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/setupTests.ts"],
    globals: true,
  },
});
