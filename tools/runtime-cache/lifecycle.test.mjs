import { build } from 'esbuild';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { test } from 'node:test';

class FakeWorker {
  listeners = new Map();
  messages = [];
  terminated = false;
  addEventListener(type, callback) {
    const values = this.listeners.get(type) ?? new Set();
    values.add(callback);
    this.listeners.set(type, values);
  }
  removeEventListener(type, callback) {
    this.listeners.get(type)?.delete(callback);
  }
  postMessage(value) {
    this.messages.push(structuredClone(value));
  }
  terminate() {
    this.terminated = true;
  }
  emitError(message) {
    const event = new Event('error', { cancelable: true });
    Object.defineProperty(event, 'message', { value: message });
    // Listeners added during dispatch do not handle that same event.
    const onerror = this.onerror;
    for (const callback of [...(this.listeners.get('error') ?? [])])
      if (this.listeners.get('error').has(callback)) callback(event);
    onerror?.(event);
    return event;
  }
  emit(data) {
    const start = this.messages.findLast((message) => typeof message.operation_id === 'number');
    if (start)
      data = {
        scopeEpoch: start.scope_epoch,
        operationId: start.operation_id,
        cacheEpoch: start.cache_epoch,
        ...data
      };
    for (const callback of [...(this.listeners.get('message') ?? [])])
      if (this.listeners.get('message').has(callback)) callback({ data });
  }
  count() {
    return [...this.listeners.values()].reduce((sum, values) => sum + values.size, 0);
  }
}
const workers = [];
globalThis.__engineTestWorker = () => {
  const worker = new FakeWorker();
  workers.push(worker);
  return worker;
};
const bundle = await build({
  entryPoints: ['src/ts/index.ts'],
  bundle: true,
  write: false,
  platform: 'node',
  format: 'esm',
  plugins: [
    {
      name: 'internal-test-construction',
      setup(build) {
        build.onResolve({ filter: /pkg\/engine(?:\.js)?$/ }, () => ({
          path: 'bindings',
          namespace: 'test'
        }));
        build.onResolve({ filter: /verified-toolchain\.js$/ }, () => ({
          path: 'toolchain',
          namespace: 'test'
        }));
        build.onResolve({ filter: /\.wasm$/ }, () => ({ path: 'binary', namespace: 'test' }));
        build.onResolve({ filter: /\.\/engine-worker$/ }, () => ({
          path: 'worker',
          namespace: 'test'
        }));
        build.onLoad({ filter: /.*/, namespace: 'test' }, ({ path }) => ({
          contents:
            path === 'toolchain'
              ? `export class VerifiedToolchain { identity = 'test'; identityBytes = 16; compilerBytes = 8; static async load(...args) { await globalThis.__engineTestToolchainLoad?.(...args); return new VerifiedToolchain(); } configure(worker, envelope) { worker.postMessage(envelope); } }`
              : path === 'worker'
                ? 'export const createEngineWorker = () => globalThis.__engineTestWorker();'
                : path === 'binary'
                  ? 'export default new Uint8Array();'
                  : `let initialized = false; export default async function init() { initialized = true; };
         export const prefetch_urls = () => []; export const cpp_exception_hash = () => 'test';
         export class DapAdapter {
           constructor() { if (!initialized) throw new Error("Debugger constructed before WASM initialization"); }
           attach(worker) { this.worker = worker; this.listener = e => {
             if (e.data.type === 'stop') this.callback?.({event:'terminated'});
           }; worker.addEventListener('message', this.listener); }
           detach() { this.worker?.removeEventListener('message', this.listener); }
           resetRun() {} on(callback) { this.callback = callback; }
           sendMessage() {} setFilterInternals() {}
         }`
        }));
      }
    }
  ]
});
const { Engine } = await import(
  `data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`
);
const options = (extra = {}) => ({
  cpp: {
    scope: { projectId: 'p', accountId: 'a' },
    objectCache: true,
    retainWorker: true,
    idleTimeoutMs: 1000,
    ...extra
  }
});
const ready = async (worker) => {
  worker.emit({ type: 'ready' });
  await new Promise((resolve) => setImmediate(resolve));
};
const commit = async (worker) => {
  worker.emit({ type: 'object_build_complete' });
  await new Promise((resolve) => setTimeout(resolve, 10));
};
const complete = async (worker) => {
  await commit(worker);
  worker.emit({ type: 'stop', exit_code: 0, build_ms: 1, run_ms: 1 });
};
const object = new Uint8Array([
  0, 97, 115, 109, 1, 0, 0, 0, 0, 9, 7, 108, 105, 110, 107, 105, 110, 103, 2
]);
const source = (key) => ({
  source: '/main.cpp',
  key: key.repeat(64),
  object_hash: createHash('sha256').update(object).digest('hex'),
  object
});
const record = (worker, engine) =>
  worker.emit({
    type: 'cpp_cache_object',
    generation: engine.cacheStats.generation,
    ...source('b')
  });

