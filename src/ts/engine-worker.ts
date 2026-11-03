// Not `&inline`: inlined data:/blob: workers cannot fetch same-origin Wasm paths
// like `/engine_bg.wasm` (Failed to parse URL / opaque origin under COEP).
import { WASM_WORKER_NAME_PREFIX } from './wasm-source';
import RustWorker from './worker?worker';

/** One explicit internal construction seam, also used by deterministic lifecycle tests. */
export function createEngineWorker(wasmUrl?: string): Worker {
  return new RustWorker(wasmUrl ? { name: WASM_WORKER_NAME_PREFIX + wasmUrl } : undefined);
}
