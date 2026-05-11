import type { Config } from "tailwindcss";

// ADR-0055 §`Tech stack`: Tailwind matches the existing ork-webui
// toolchain. v1 ships with the default theme; light/dark theme
// toggle lives in the App-level `data-theme` attribute.
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  darkMode: ["selector", "[data-theme='dark']"],
  theme: {
    extend: {},
  },
  plugins: [],
} satisfies Config;