for (const identityFirst of [false, true]) {
  test(`initial toolchain loading overlaps worker initialization (${identityFirst ? 'identity' : 'ready'} first)`, async () => {
    const gate = Promise.withResolvers();
    let loads = 0;
    globalThis.__engineTestToolchainLoad = () => {
      loads++;
      return gate.promise;
    };
    const engine = await Engine.create('c', options());
    try {
      assert.equal(loads, 0, 'creation does not fetch');
      const job = engine.prepareCompiler();
      const worker = workers.at(-1);
      assert.equal(loads, 1, 'load starts without waiting for ready');
      assert.equal(worker.messages.length, 0);
      if (identityFirst) {
        gate.resolve();
        await new Promise((resolve) => setImmediate(resolve));
        assert.equal(worker.messages.length, 0, 'verified code cannot configure an unready worker');
        await ready(worker);
      } else {
        await ready(worker);
        assert.equal(worker.messages.length, 0, 'ready alone cannot start an unverified job');
        gate.resolve();
        await new Promise((resolve) => setImmediate(resolve));
      }
      assert.equal(loads, 1);
      assert.equal(worker.messages[0].type, 'configure_cpp');
      assert.equal(worker.messages[1].prepare_compiler, true);
      await complete(worker);
      assert.equal((await job).type, 'completed');
    } finally {
      gate.resolve();
      delete globalThis.__engineTestToolchainLoad;
      await engine.dispose();
    }
  });
}

test('toolchain rejection before worker ready settles and retires the job', async () => {
  const gate = Promise.withResolvers();
  globalThis.__engineTestToolchainLoad = () => gate.promise;
  const engine = await Engine.create('c', options());
  try {
    const job = engine.prepareCompiler();
    const worker = workers.at(-1);
    gate.reject(new Error('toolchain unavailable'));
    assert.equal((await job).type, 'error');
    assert.equal(worker.terminated, true);
    await ready(worker);
    assert.equal(worker.messages.length, 0);
  } finally {
    delete globalThis.__engineTestToolchainLoad;
    await engine.dispose();
  }
});

test('cancel before ready fences the old worker while a new job shares the pending load', async () => {
  const gate = Promise.withResolvers();
  let loads = 0;
  globalThis.__engineTestToolchainLoad = () => {
    loads++;
    return gate.promise;
  };
  const engine = await Engine.create('c', options());
  try {
    const oldJob = engine.prepareCompiler();
    const oldWorker = workers.at(-1);
    await engine.cancelCurrentOperation();
    assert.equal((await oldJob).type, 'stopped');
    const newJob = engine.prepareCompiler();
    const newWorker = workers.at(-1);
    await ready(oldWorker);
    await ready(newWorker);
    gate.resolve();
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(loads, 1);
    assert.equal(oldWorker.messages.length, 0);
    assert.equal(oldWorker.terminated, true);
    assert.equal(newWorker.messages.at(-1).prepare_compiler, true);
    await complete(newWorker);
    await newJob;
  } finally {
    gate.resolve();
    delete globalThis.__engineTestToolchainLoad;
    await engine.dispose();
  }
});

