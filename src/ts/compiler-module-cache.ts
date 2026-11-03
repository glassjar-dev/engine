import type { ToolchainSeed } from './verified-toolchain.js';

/** Internal, engine-scoped immutable code. Never store an Instance or Store here. */
export interface RetainedCompilerModule {
  readonly module: WebAssembly.Module;
  readonly bytes: Uint8Array;
  readonly sha256: string;
}

export interface CompilerModuleStats {
  hits: number;
  misses: number;
  bypasses: number;
  hashMs: number;
  compareMs: number;
  compileMs: number;
  instantiateMs: number;
  /** The worker's retained identity copy only; excludes browser compiled code. */
  identityBytes: number;
  sha256?: string;
}

export type CompilerModuleMessage =
  | { type: 'compiler_module'; value: RetainedCompilerModule }
  | ({ type: 'compiler_module_stats' } & CompilerModuleStats);

export interface ConfigureCompilerModule {
  type: 'configure_cpp';
  cacheCompilerModule: boolean;
  compilerModule?: RetainedCompilerModule;
}

/** One immutable compiler Module; called only by explicitly marked compiler steps. */
export class CompilerModuleCache {
  private enabled = false;
  private verified?: ToolchainSeed;
  private verifiedBytes = 0;
  private generation = 0;
  private retained?: RetainedCompilerModule;
  private stats = this.emptyStats();
  private readonly emit: (message: CompilerModuleMessage) => void;

  constructor(emit: (message: CompilerModuleMessage) => void) {
    this.emit = emit;
  }

  configure({ cacheCompilerModule, compilerModule }: ConfigureCompilerModule): void {
    this.generation++;
    this.enabled = cacheCompilerModule;
    this.retained = undefined;
    this.stats = this.emptyStats();
    if (
      cacheCompilerModule &&
      compilerModule?.module instanceof WebAssembly.Module &&
      compilerModule.bytes instanceof Uint8Array &&
      /^[a-f0-9]{64}$/.test(compilerModule.sha256)
    ) {
      // Also own direct callers' input; structured cloning alone is not this contract.
      this.retained = { ...compilerModule, bytes: new Uint8Array(compilerModule.bytes) };
    }
  }

  /** Private worker seed received only from the engine's verified owner. */
  configureVerified(seed: ToolchainSeed, enabled: boolean): void {
    this.enabled = enabled;
    this.verifiedBytes = seed.compiler.byteLength;
    this.verified = { ...seed, compiler: new Uint8Array(), sysroot: new Uint8Array() };
    this.retained = undefined;
    this.stats = this.emptyStats();
  }

  beginOperation(): void {
    this.stats = this.emptyStats();
  }

  constructVerified(bytes: Uint8Array): WebAssembly.Module {
    const seed = this.verified;
    if (!seed) throw new Error('Compiler toolchain is not configured');
    if (seed.module && this.enabled) {
      this.stats.hits++;
      this.publishStats();
      return seed.module;
    }
    this.stats.misses++;
    const module = this.compile(bytes);
    if (this.enabled) seed.module = module;
    this.publishStats();
    return module;
  }

  async construct(bytes: Uint8Array): Promise<WebAssembly.Module> {
    if (!this.enabled) return this.compile(bytes);
    const generation = this.generation;
    const stats = this.stats;

    // wasm-bindgen may supply a view into growable engine memory. Own it before await.
    const owned = new Uint8Array(bytes);
    let sha256: string;
    const hashStart = performance.now();
    try {
      const digest = await crypto.subtle.digest('SHA-256', owned);
      sha256 = Array.from(new Uint8Array(digest), (byte) =>
        byte.toString(16).padStart(2, '0')
      ).join('');
    } catch {
      // Optional caching must never prevent the ordinary compiler path.
      stats.hashMs += performance.now() - hashStart;
      stats.bypasses++;
      if (generation === this.generation) this.retained = undefined;
      return this.compile(owned, stats);
    }
    stats.hashMs += performance.now() - hashStart;
    // An explicit clear/configure during hashing may not repopulate retained code.
    if (generation !== this.generation) return this.compile(owned, stats);

    const compareStart = performance.now();
    const previous = this.retained;
    const hit =
      previous?.sha256 === sha256 &&
      previous.bytes.length === owned.length &&
      equalBytes(previous.bytes, owned);
    this.stats.compareMs += performance.now() - compareStart;
    if (hit) {
      this.stats.hits++;
      this.publishStats();
      return previous.module;
    }

    this.stats.misses++;
    // Release an obsolete identity before compiling its replacement.
    this.retained = undefined;
    const module = this.compile(owned);
    this.retained = { module, bytes: owned, sha256 };
    this.send({ type: 'compiler_module', value: this.retained });
    this.publishStats();
    return module;
  }

  /** Called after a fresh compiler WASI environment and Instance have been created. */
  instantiated(milliseconds: number): void {
    if (Number.isFinite(milliseconds) && milliseconds >= 0)
      this.stats.instantiateMs += milliseconds;
    this.publishStats();
  }

  private compile(bytes: Uint8Array, stats = this.stats): WebAssembly.Module {
    const start = performance.now();
    try {
      return new WebAssembly.Module(bytes as Uint8Array<ArrayBuffer>);
    } finally {
      stats.compileMs += performance.now() - start;
    }
  }

  private publishStats(): void {
    this.send({
      type: 'compiler_module_stats',
      ...this.stats,
      identityBytes: this.verified ? this.verifiedBytes : (this.retained?.bytes.byteLength ?? 0),
      sha256: this.verified?.compilerHash ?? this.retained?.sha256
    });
  }

  private emptyStats(): CompilerModuleStats {
    return {
      hits: 0,
      misses: 0,
      bypasses: 0,
      hashMs: 0,
      compareMs: 0,
      compileMs: 0,
      instantiateMs: 0,
      identityBytes: 0
    };
  }

  private send(message: CompilerModuleMessage): void {
    // Browsers that cannot clone a Module may still use this worker's local code.
    try {
      this.emit(message);
    } catch {
      this.stats.bypasses++;
    }
  }
}

function equalBytes(left: Uint8Array, right: Uint8Array): boolean {
  for (let index = 0; index < left.length; index++) {
    if (left[index] !== right[index]) return false;
  }
  return true;
}
