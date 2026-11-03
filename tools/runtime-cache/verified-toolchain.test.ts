import { build } from 'esbuild';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import test, { type TestContext } from 'node:test';

import type { ToolchainSeed } from '../../src/ts/verified-toolchain.ts';

type SeedMessage = { toolchain: ToolchainSeed };

const bundle = await build({
  entryPoints: ['src/ts/verified-toolchain.ts'],
  bundle: true,
  write: false,
  platform: 'node',
  format: 'esm'
});
const { VerifiedToolchain } = await import(
  `data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`
);

const minimal = () => new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]);

// A tiny, fixed test manifest keeps these scheduling/error tests independent of
// the production compiler bytes. Actual hashes are still computed by WebCrypto.
const fixtureDigests = {
  compiler: createHash('sha256').update(minimal()).digest('hex'),
  sysroot: createHash('sha256')
    .update(new Uint8Array([1, 2]))
    .digest('hex'),
  exceptions: 'a'.repeat(64)
};
const loadBundle = await build({
  entryPoints: ['src/ts/verified-toolchain.ts'],
  bundle: true,
  write: false,
  platform: 'node',
  format: 'esm',
  plugins: [
    {
      name: 'fixed-tiny-toolchain-fixture',
      setup(builder) {
        builder.onLoad({ filter: /\/toolchain-manifest\.ts$/ }, () => ({
          contents: `export const LEGACY_TOOLCHAIN_DIGESTS = ${JSON.stringify(fixtureDigests)};
          export function requireToolchainManifest() { return { profile: 'legacy', digests: LEGACY_TOOLCHAIN_DIGESTS }; }`,
          loader: 'ts'
        }));
      }
    }
  ]
});
const { VerifiedToolchain: LoadToolchain } = await import(
  `data:text/javascript;base64,${Buffer.from(loadBundle.outputFiles[0].text).toString('base64')}`
);

function controlledLoad(
  t: TestContext,
  options: {
    cacheModule?: boolean;
    badCompiler?: boolean;
    digestError?: Error;
    holdSysrootHash?: boolean;
    compileError?: Error;
    synchronousCompileError?: boolean;
    signal?: AbortSignal;
  } = {}
) {
  const nativeDigest = crypto.subtle.digest.bind(crypto.subtle);
  const nativeCompile = WebAssembly.compile.bind(WebAssembly);
  const hashes = Promise.withResolvers<void>();
  const sysrootHash = Promise.withResolvers<void>();
  const compilation = Promise.withResolvers<void>();
  const started = Promise.withResolvers<void>();
  let hashCalls = 0;
  let compileCalls = 0;
  let settled = false;
  const cacheModule = options.cacheModule ?? true;
  const notifyStarted = () => {
    if (hashCalls === 2 && compileCalls === (cacheModule ? 1 : 0)) started.resolve();
  };
  t.mock.method(globalThis, 'fetch', async (url: string) => {
    const bytes = url.endsWith('compiler')
      ? options.badCompiler
        ? new Uint8Array([0])
        : minimal()
      : new Uint8Array([1, 2]);
    // Node 22's first Response construction lazily compiles Undici's llhttp
    // WASM. Keep the controlled compile count specific to the loader under test.
    return { ok: true, arrayBuffer: async () => bytes.buffer };
  });
  t.mock.method(crypto.subtle, 'digest', async (...args: Parameters<typeof nativeDigest>) => {
    hashCalls++;
    notifyStarted();
    await hashes.promise;
    if (options.holdSysrootHash && (args[1] as Uint8Array).byteLength === 2)
      await sysrootHash.promise;
    if (options.digestError) throw options.digestError;
    return nativeDigest(...args);
  });
  t.mock.method(WebAssembly, 'compile', (...args: Parameters<typeof nativeCompile>) => {
    compileCalls++;
    notifyStarted();
    if (options.synchronousCompileError) throw options.compileError;
    return compilation.promise.then(() => {
      if (options.compileError) throw options.compileError;
      return nativeCompile(...args);
    });
  });
  const result = LoadToolchain.load(
    ['https://fixture.test/compiler', 'https://fixture.test/sysroot'],
    cacheModule,
    options.signal
  );
  // Observe rejection immediately, including tests that intentionally hold the
  // other operation open. No promise is left to reject without a handler.
  void result.then(
    () => {
      settled = true;
    },
    () => {
      settled = true;
    }
  );
  return {
    result,
    hashes,
    sysrootHash,
    compilation,
    started: started.promise,
    settled: () => settled,
    calls: () => ({ hashCalls, compileCalls })
  };
}