test('a cancelled early rejection cannot poison the next submission', async () => {
  const gate = Promise.withResolvers();
  let loads = 0;
  globalThis.__engineTestToolchainLoad = () => {
    loads++;
    return loads === 1 ? gate.promise : Promise.resolve();
  };
  const engine = await Engine.create('c', options());
  try {
    const oldJob = engine.prepareCompiler();
    await engine.cancelCurrentOperation();
    await oldJob;
    gate.reject(new Error('cancelled load failed'));
    await new Promise((resolve) => setImmediate(resolve));
    const next = engine.prepareCompiler();
    const worker = workers.at(-1);
    await ready(worker);
    assert.equal(loads, 2);
    assert.equal(worker.messages.at(-1).prepare_compiler, true);
    await complete(worker);
    assert.equal((await next).type, 'completed');
  } finally {
    delete globalThis.__engineTestToolchainLoad;
    await engine.dispose();
  }
});

test('dispose before worker ready aborts and drains early initialization without resurrection', async () => {
  const gate = Promise.withResolvers();
  let signal;
  globalThis.__engineTestToolchainLoad = (_urls, _cache, suppliedSignal) => {
    signal = suppliedSignal;
    return gate.promise;
  };
  const engine = await Engine.create('c', options());
  try {
    const job = engine.prepareCompiler();
    const worker = workers.at(-1);
    let drained = false;
    const disposal = engine.dispose().then(() => {
      drained = true;
    });
    assert.equal(signal.aborted, true);
    await ready(worker);
    assert.equal(drained, false);
    assert.equal((await job).type, 'stopped');
    gate.resolve();
    await disposal;
    assert.equal(worker.messages.length, 0);
    assert.equal(engine.cacheStats.workerRetained, false);
    assert.equal(engine.cacheStats.toolchainIdentityBytes, 0);
  } finally {
    gate.resolve();
    delete globalThis.__engineTestToolchainLoad;
    await engine.dispose();
  }
});

test('release after early cancellation aborts and drains initialization', async () => {
  const gate = Promise.withResolvers();
  let signal;
  globalThis.__engineTestToolchainLoad = (_urls, _cache, suppliedSignal) => {
    signal = suppliedSignal;
    return gate.promise;
  };
  const engine = await Engine.create('c', options());
  try {
    const job = engine.prepareCompiler();
    const worker = workers.at(-1);
    await engine.cancelCurrentOperation();
    await job;
    let drained = false;
    const release = engine.releaseCompilerResources().then((result) => {
      drained = true;
      return result;
    });
    assert.equal(signal.aborted, true);
    await ready(worker);
    assert.equal(drained, false);
    gate.resolve();
    assert.equal(await release, 'released');
    assert.equal(worker.messages.length, 0);
    assert.equal(engine.cacheStats.toolchainIdentityBytes, 0);
  } finally {
    gate.resolve();
    delete globalThis.__engineTestToolchainLoad;
    await engine.dispose();
  }
});

test('snapshot and mode captured before readiness; one job active', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  engine.fs = { 'main.cpp': 'first' };
  engine.binaryFiles = { '/x.a': new Uint8Array([1]) };
  const job = engine.run();
  const worker = workers.at(-1);
  engine.fs['main.cpp'] = 'later';
  engine.binaryFiles['/x.a'][0] = 2;
  engine.debugger.enabled = true;
  assert.equal((await engine.compile()).type, 'error');
  await ready(worker);
  const start = worker.messages.at(-1);
  assert.equal(start.fs['main.cpp'], 'first');
  assert.equal(start.binary_files['/x.a'][0], 1);
  assert.equal(start.is_debug, false);
  await complete(worker);
  await job;
  await engine.dispose();
});

test('creation owns options before asynchronous initialization', async () => {
  const configuration = options();
  const creation = Engine.create('c', configuration);
  configuration.cpp.scope.projectId = 'mutated';
  configuration.cpp.retainWorker = false;
  const engine = await creation;
  assert.equal(engine.scope.projectId, 'p');
  engine.debugger.enabled = false;
  const job = engine.run();
  const worker = workers.at(-1);
  await ready(worker);
  await complete(worker);
  await job;
  assert.equal(engine.cacheStats.workerRetained, true);
  await engine.dispose();
});

