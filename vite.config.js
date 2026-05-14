// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

import { resolve } from "path";
import { defineConfig } from "vite";

export default defineConfig({
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
      external: ['ros-madair-client'],
      output: { exports: 'named' },
    },
  },
});
