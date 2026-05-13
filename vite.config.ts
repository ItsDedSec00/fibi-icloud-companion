import { defineConfig } from "vite";
import { resolve } from "path";

const projectRoot = __dirname;
const srcRoot = resolve(projectRoot, "src");

export default defineConfig({
  clearScreen: false,
  root: srcRoot,
  publicDir: resolve(projectRoot, "assets"),
  server: {
    port: 1420,
    strictPort: true,
    host: "127.0.0.1",
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "es2022",
    minify: "esbuild",
    sourcemap: true,
    outDir: resolve(projectRoot, "dist"),
    emptyOutDir: true,
    rollupOptions: {
      input: {
        cat: resolve(srcRoot, "cat/index.html"),
        settings: resolve(srcRoot, "settings/index.html"),
      },
    },
  },
});