test('binary snapshots own Buffer and subclass bytes before worker readiness', async () => {
  class ViewBytes extends Uint8Array {
    slice() {
      return this.subarray();
    }
  }
  const engine = await Engine.create('c', options());
  const buffer = Buffer.from([1, 2]);
  const subclass = new ViewBytes([3, 4]);
  engine.binaryFiles = { '/buffer.bin': buffer, '/subclass.bin': subclass };
  const job = engine.compile();
  const worker = workers.at(-1);
  buffer[0] = 9;
  subclass[0] = 9;
  await ready(worker);
  const start = worker.messages.at(-1);
  assert.deepEqual([...start.binary_files['/buffer.bin']], [1, 2]);
  assert.deepEqual([...start.binary_files['/subclass.bin']], [3, 4]);
  await complete(worker);
  await job;
  await engine.dispose();
});

test('Debug retires its worker while preserving the verified compiler owner', async () => {
  const engine = await Engine.create('c', options({ cacheCompilerModule: true }));
  engine.debugger.enabled = false;
  const first = engine.compile();
  const retained = workers.at(-1);
  await ready(retained);
  const bytes = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]);
  retained.emit({
    type: 'compiler_module',
    value: { module: new WebAssembly.Module(bytes), bytes, sha256: 'test-identity' }
  });
  await complete(retained);
  await first;
  assert.equal(engine.cacheStats.compilerModuleIdentityBytes, 8);
  engine.debugger.enabled = true;
  const debug = engine.run();
  assert.equal(retained.terminated, true);
  assert.equal(engine.cacheStats.compilerModuleIdentityBytes, 8);
  const worker = workers.at(-1);
  await ready(worker);
  const configuration = worker.messages.find((message) => message.type === 'configure_cpp');
  assert.equal(configuration.cacheCompilerModule, true);
  assert.equal(configuration.compilerModule, undefined);
  await complete(worker);
  await debug;
  await engine.dispose();
});

test('immediate Stop before Ready fences dispatch and late output', async () => {
  const engine = await Engine.create('c', options());
  let output = 0;
  engine.stdout.on('data', () => output++);
  const job = engine.run();
  const worker = workers.at(-1);
  await engine.stop();
  await ready(worker);
  worker.emit({ type: 'stdout', mode: 1, data: new Uint8Array([1]) });
  assert.equal((await job).type, 'stopped');
  assert.equal(worker.messages.length, 0);
  assert.equal(output, 0);
  assert.equal(worker.count(), 0);
  await engine.dispose();
});

test('clear during compile rejects all late object records and retires worker', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  const job = engine.compile();
  const worker = workers.at(-1);
  await ready(worker);
  const generation = engine.cacheStats.generation;
  engine.clearCaches();
  worker.emit({ type: 'cpp_cache_object', generation, ...source('b') });
  await complete(worker);
  await job;
  assert.equal(engine.cacheStats.entries, 0);
  assert.equal(worker.terminated, true);
  await engine.dispose();
});

test('successful repeated jobs reuse worker with stable listeners and fresh stdin', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  const first = engine.run();
  const worker = workers.at(-1);
  await ready(worker);
  const firstBuffer = worker.messages.at(-1).stdin_buffer;
  const listeners = worker.count();
  record(worker, engine);
  await complete(worker);
  await first;
  assert.equal(engine.cacheStats.entries, 1);
  const second = engine.run();
  assert.equal(workers.at(-1), worker);
  assert.equal(worker.count(), listeners);
  assert.notEqual(worker.messages.at(-1).stdin_buffer, firstBuffer);
  assert.equal(worker.messages.at(-1).cpp_cache.entries.length, 1);
  await complete(worker);
  await second;
  assert.equal(engine.cacheStats.latest.workerReused, true);
  await engine.dispose();
  assert.equal(worker.count(), 0);
  assert.equal(engine.cacheStats.entries, 0);
});

