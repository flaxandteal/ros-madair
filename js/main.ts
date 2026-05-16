// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

// Version injected at build time by Vite
declare const __ROS_MADAIR_VERSION__: string;
export const version: string = __ROS_MADAIR_VERSION__;

export { initWasm as default, initWasm, setWasmURL } from "./_wasm";
export { SparqlStore } from "./_wasm";
export {
  setBackend,
  getBackend,
  setNapiModule,
  getNapiModule,
  autoDetectBackend,
  createSparqlStore,
} from "./backend";
export type { BackendType } from "./backend";
