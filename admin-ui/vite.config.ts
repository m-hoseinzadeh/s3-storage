import { writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// `dist/` is generated, but rust-embed needs the directory to exist at compile
// time or `cargo build` fails outright on a fresh clone. Git cannot track an empty
// directory, so a `.gitkeep` is committed instead (see admin-ui/.gitignore) — and
// `emptyOutDir` wipes it on every build, so put it back afterwards.
const keepDistTracked = (): Plugin => ({
  name: "keep-dist-tracked",
  apply: "build",
  closeBundle() {
    writeFileSync(resolve(__dirname, "dist/.gitkeep"), "");
  },
});

// The panel is served at the root of its own dedicated port.
export default defineConfig({
  base: "/",
  plugins: [react(), tailwindcss(), keepDistTracked()],
  // Injected at build time (set by CI / Docker on each push); "dev" locally.
  define: {
    __APP_VERSION__: JSON.stringify(process.env.APP_VERSION || "dev"),
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
});