const tick = () => new Promise<void>((resolve) => setImmediate(resolve));

test('asset failure drains the other body read before rejecting', async (t) => {
  const body = Promise.withResolvers<ArrayBuffer>();
  const reading = Promise.withResolvers<void>();
  const failure = new Error('compiler fetch failed');
  let settled = false;
  let compileCalls = 0;
  t.mock.method(globalThis, 'fetch', async (url: string) => {
    if (url.endsWith('compiler')) throw failure;
    return {
      ok: true,
      arrayBuffer() {
        reading.resolve();
        return body.promise;
      }
    };
  });
  t.mock.method(WebAssembly, 'compile', () => {
    compileCalls++;
    throw new Error('must not compile');
  });
  const result = LoadToolchain.load(
    ['https://fixture.test/compiler', 'https://fixture.test/sysroot'],
    true
  );
  void result.then(
    () => {
      settled = true;
    },
    () => {
      settled = true;
    }
  );
  await reading.promise;
  await tick();
  assert.equal(settled, false);
  body.resolve(new ArrayBuffer(2));
  await assert.rejects(result, (error) => error === failure);
  assert.equal(compileCalls, 0);
});

test('load overlaps compilation and hashing but publishes only after both finish', async (t) => {
  const load = controlledLoad(t);
  await load.started;
  load.compilation.resolve();
  await tick();
  assert.equal(load.settled(), false, 'compiled code cannot escape before its hashes');
  load.hashes.resolve();
  const owner = await load.result;
  const messages: SeedMessage[] = [];
  owner.configure(
    {
      postMessage(value: SeedMessage) {
        messages.push(value);
      }
    } as Worker,
    {}
  );
  assert.ok(messages[0].toolchain.module instanceof WebAssembly.Module);
  assert.equal(messages[0].toolchain.compilerHash, fixtureDigests.compiler);
  assert.deepEqual(load.calls(), { hashCalls: 2, compileCalls: 1 });
});

test('load drains an outstanding compilation after a digest failure', async (t) => {
  const failure = new Error('digest failed');
  const load = controlledLoad(t, { digestError: failure });
  await load.started;
  load.hashes.resolve();
  await tick();
  assert.equal(load.settled(), false);
  load.compilation.resolve();
  await assert.rejects(load.result, (error) => error === failure);
});

test('a failed compiler digest also drains the outstanding sysroot digest', async (t) => {
  const failure = new Error('digest failed');
  const load = controlledLoad(t, { digestError: failure, holdSysrootHash: true });
  await load.started;
  load.hashes.resolve();
  load.compilation.resolve();
  await tick();
  assert.equal(load.settled(), false);
  load.sysrootHash.resolve();
  await assert.rejects(load.result, (error) => error === failure);
});

for (const synchronousCompileError of [false, true]) {
  test(`load drains hashes after a ${synchronousCompileError ? 'synchronous' : 'rejected'} compile failure`, async (t) => {
    const failure = new Error('compile failed');
    const load = controlledLoad(t, { compileError: failure, synchronousCompileError });
    await load.started;
    load.compilation.resolve();
    await tick();
    assert.equal(load.settled(), false);
    load.hashes.resolve();
    await assert.rejects(load.result, (error) => error === failure);
  });
}

test('manifest integrity failure remains authoritative over compilation failure', async (t) => {
  const load = controlledLoad(t, { badCompiler: true, compileError: new Error('compile failed') });
  await load.started;
  load.hashes.resolve();
  load.compilation.resolve();
  await assert.rejects(load.result, /toolchain integrity mismatch/);
});

