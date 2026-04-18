// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

import wasm from "vite-plugin-wasm";
import { resolve } from "path";
import { defineConfig } from "vite";
import { readFileSync, writeFileSync, existsSync } from "fs";
import topLevelAwait from "vite-plugin-top-level-await";

// Workaround for Vite 5 library mode inlining all assets as base64 (vitejs/vite#4454).
// vite-plugin-wasm generates `import url from "file.wasm?url"`, but Vite 5 lib mode
// resolves ?url imports as base64 data URIs. This plugin replaces the inlined WASM
// with a relative file path, and copies the actual WASM file to dist.
// Can be removed once upgraded to Vite 6+ (use ?no-inline instead).
function externalizeWasmPlugin() {
  return {
    name: 'externalize-wasm',
    enforce: 'post',
    generateBundle(_options, bundle) {
      for (const [fileName, chunk] of Object.entries(bundle)) {
        if (chunk.type === 'chunk' && chunk.code) {
          const before = chunk.code.length;
          chunk.code = chunk.code.replace(
            /["']data:application\/wasm;base64,[A-Za-z0-9+/=]+["']/g,
            '"ros_madair_bg.wasm"'
          );
          if (chunk.code.length !== before) {
            const saved = ((before - chunk.code.length) / 1024 / 1024).toFixed(2);
            console.log(`[externalize-wasm] Replaced inlined WASM in ${fileName} (saved ${saved} MB)`);
          }
        }
      }
    },
    writeBundle(options) {
      const outDir = options.dir || 'dist';
      const wasmSrc = resolve(__dirname, 'pkg/ros_madair_bg.wasm');
      const wasmDest = resolve(outDir, 'ros_madair_bg.wasm');

      if (existsSync(wasmSrc)) {
        const wasmContent = readFileSync(wasmSrc);
        writeFileSync(wasmDest, wasmContent);
        console.log(`[externalize-wasm] Copied WASM to ${wasmDest} (${(wasmContent.length / 1024 / 1024).toFixed(2)} MB)`);
      }
    }
  };
}

export default defineConfig({
  plugins: [
    wasm(),
    topLevelAwait(),
    externalizeWasmPlugin(),
  ],
  optimizeDeps: {
    exclude: ['./pkg'],
  },
  build: {
    minify: false,
    sourcemap: true,
    // Don't inline any assets - emit WASM as separate file
    assetsInlineLimit: 0,
    lib: {
      entry: resolve(__dirname, "js/main.ts"),
      name: "RosMadair",
      fileName: (format) => format === 'es' ? 'ros-madair.js' : 'ros-madair.umd.cjs',
      formats: ['es', 'umd'],
    },
    rollupOptions: {
      output: {
        exports: 'named',
        // Preserve WASM filename without hash for predictable imports
        assetFileNames: (assetInfo) => {
          if (assetInfo.name?.endsWith('.wasm')) {
            return 'ros_madair_bg.wasm';
          }
          return 'assets/[name]-[hash][extname]';
        },
      },
    },
  },
});
