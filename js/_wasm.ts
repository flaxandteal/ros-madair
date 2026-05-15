// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

import init, { SparqlStore } from "ros-madair-client";
import { getBackend, setWasmModule } from "./backend";

let customWasmURL: string | undefined;
let wasmInitialized = false;

export function setWasmURL(url: string) {
  if (wasmInitialized) {
    throw new Error("Cannot set WASM URL after initialization");
  }
  customWasmURL = url;
}

export async function initWasm() {
  if (getBackend() === 'napi') return;
  if (wasmInitialized) return;
  if (customWasmURL) {
    await init({ module_or_path: customWasmURL });
  } else {
    await init();
  }
  wasmInitialized = true;
  setWasmModule({ SparqlStore });
}

export { SparqlStore };