test('abort during compilation drains work and never returns an owner', async (t) => {
  const controller = new AbortController();
  const load = controlledLoad(t, { signal: controller.signal });
  await load.started;
  controller.abort();
  load.hashes.resolve();
  await tick();
  assert.equal(load.settled(), false);
  load.compilation.resolve();
  await assert.rejects(load.result, { name: 'AbortError' });
});

test('module-disabled load verifies identities without compiling', async (t) => {
  const load = controlledLoad(t, { cacheModule: false });
  await load.started;
  load.hashes.resolve();
  const owner = await load.result;
  assert.equal(
    owner.identity,
    `${fixtureDigests.compiler}:${fixtureDigests.sysroot}:${fixtureDigests.exceptions}`
  );
  assert.deepEqual(load.calls(), { hashCalls: 2, compileCalls: 0 });
});

test('verified toolchain owns Buffer/subclass inputs before hashing and keeps seed after transport', async () => {
  class HostileBytes extends Uint8Array {
    slice() {
      return this.subarray();
    }
  }
  const wasm = Buffer.from(minimal());
  const sysroot = new HostileBytes([1, 2]);
  const pending = VerifiedToolchain.fromBytes(wasm, sysroot, true);
  wasm.fill(255);
  sysroot.fill(255);
  const owner = await pending;
  const messages: SeedMessage[] = [];
  const worker = {
    postMessage(value: SeedMessage) {
      messages.push(structuredClone(value));
    }
  } as Worker;
  owner.configure(worker, { type: 'configure_cpp' });
  assert.deepEqual([...messages[0].toolchain.compiler], [...minimal()]);
  assert.deepEqual([...messages[0].toolchain.sysroot], [1, 2]);
  messages[0].toolchain.compiler.fill(255);
  owner.configure(worker, { type: 'configure_cpp' });
  assert.deepEqual([...messages[1].toolchain.compiler], [...minimal()]);
  assert.equal(owner.identityBytes, 10);
});

test('module clone fallback transports exact owned bytes without requiring module clone support', async () => {
  const owner = await VerifiedToolchain.fromBytes(minimal(), new Uint8Array([1]), true);
  const messages: SeedMessage[] = [];
  const worker = {
    postMessage(value: SeedMessage) {
      if (value.toolchain.module) throw new DOMException('unsupported', 'DataCloneError');
      messages.push(value);
    }
  } as Worker;
  owner.configure(worker, {});
  assert.equal(messages.length, 1);
  assert.equal(messages[0].toolchain.module, undefined);
  assert.deepEqual([...messages[0].toolchain.compiler], [...minimal()]);
});

test('companion identity is an expectation and cannot authorize a different compiler', async () => {
  const owner = await VerifiedToolchain.fromBytes(minimal(), new Uint8Array([1]), false);
  const captured: SeedMessage[] = [];
  owner.configure(
    {
      postMessage(value: SeedMessage) {
        captured.push(value);
      }
    } as Worker,
    {}
  );
  const seed = captured[0].toolchain;
  assert.equal(
    seed.stockProfile,
    undefined,
    'Arbitrary owned test bytes cannot acquire stock-cache authority'
  );
  const expectation = {
    compilerSha256: seed.compilerHash,
    sysrootSha256: seed.sysrootHash,
    exceptionsSha256: seed.exceptionsHash
  };
  assert.equal(owner.matchesExpectation(expectation), true);
  assert.equal(owner.matchesExpectation({ ...expectation, compilerSha256: '0'.repeat(64) }), false);
});

test('unknown toolchain profile fails before fetching or trusting caller bytes', async () => {
  const original = globalThis.fetch;
  let calls = 0;
  globalThis.fetch = async () => {
    calls++;
    throw new Error('unexpected fetch');
  };
  try {
    await assert.rejects(
      VerifiedToolchain.load(
        ['https://example.test/compiler', 'https://example.test/sysroot'],
        false,
        undefined,
        'a'.repeat(64),
        'unsupported-profile'
      ),
      /not available/
    );
    assert.equal(calls, 0);
  } finally {
    globalThis.fetch = original;
  }
});
