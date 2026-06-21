import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// The panel is served at the root of its own dedicated port.
export default defineConfig({
  base: "/",
  plugins: [react(), tailwindcss()],
  // Injected at build time (set by CI / Docker on each push); "dev" locally.
  define: {
    __APP_VERSION__: JSON.stringify(process.env.APP_VERSION || "dev"),
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
});
