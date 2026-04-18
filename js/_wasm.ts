// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

import init from "../pkg/ros_madair";

let wasmURL: string = (() => {
  try {
    return new URL("../pkg/ros_madair_bg.wasm", import.meta.url).href;
  } catch {
    return "ros_madair_bg.wasm";
  }
})();

let wasmInitialized = false;

export function setWasmURL(url: string) {
  if (wasmInitialized) {
    throw new Error("Cannot set WASM URL after initialization");
  }
  wasmURL = url;
}

export async function initWasm() {
  if (wasmInitialized) return;
  if (wasmURL === 'data:,' || wasmURL === '') {
    throw new Error('[ros-madair] WASM URL not configured');
  }
  await init({ module_or_path: wasmURL });
  wasmInitialized = true;
}

export { SparqlStore } from "../pkg/ros_madair";
