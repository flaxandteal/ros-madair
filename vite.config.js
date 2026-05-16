// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

import { resolve } from "path";
import { defineConfig } from "vite";
import pkg from "./package.json" with { type: "json" };

export default defineConfig({
  define: {
    __ROS_MADAIR_VERSION__: JSON.stringify(pkg.version),
  },
  build: {
    minify: false,
    sourcemap: true,
    lib: {
      entry: resolve(__dirname, "js/main.ts"),
      name: "RosMadair",
      fileName: () => 'ros-madair.js',
      formats: ['es'],
    },
    rollupOptions: {
      external: ['ros-madair-client', 'ros-madair-napi'],
      output: {
        exports: 'named',
        paths: {
          // Rewrite the external import so the dist bundle references the
          // wasm-pack output via a relative path.  This eliminates the need
          // for ros-madair-client as an npm dependency — the pkg/ directory
          // is shipped in the tarball via the "files" field instead.
          'ros-madair-client': '../pkg/ros_madair.js',
        },
      },
    },
  },
});
