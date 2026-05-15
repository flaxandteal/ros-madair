// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

/**
 * Backend abstraction for WASM vs NAPI.
 *
 * Consumers call factory functions; the underlying backend is swapped at
 * runtime. Follows the same pattern as alizarin's backend.ts.
 */

export type BackendType = 'wasm' | 'napi';

let _backend: BackendType = 'wasm';
let _napiModule: any = null;
let _wasmModule: any = null;

// ---------------------------------------------------------------------------
// Backend switching
// ---------------------------------------------------------------------------

export function setBackend(backend: BackendType): void {
  _backend = backend;
}

export function getBackend(): BackendType {
  return _backend;
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

export function setNapiModule(mod: any): void {
  _napiModule = mod;
}

export function getNapiModule(): any {
  if (_napiModule) return _napiModule;
  if (typeof globalThis !== 'undefined' && (globalThis as any).__ros_madair_napi) {
    _napiModule = (globalThis as any).__ros_madair_napi;
    return _napiModule;
  }
  return null;
}

export function setWasmModule(mod: any): void {
  _wasmModule = mod;
}

export function getWasmModule(): any {
  return _wasmModule;
}

// ---------------------------------------------------------------------------
// Auto-detection
// ---------------------------------------------------------------------------

export function autoDetectBackend(): BackendType {
  // Environment variable override
  if (typeof process !== 'undefined' && process.env?.ROS_MADAIR_BACKEND) {
    const env = process.env.ROS_MADAIR_BACKEND.toLowerCase();
    if (env === 'napi' || env === 'wasm') {
      _backend = env as BackendType;
      return _backend;
    }
  }

  // In Node.js, prefer NAPI if available
  if (typeof process !== 'undefined' && process.versions?.node) {
    const napi = getNapiModule();
    if (napi) {
      _backend = 'napi';
      return 'napi';
    }
  }

  _backend = 'wasm';
  return 'wasm';
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/**
 * Create a SparqlStore backed by the current backend.
 *
 * - NAPI: `new NapiSparqlStore(indexDir, baseUri)` — synchronous filesystem
 * - WASM: `new SparqlStore(baseUrl)` — requires separate `loadSummary()` call
 *
 * For NAPI the first argument is a filesystem path; for WASM it is a URL.
 */
export function createSparqlStore(pathOrUrl: string, baseUri?: string): any {
  if (_backend === 'napi') {
    const napi = getNapiModule();
    if (!napi) {
      throw new Error('NAPI backend selected but ros-madair-napi module not available');
    }
    return new napi.NapiSparqlStore(pathOrUrl, baseUri ?? '');
  }

  const wasm = getWasmModule();
  if (!wasm) {
    throw new Error('WASM backend selected but ros-madair-client module not loaded');
  }
  return new wasm.SparqlStore(pathOrUrl);
}
