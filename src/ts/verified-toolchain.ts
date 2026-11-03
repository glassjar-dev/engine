import { LEGACY_TOOLCHAIN_DIGESTS, requireToolchainManifest } from './toolchain-manifest.js';

/** Engine-private immutable toolchain ownership. Seeds never cross the public API. */
export interface ToolchainSeed {
  compiler: Uint8Array;
  sysroot: Uint8Array;
  compilerHash: string;
  sysrootHash: string;
  exceptionsHash: string;
  stockProfile?: boolean;
  module?: WebAssembly.Module;
}

export class VerifiedToolchain {
  #seed: ToolchainSeed;
  readonly hashCount = 3;
  readonly hashMs: number;
  private constructor(seed: ToolchainSeed, hashMs: number) {
    this.#seed = seed;
    this.hashMs = hashMs;
  }

  static async load(
    urls: readonly string[],
    cacheModule: boolean,
    signal?: AbortSignal,
    exceptionsHash: string = LEGACY_TOOLCHAIN_DIGESTS.exceptions,
    profile = 'legacy'
  ): Promise<VerifiedToolchain> {
    const manifest = requireToolchainManifest(profile);
    if (urls.length !== 2) throw new Error('Missing C++ toolchain assets');
    let fetchFailure: { error: unknown } | undefined;
    const assets = await Promise.allSettled(
      urls.map(async (url) => {
        try {
          const response = await fetch(url, { cache: 'force-cache', signal });
          if (!response.ok) throw new Error(`Cannot load C++ toolchain (${response.status})`);
          return new Uint8Array(await response.arrayBuffer());
        } catch (error) {
          fetchFailure ??= { error };
          throw error;
        }
      })
    );
    if (assets[0].status === 'rejected' || assets[1].status === 'rejected')
      throw fetchFailure?.error;
    const [compiler, sysroot] = [assets[0].value, assets[1].value];
    // Both operations consume the same private, immutable bytes. Compiling does
    // not execute them; no module or seed can escape before integrity succeeds.
    // Drain both promises on failure so disposal cannot leave work unobserved.
    const [ingestion, compilation] = await Promise.allSettled([
      VerifiedToolchain.ingestOwned(compiler, sysroot, false, exceptionsHash),
      cacheModule
        ? Promise.resolve().then(() => WebAssembly.compile(compiler as Uint8Array<ArrayBuffer>))
        : Promise.resolve(undefined)
    ]);
    if (ingestion.status === 'rejected') throw ingestion.reason;
    const owner = ingestion.value;
    if (
      owner.#seed.compilerHash !== manifest.digests.compiler ||
      owner.#seed.sysrootHash !== manifest.digests.sysroot ||
      owner.#seed.exceptionsHash !== manifest.digests.exceptions
    )
      throw new Error('C++ toolchain integrity mismatch');
    if (signal?.aborted) throw new DOMException('Toolchain loading aborted', 'AbortError');
    if (compilation.status === 'rejected') throw compilation.reason;
    owner.#seed.stockProfile = manifest.profile === 'stock-certified-v1';
    owner.#seed.module = compilation.value;
    return owner;
  }

  /** Captures even Buffer/subclass inputs before the first asynchronous boundary. */
  static async fromBytes(
    compilerInput: Uint8Array,
    sysrootInput: Uint8Array,
    cacheModule: boolean
  ): Promise<VerifiedToolchain> {
    const compiler = new Uint8Array(compilerInput);
    const sysroot = new Uint8Array(sysrootInput);
    return VerifiedToolchain.ingestOwned(
      compiler,
      sysroot,
      cacheModule,
      LEGACY_TOOLCHAIN_DIGESTS.exceptions
    );
  }

  private static async ingestOwned(
    compiler: Uint8Array<ArrayBuffer>,
    sysroot: Uint8Array<ArrayBuffer>,
    cacheModule: boolean,
    exceptionsHash: string
  ): Promise<VerifiedToolchain> {
    const started = performance.now();
    const hashes = await Promise.allSettled([digest(compiler), digest(sysroot)]);
    if (hashes[0].status === 'rejected') throw hashes[0].reason;
    if (hashes[1].status === 'rejected') throw hashes[1].reason;
    const [compilerHash, sysrootHash] = [hashes[0].value, hashes[1].value];
    const module = cacheModule ? await WebAssembly.compile(compiler) : undefined;
    return new VerifiedToolchain(
      { compiler, sysroot, compilerHash, sysrootHash, exceptionsHash, module },
      performance.now() - started
    );
  }

  matchesExpectation(expected: {
    compilerSha256: string;
    sysrootSha256: string;
    exceptionsSha256: string;
  }): boolean {
    return (
      expected.compilerSha256 === this.#seed.compilerHash &&
      expected.sysrootSha256 === this.#seed.sysrootHash &&
      expected.exceptionsSha256 === this.#seed.exceptionsHash
    );
  }

  get identity(): string {
    return `${this.#seed.compilerHash}:${this.#seed.sysrootHash}:${this.#seed.exceptionsHash}`;
  }
  get identityBytes(): number {
    return this.#seed.compiler.byteLength + this.#seed.sysroot.byteLength;
  }
  get compilerBytes(): number {
    return this.#seed.compiler.byteLength;
  }

  /** postMessage clones the private seed; never transfer/detach the restart owner. */
  configure(worker: Worker, envelope: object): boolean {
    try {
      worker.postMessage({ ...envelope, toolchain: this.#seed });
      return !!this.#seed.module;
    } catch (error) {
      if (!(error instanceof DOMException) || error.name !== 'DataCloneError' || !this.#seed.module)
        throw error;
      worker.postMessage({ ...envelope, toolchain: { ...this.#seed, module: undefined } });
      return false;
    }
  }
}

async function digest(bytes: Uint8Array<ArrayBuffer>): Promise<string> {
  const hash = await crypto.subtle.digest('SHA-256', bytes);
  return Array.from(new Uint8Array(hash), (byte) => byte.toString(16).padStart(2, '0')).join('');
}
