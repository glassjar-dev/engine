/**
 * Where the engine's own `engine_bg.wasm` comes from when the host overrides the
 * build-time default (`EngineOptions.wasmUrl`).
 *
 * Workers load the Wasm as soon as they start, before any message could reach them, so
 * the URL travels in `WorkerOptions.name` behind this prefix.
 */
export const WASM_WORKER_NAME_PREFIX = 'glassjar-wasm:';

/** Resolve to an absolute http(s) URL, or throw. */
export function resolveWasmUrl(value: string | URL, base?: string): string {
  const url = new URL(String(value), base);
  if (url.protocol !== 'https:' && url.protocol !== 'http:') {
    throw new TypeError(`Engine Wasm must be served over http(s), not ${url.protocol}`);
  }
  return url.href;
}