test('nonzero outcomes never populate; errors recover; Debug retires retained worker', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  let job = engine.run();
  let worker = workers.at(-1);
  await ready(worker);
  record(worker, engine);
  worker.emit({ type: 'stop', exit_code: 1, build_ms: 1, run_ms: 0 });
  await job;
  assert.equal(engine.cacheStats.entries, 0);
  assert.ok(worker.terminated);
  job = engine.run();
  worker = workers.at(-1);
  await ready(worker);
  worker.emit({ type: 'error', message: 'failure' });
  assert.equal((await job).type, 'error');
  job = engine.run();
  worker = workers.at(-1);
  await ready(worker);
  await complete(worker);
  await job;
  engine.debugger.enabled = true;
  job = engine.run();
  assert.ok(worker.terminated);
  worker = workers.at(-1);
  await ready(worker);
  assert.equal(worker.messages.at(-1).cpp_cache, null);
  await complete(worker);
  await job;
  assert.ok(worker.terminated);
  await engine.dispose();
});

test('retired worker errors are contained without affecting the replacement job', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  const run = engine.run();
  const retired = workers.at(-1);
  await ready(retired);
  let terminationError;
  retired.terminate = () => {
    retired.terminated = true;
    terminationError = retired.emitError('termination trap');
  };
  await engine.cancelCurrentOperation();
  assert.equal((await run).type, 'stopped');
  assert.equal(terminationError.defaultPrevented, true);
  assert.equal(retired.count(), 0);

  const replacementRun = engine.run();
  const replacement = workers.at(-1);
  await ready(replacement);
  const active = engine.active;
  assert.equal(retired.emitError('late queued trap').defaultPrevented, true);
  assert.equal(engine.active, active);
  assert.equal(replacement.terminated, false);
  await complete(replacement);
  assert.equal((await replacementRun).type, 'completed');
  await engine.dispose();
  assert.equal(replacement.emitError('late disposal trap').defaultPrevented, true);
});

test('current worker errors still fail the operation and clear committed objects', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  const run = engine.run();
  const worker = workers.at(-1);
  await ready(worker);
  record(worker, engine);
  await commit(worker);
  assert.equal(engine.cacheStats.entries, 1);
  const error = worker.emitError('current worker fault');
  const result = await run;
  assert.equal(result.type, 'error');
  assert.match(result.error.message, /current worker fault/);
  assert.equal(error.defaultPrevented, false);
  assert.equal(engine.cacheStats.entries, 0);
  assert.equal(worker.terminated, true);
  await engine.dispose();
});

test('compile never opens devices; synchronous opener Stop prevents start', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  let opens = 0;
  engine.hostDevice = () => {
    opens++;
    void engine.stop();
  };
  let job = engine.compile();
  let worker = workers.at(-1);
  await ready(worker);
  assert.equal(worker.messages.at(-1).build_only, true);
  await complete(worker);
  await job;
  assert.equal(opens, 0);
  job = engine.run();
  worker = workers.at(-1);
  assert.equal((await job).type, 'stopped');
  assert.equal(opens, 1);
  assert.equal(worker.messages.filter((m) => m.lang).length, 1);
  await engine.dispose();
});

test('scope fixed and isolated; disposed engines reject; idle timeout releases worker', async () => {
  const engine = await Engine.create('c', options({ idleTimeoutMs: 5 }));
  engine.debugger.enabled = false;
  const job = engine.run();
  const worker = workers.at(-1);
  await ready(worker);
  record(worker, engine);
  await complete(worker);
  await job;
  await new Promise((resolve) => setTimeout(resolve, 15));
  assert.ok(worker.terminated);
  const other = await Engine.create(
    'c',
    options({ scope: { projectId: 'other', accountId: 'b' } })
  );
  assert.equal(other.cacheStats.entries, 0);
  assert.ok(Object.isFrozen(engine.scope));
  await engine.dispose();
  assert.equal((await engine.run()).type, 'error');
  await other.dispose();
});

