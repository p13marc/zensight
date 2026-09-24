import { defineConfig } from "vitest/config";
import wasm from "vite-plugin-wasm";

export default defineConfig({
  // zenoh-ts's key-expression checker is wasm-bindgen's "bundler" output
  // (`import * as wasm from "./…_bg.wasm"`), which needs this plugin; the
  // plugin's ESM integration top-level-awaits the module, which an `esnext`
  // target emits as is (every browser with WebCodecs has it, #707).
  plugins: [wasm()],
  optimizeDeps: { exclude: ["@eclipse-zenoh/zenoh-ts"] },
  // The page is served next to the bridge in a deployment; no absolute base.
  base: "./",
  build: {
    target: "esnext",
    outDir: "dist",
    sourcemap: true,
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
