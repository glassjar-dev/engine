import init, * as wasm from '../../pkg/engine.js';
import wasmBinary from '../../pkg/engine_bg.wasm';
import { CompilerModuleCache, ConfigureCompilerModule } from './compiler-module-cache';
import type { ToolchainSeed } from './verified-toolchain';

let identity = { scopeEpoch: 0, operationId: 0, cacheEpoch: 0 };
let acknowledge: (() => void) | undefined;
const compilerModules = new CompilerModuleCache((message) =>
  postMessage({ ...message, ...identity })
);

// Register before Rust's message handler so these internal envelopes never become jobs.
addEventListener(
  'message',
  (
    event: MessageEvent<
      ConfigureCompilerModule & {
        toolchain?: ToolchainSeed;
        scope_epoch?: number;
        operation_id?: number;
        cache_epoch?: number;
      }
    >
  ) => {
    const message = event.data;
    if (message?.type === 'configure_cpp') {
      event.stopImmediatePropagation();
      if (!message.toolchain) throw new Error('Missing verified compiler seed');
      compilerModules.configureVerified(message.toolchain, message.cacheCompilerModule);
      wasm.configure_compiler_toolchain(
        message.toolchain.compiler,
        message.toolchain.sysroot,
        message.toolchain.compilerHash,
        message.toolchain.sysrootHash,
        message.toolchain.exceptionsHash,
        message.toolchain.stockProfile === true
      );
    } else if ((message as { type?: string })?.type === 'object_build_ack') {
      event.stopImmediatePropagation();
      const ack = message as unknown as typeof identity;
      if (
        ack.scopeEpoch === identity.scopeEpoch &&
        ack.operationId === identity.operationId &&
        ack.cacheEpoch === identity.cacheEpoch
      ) {
        const done = acknowledge;
        acknowledge = undefined;
        done?.();
      }
    } else if (typeof message?.operation_id === 'number') {
      compilerModules.beginOperation();
      identity = {
        scopeEpoch: message.scope_epoch!,
        operationId: message.operation_id,
        cacheEpoch: message.cache_epoch!
      };
    }
  }
);

try {
  await init({ module_or_path: wasmBinary });
  wasm.set_compiler_module_constructor((bytes: Uint8Array) =>
    compilerModules.constructVerified(bytes)
  );
  wasm.set_object_build_observer(
    () =>
      new Promise<void>((resolve) => {
        acknowledge = resolve;
        postMessage({ type: 'object_build_complete', ...identity });
      })
  );
  wasm.set_compiler_instance_observer((milliseconds: number) =>
    compilerModules.instantiated(milliseconds)
  );
  wasm.main();
} catch (err) {
  postMessage({
    type: 'error',
    message: err instanceof Error ? err.message : String(err)
  });
}