test('Debug completion still emits DAP terminated before listener retirement', async () => {
  const engine = await Engine.create('c');
  let terminated = 0;
  engine.debugger.on('event', (event) => {
    if (event.event === 'terminated') terminated++;
  });
  const job = engine.run();
  const worker = workers.at(-1);
  await ready(worker);
  await complete(worker);
  assert.equal((await job).type, 'completed');
  assert.equal(terminated, 1);
  assert.equal(worker.count(), 0);
  await engine.dispose();
});

test('compiler preparation has no student inputs/devices and uses the ordinary busy lane', async () => {
  const engine = await Engine.create('c', options({ cacheCompilerModule: true }));
  let opened = 0;
  engine.hostDevice = () => {
    opened++;
  };
  engine.fs = { 'bad.cpp': '#error must not compile' };
  engine.binaryFiles = { '/secret.bin': new Uint8Array([1]) };
  const preparation = engine.prepareCompiler();
  const worker = workers.at(-1);
  assert.equal((await engine.run()).type, 'error');
  await ready(worker);
  const start = worker.messages.at(-1);
  assert.equal(start.prepare_compiler, true);
  assert.deepEqual(start.fs, {});
  assert.deepEqual(start.binary_files, {});
  assert.equal(start.cpp_cache, null);
  assert.equal(opened, 0);
  worker.emit({ type: 'stop', exit_code: 0, build_ms: 1, run_ms: 0 });
  assert.equal((await preparation).timing.runMs, 0);
  assert.equal(engine.cacheStats.latest.operation, 'prepare_compiler');
  await engine.dispose();
});

test('compiler-stage acknowledgement commits before GUI Stop and reseeding retains objects', async () => {
  const engine = await Engine.create('c', options({ cacheCompilerModule: true }));
  engine.debugger.enabled = false;
  const run = engine.run();
  const worker = workers.at(-1);
  await ready(worker);
  record(worker, engine);
  assert.equal(engine.cacheStats.entries, 0);
  assert.equal(engine.cacheStats.stagingEntries, 1);
  await commit(worker);
  assert.equal(worker.messages.at(-1).type, 'object_build_ack');
  assert.equal(engine.cacheStats.entries, 1);
  await engine.cancelCurrentOperation();
  assert.equal((await run).type, 'stopped');
  assert.equal(engine.cacheStats.entries, 1);
  assert.equal(engine.cacheStats.compilerModuleIdentityBytes, 8);
  const preparation = engine.prepareCompiler();
  const replacement = workers.at(-1);
  await ready(replacement);
  await complete(replacement);
  await preparation;
  assert.equal(engine.cacheStats.entries, 1);
  await engine.stop();
  assert.equal(engine.cacheStats.entries, 0);
  assert.equal(engine.cacheStats.compilerModuleIdentityBytes, 0);
  await engine.dispose();
});

test('clear fences cache writes while preserving active guest output and commit acknowledgement', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  let output = 0;
  engine.stdout.on('data', () => output++);
  const run = engine.run();
  const worker = workers.at(-1);
  await ready(worker);
  record(worker, engine);
  engine.clearCaches();
  worker.emit({ type: 'stdout', mode: 1, data: new Uint8Array([1]) });
  await commit(worker);
  assert.equal(worker.messages.at(-1).type, 'object_build_ack');
  assert.equal(output, 1);
  assert.equal(engine.cacheStats.entries, 0);
  await complete(worker);
  await run;
  assert.equal(worker.terminated, true);
  await engine.dispose();
});

test('retained slot rejects prior-operation output and completion', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  const first = engine.run();
  const worker = workers.at(-1);
  await ready(worker);
  const operationId = engine.cacheStats.operationId;
  await complete(worker);
  await first;
  let output = 0;
  engine.stdout.on('data', () => output++);
  const second = engine.run();
  worker.emit({ type: 'stdout', operationId, mode: 1, data: new Uint8Array([1]) });
  worker.emit({ type: 'stop', operationId, exit_code: 0, build_ms: 0, run_ms: 0 });
  assert.equal(output, 0);
  assert.equal((await engine.compile()).type, 'error');
  assert.equal(worker.messages.filter((message) => message.type === 'configure_cpp').length, 1);
  await complete(worker);
  await second;
  await engine.dispose();
});

test('project lifetime skips idle timer; aborting deferred release preserves active guest', async () => {
  const engine = await Engine.create(
    'c',
    options({ compilerLifetime: 'project', idleTimeoutMs: 5 })
  );
  engine.debugger.enabled = false;
  let run = engine.run();
  const worker = workers.at(-1);
  await ready(worker);
  record(worker, engine);
  await complete(worker);
  await run;
  await new Promise((resolve) => setTimeout(resolve, 15));
  assert.equal(worker.terminated, false);
  run = engine.run();
  const controller = new AbortController();
  const release = engine.releaseCompilerResources({ signal: controller.signal });
  assert.equal(worker.terminated, false);
  controller.abort();
  assert.equal(await release, 'cancelled');
  await complete(worker);
  await run;
  assert.equal(worker.terminated, false);
  assert.equal(await engine.releaseCompilerResources(), 'released');
  assert.equal(worker.terminated, true);
  assert.equal(engine.cacheStats.entries, 1);
  const firstDisposal = engine.dispose();
  assert.strictEqual(engine.dispose(), firstDisposal);
  await firstDisposal;
});

test('nonzero guest exit preserves already acknowledged objects', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  const run = engine.run();
  const worker = workers.at(-1);
  await ready(worker);
  record(worker, engine);
  await commit(worker);
  worker.emit({ type: 'stop', exit_code: 1, build_ms: 1, run_ms: 1 });
  await run;
  assert.equal(engine.cacheStats.entries, 1);
  await engine.dispose();
});

test('Stop during parent validation waits for its drain and prevents late commit/ack', async () => {
  const engine = await Engine.create('c', options());
  engine.debugger.enabled = false;
  const run = engine.run();
  const worker = workers.at(-1);
  await ready(worker);
  record(worker, engine);
  let finishValidation;
  engine.objects.validateBatch = () =>
    new Promise((resolve) => {
      finishValidation = resolve;
    });
  worker.emit({ type: 'object_build_complete' });
  let stopped = false;
  const stop = engine.cancelCurrentOperation().then(() => {
    stopped = true;
  });
  await Promise.resolve();
  assert.equal(stopped, false);
  assert.equal((await run).type, 'stopped');
  assert.equal((await engine.run()).type, 'error');
  finishValidation(true);
  await stop;
  assert.equal(engine.cacheStats.entries, 0);
  assert.equal(worker.messages.filter((message) => message.type === 'object_build_ack').length, 0);
  await engine.dispose();
});

// Public transport/lease tests use the real fixed profile registry and mocked
// worker messages; they do not prove compiler admission or grant cache trust.
test('stock validation owns its plan and binds eligibility to the current operation', async () => {
  const engine = await Engine.create('c', options({ toolchainProfile: 'stock-certified-v1' }));
  engine.debugger.enabled = false;
  engine.fs = { 'main.cpp': 'int main(){}' };
  engine.cppArtifacts = { cacheCertificate: 'exact metadata bytes' };
  const preparation = engine.prepareCompiler({ validateCacheInputs: true });
  const worker = workers.at(-1);
  engine.fs['main.cpp'] = 'later';
  await ready(worker);
  const start = worker.messages.at(-1);
  assert.equal(start.validate_cache_inputs, true);
  assert.equal(start.fs['main.cpp'], 'int main(){}');
  assert.equal(start.cpp_artifacts.cacheCertificate, 'exact metadata bytes');
  const certificate = 'a'.repeat(64),
    snapshot = 'b'.repeat(64);
  worker.emit({
    type: 'cpp_cache_eligibility',
    eligible: true,
    certificate_sha256: certificate,
    snapshot,
    operationId: start.operation_id - 1
  });
  assert.equal(engine.cacheStats.cacheEligible, false);
  worker.emit({
    type: 'cpp_cache_eligibility',
    eligible: true,
    certificate_sha256: certificate,
    snapshot
  });
  worker.emit({ type: 'stop', exit_code: 0, build_ms: 1, run_ms: 0 });
  await preparation;
  assert.equal(engine.cacheStats.cacheEligibilityOperationId, start.operation_id);
  assert.equal(engine.cacheStats.cacheEligibilitySnapshot, snapshot);
  assert.equal(engine.cacheStats.cacheEligible, true);
  const compile = engine.compile();
  assert.equal(engine.cacheStats.cacheEligible, false);
  worker.emit({
    type: 'cpp_cache_eligibility',
    eligible: false,
    certificate_sha256: certificate,
    snapshot: null,
    reason: 'stock-temporal'
  });
  await complete(worker);
  await compile;
  assert.equal(engine.cacheStats.cacheEligible, false);
  assert.equal(engine.cacheStats.stockCertificateSha256, certificate);
  assert.equal(engine.cacheStats.cacheEligibilityReason, 'stock-temporal');
  await engine.dispose();
  assert.equal(engine.cacheStats.stockCertificateSha256, undefined);
});

test('zero per-entry metadata capacity keeps stock preparation toolchain-only', async () => {
  const engine = await Engine.create(
    'c',
    options({ toolchainProfile: 'stock-certified-v1', objectCache: { maxEntryMetadataBytes: 0 } })
  );
  engine.fs = { 'main.cpp': '#error not compiled' };
  const preparation = engine.prepareCompiler({ validateCacheInputs: true });
  const worker = workers.at(-1);
  await ready(worker);
  const start = worker.messages.at(-1);
  assert.equal(start.validate_cache_inputs, false);
  assert.deepEqual(start.fs, {});
  worker.emit({ type: 'stop', exit_code: 0, build_ms: 1, run_ms: 0 });
  await preparation;
  assert.equal(engine.cacheStats.cacheEligible, false);
  await engine.dispose();
});

test('failed parent publication clears stock eligibility without failing compilation', async () => {
  const engine = await Engine.create('c', options({ toolchainProfile: 'stock-certified-v1' }));
  engine.debugger.enabled = false;
  const compile = engine.compile();
  const worker = workers.at(-1);
  await ready(worker);
  record(worker, engine);
  worker.emit({
    type: 'cpp_cache_eligibility',
    eligible: true,
    certificate_sha256: 'a'.repeat(64),
    snapshot: 'b'.repeat(64)
  });
  const commitOriginal = engine.objects.commitBatch.bind(engine.objects);
  engine.objects.commitBatch = () => {
    commitOriginal();
    throw new Error('optional cache allocation failure after insertion');
  };
  await complete(worker);
  assert.equal((await compile).exitCode, 0);
  assert.equal(engine.cacheStats.cacheEligible, false);
  assert.equal(engine.cacheStats.cacheEligibilitySnapshot, undefined);
  assert.equal(engine.cacheStats.stockCertificateSha256, 'a'.repeat(64));
  assert.equal(engine.cacheStats.entries, 0);
  assert.equal(engine.cacheStats.stagingEntries, 0);
  assert.equal(engine.cacheStats.cacheEligibilityReason, 'stock-publication-failure');
  await engine.dispose();
});

test('invalid optional certificate metadata cannot change ordinary job transport', async () => {
  for (const cacheCertificate of ['x'.repeat(1024 * 1024 + 1), 'é'.repeat(600000), 42]) {
    const engine = await Engine.create('c', options({ toolchainProfile: 'stock-certified-v1' }));
    engine.debugger.enabled = false;
    engine.fs = { 'main.cpp': 'int main(){}' };
    engine.cppArtifacts = { cacheCertificate, archivePaths: ['/a.a'] };
    const compile = engine.compile();
    const worker = workers.at(-1);
    await ready(worker);
    const start = worker.messages.at(-1);
    assert.equal(start.cpp_artifacts.cacheCertificate, undefined);
    assert.deepEqual(start.cpp_artifacts.archivePaths, ['/a.a']);
    assert.equal(start.fs['main.cpp'], 'int main(){}');
    await complete(worker);
    assert.equal((await compile).exitCode, 0);
    await engine.dispose();
  }
});
