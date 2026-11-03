import EventEmitter from 'events';

import { cpp_exception_hash, prefetch_urls, WorkerOut, WorkerStart } from '../../pkg/engine';
import init from '../../pkg/engine';
import wasmBinary from '../../pkg/engine_bg.wasm';
import type { CompilerModuleMessage, CompilerModuleStats } from './compiler-module-cache.js';
import { CppCacheLimits, CppObjectCache, ObjectCacheStats } from './cpp-object-cache.js';
import { Debugger } from './debugger.js';
import { createEngineWorker } from './engine-worker';
import { HostDeviceOpener, HostDeviceSession } from './host-device.js';
import { requireToolchainManifest } from './toolchain-manifest.js';
import { errorResult, Internals } from './util.js';
import { VerifiedToolchain } from './verified-toolchain.js';

let initialization: ReturnType<typeof init> | undefined;
export type Lang = 'c' | 'python' | 'rust';
type StdoutMode = 1 | 2;

export interface CppToolchainIdentity {
  compilerSha256: string;
  sysrootSha256: string;
  exceptionsSha256: string;
}

export interface CppArtifacts {
  sources?: string[] | null;
  archives?: string[] | null;
  precompiledHeader?: string | null;
  requiredToolchain?: CppToolchainIdentity | null;
  /** Exact bounded certificate JSON. A supplied document never grants trust. */
  cacheCertificate?: string;
}

/** Immutable ownership boundary. Create a new Engine when either identity changes. */
export interface EngineScope {
  projectId: string;
  accountId: string;
}
export interface CppPerformanceOptions {
  scope: EngineScope;
  objectCache?: boolean | Partial<CppCacheLimits>;
  retainWorker?: boolean;
  cacheCompilerModule?: boolean;
  /** Idle worker lifetime; defaults to 30 seconds, at most five minutes. */
  idleTimeoutMs?: number;
  compilerLifetime?: 'idle' | 'project';
  toolchainProfile?: 'legacy' | 'stock-certified-v1';
}
export interface EngineOptions {
  cpp?: CppPerformanceOptions;
}
export interface CompilerPreparationOptions {
  /** Inspect an owned candidate snapshot for known cache exclusions, without Clang. */
  validateCacheInputs?: boolean;
}
export type { CppCacheLimits, ObjectCacheStats } from './cpp-object-cache.js';

export type Timing = { totalMs: number; buildMs: number; runMs: number };
export type CompletedResult = { type: 'completed'; exitCode: number; timing: Timing };
export type StoppedResult = { type: 'stopped' };
export type ErrorResult = { type: 'error'; error: { type: string; message: string } };
export type RunResult = CompletedResult | StoppedResult | ErrorResult;
export type FsNode = string | DirNode;
export type DirNode = { [name: string]: FsNode };

/** Bounded, source-free compilation measurements from one worker job. */
export interface CppTranslationUnitStatistics {
  hits: number;
  misses: number;
  /** Actual compiler/linker step dispatches; absent in older engine reports. */
  clang_invocations?: number;
  link_invocations?: number;
  bypasses: Array<{ source: string; reason: string }>;
  hit_sources: string[];
  invalid_sources: string[];
  hash_ms: number;
  lookup_ms: number;
  compile_ms: number;
  link_ms: number;
}
export interface CppJobStatistics {
  operation: 'prepare_compiler' | 'compile' | 'run';
  workerReadyMs: number;
  workerReused: boolean;
  cache?: CppTranslationUnitStatistics;
  compilerModule?: CompilerModuleStats;
}
export interface CppCacheStatistics extends ObjectCacheStats {
  generation: number;
  scopeEpoch: number;
  operationId: number;
  cacheEpoch: number;
  toolchainProfile: 'legacy' | 'stock-certified-v1';
  /** Exact registered certificate verified for this operation, including input bypasses. */
  stockCertificateSha256?: string;
  /** Last validated owned snapshot, never authority for a later mutable plan.
   * Preparation means no known exclusion; an ordinary miss still validates its
   * complete observed inputs and diagnostics. Compile/Run means every selected
   * TU was admitted or hit. Cleared when a new operation starts.
   */
  cacheEligible: boolean;
  cacheEligibilityReason?: string;
  cacheEligibilitySnapshot?: string;
  cacheEligibilityOperationId?: number;
  toolchainIdentityBytes: number;
  toolchainIngestions: number;
  toolchainHashCount: number;
  toolchainHashMs: number;
  compilerModuleCloned: boolean;
  workerRetained: boolean;
  workerRetirements: number;
  compilerModuleIdentityBytes: number;
  latest?: CppJobStatistics;
}

interface OperationIdentity {
  scopeEpoch: number;
  operationId: number;
  cacheEpoch: number;
}
type InternalMessage = (
  | WorkerOut
  | CompilerModuleMessage
  | { type: 'object_build_complete' | 'cpp_cache_abandon' }
) &
  Partial<OperationIdentity>;
interface Job extends OperationIdentity {
  generation: number;
  prepareOnly: boolean;
  validateCacheInputs: boolean;
  publication?: Promise<void>;
  settled: boolean;
  buildOnly: boolean;
  debug: boolean;
  start: WorkerStart;
  opener?: HostDeviceOpener;
  device?: HostDeviceSession;
  startedAt: number;
  stats: CppJobStatistics;
  resolve: (result: RunResult) => void;
}
interface WorkerSlot {
  worker: Worker;
  ready: boolean;
  toolchain?: VerifiedToolchain;
  remove: () => void;
}

export class Engine {
  public readonly stdout = new Stdout(1);
  public readonly stderr = new Stdout(2);
  public readonly stdin = new Stdin();
  public readonly debugger: Debugger;
  public readonly lang: Lang;
  public readonly scope?: Readonly<EngineScope>;
  public fs: DirNode = {};
  public binaryFiles: Record<string, Uint8Array> = {};
  public cppArtifacts?: CppArtifacts;
  public hostDevice?: HostDeviceOpener;

  private readonly options?: Readonly<CppPerformanceOptions>;
  private readonly objects: CppObjectCache;
  private generation = 0;
  private scopeEpoch = 0;
  private operationId = 0;
  private disposed = false;
  private active?: Job;
  private promise?: Promise<RunResult>;
  private slot?: WorkerSlot;
  private idleTimer?: ReturnType<typeof setTimeout>;
  private toolchain?: VerifiedToolchain;
  private toolchainLoading?: Promise<VerifiedToolchain>;
  private toolchainController?: AbortController;
  private initializing = new Set<Promise<VerifiedToolchain>>();
  private toolchainIdentity?: string;
  private toolchainIngestions = 0;
  private toolchainHashCount = 0;
  private toolchainHashMs = 0;
  private compilerModuleCloned = false;
  private disposal?: Promise<void>;
  private cancellationDraining?: Promise<void>;
  private releaseDrain?: Promise<void>;
  private releases = new Set<{
    resolve: (value: 'released' | 'cancelled') => void;
    signal?: AbortSignal;
    abort: () => void;
  }>();
  private retirements = 0;
  private latest?: CppJobStatistics;
  private cacheValidation?: {
    eligible: boolean;
    certificate?: string;
    snapshot?: string;
    operationId: number;
    reason?: string;
  };

  static async create(lang: Lang, options: EngineOptions = {}): Promise<Engine> {
    const capturedOptions = structuredClone(options);
    initialization ??= init({ module_or_path: wasmBinary }).catch((error: unknown) => {
      initialization = undefined;
      throw error;
    });
    await initialization;
    const engine = new Engine(lang, capturedOptions);
    if (lang !== 'c' && typeof window !== 'undefined' && typeof fetch !== 'undefined')
      for (const url of prefetch_urls(lang))
        void fetch(url, { cache: 'force-cache' })
          .then((response) => response.arrayBuffer())
          .catch(() => {});
    return engine;
  }

  private constructor(lang: Lang, options: EngineOptions) {
    const cpp = options.cpp;
    if (cpp) {
      if (lang !== 'c') throw new TypeError('C++ performance options require C/C++');
      if (!cpp.scope?.projectId || !cpp.scope.accountId)
        throw new TypeError('C++ performance options require project and account identity');
      const idleTimeoutMs = cpp.idleTimeoutMs ?? 30_000;
      if (!Number.isSafeInteger(idleTimeoutMs) || idleTimeoutMs < 1 || idleTimeoutMs > 300_000)
        throw new RangeError('Compiler idle timeout must be between 1 and 300000 milliseconds');
      if (cpp.compilerLifetime !== undefined && !['idle', 'project'].includes(cpp.compilerLifetime))
        throw new TypeError('Invalid compiler lifetime');
      requireToolchainManifest(cpp.toolchainProfile);
      this.scope = Object.freeze({ ...cpp.scope });
      this.options = Object.freeze({ ...cpp, scope: this.scope, idleTimeoutMs });
    }
    this.objects = new CppObjectCache(typeof cpp?.objectCache === 'object' ? cpp.objectCache : {});
    this.debugger = new Debugger();
    this.lang = lang;
  }

  public get cacheStats(): CppCacheStatistics {
    return structuredClone({
      ...this.objects.stats(),
      generation: this.generation,
      scopeEpoch: this.scopeEpoch,
      operationId: this.operationId,
      cacheEpoch: this.generation,
      toolchainProfile: this.options?.toolchainProfile ?? 'legacy',
      cacheEligible: this.cacheValidation?.eligible ?? false,
      stockCertificateSha256: this.cacheValidation?.certificate,
      cacheEligibilitySnapshot: this.cacheValidation?.snapshot,
      cacheEligibilityOperationId: this.cacheValidation?.operationId,
      cacheEligibilityReason: this.cacheValidation?.reason,
      toolchainIdentityBytes: this.toolchain?.identityBytes ?? 0,
      toolchainIngestions: this.toolchainIngestions,
      toolchainHashCount: this.toolchainHashCount,
      toolchainHashMs: this.toolchainHashMs,
      compilerModuleCloned: this.compilerModuleCloned,
      workerRetained: !!this.slot && !this.active,
      workerRetirements: this.retirements,
      compilerModuleIdentityBytes: this.options?.cacheCompilerModule
        ? (this.toolchain?.compilerBytes ?? 0)
        : 0,
      latest: this.active?.stats ?? this.latest
    });
  }

  /** Clears all retained code/objects. An active job may finish but cannot repopulate them. */
  public clearCaches(): void {
    this.cacheValidation = undefined;
    this.generation++;
    this.objects.clear();
    this.toolchain = undefined;
    if (!this.active) {
      this.toolchainController?.abort();
      this.toolchainLoading = undefined;
    }
    if (!this.active) this.retireWorker();
  }

  /** Stops synchronously, drops retained resources, and resolves after job cleanup. */
  public stop(): Promise<void> {
    this.clearCaches();
    return this.cancelCurrentOperation();
  }

  /** Cancels execution without discarding committed immutable compiler resources. */
  public cancelCurrentOperation(): Promise<void> {
    if (this.cancellationDraining) return this.cancellationDraining;
    this.operationId++;
    const drain = this.promise;
    const publication = this.active?.publication;
    this.retireWorker();
    this.objects.discardBatch();
    if (this.active) this.finish(this.active, { type: 'stopped' });
    const barrier = Promise.allSettled([drain, publication]).then(() => {
      if (this.cancellationDraining === barrier) this.cancellationDraining = undefined;
    });
    this.cancellationDraining = barrier;
    return barrier;
  }

  /** Release idle compiler resources, or defer behind an active guest without stopping it. */
  public releaseCompilerResources(
    options: { signal?: AbortSignal } = {}
  ): Promise<'released' | 'cancelled'> {
    const signal = options.signal;
    if (signal?.aborted) return Promise.resolve('cancelled');
    if (this.disposed) return Promise.resolve('released');
    return new Promise((resolve) => {
      const request = { resolve, signal, abort: () => this.settleRelease(request, 'cancelled') };
      this.releases.add(request);
      signal?.addEventListener('abort', request.abort, { once: true });
      this.flushReleases();
    });
  }

  private settleRelease(
    request: {
      resolve: (value: 'released' | 'cancelled') => void;
      signal?: AbortSignal;
      abort: () => void;
    },
    result: 'released' | 'cancelled'
  ): void {
    if (!this.releases.delete(request)) return;
    request.signal?.removeEventListener('abort', request.abort);
    request.resolve(result);
  }

  private flushReleases(): void {
    if (this.active || !this.releases.size || this.releaseDrain) return;
    if (this.initializing.size) {
      this.toolchainController?.abort();
      const pending = [...this.initializing];
      this.toolchainLoading = undefined;
      this.releaseDrain = Promise.allSettled(pending).then(() => {
        this.releaseDrain = undefined;
        this.flushReleases();
      });
      return;
    }
    this.retireWorker();
    this.toolchainController?.abort();
    this.toolchain = undefined;
    this.toolchainLoading = undefined;
    for (const request of this.releases) this.settleRelease(request, 'released');
  }

  /** Permanent synchronous fence and idempotent cleanup barrier. */
  public dispose(): Promise<void> {
    if (this.disposal) return this.disposal;
    this.disposed = true;
    this.scopeEpoch++;
    this.fs = {};
    this.binaryFiles = {};
    this.cppArtifacts = undefined;
    this.hostDevice = undefined;
    this.stdout.removeAllListeners();
    this.stderr.removeAllListeners();
    this.debugger.removeAllListeners();
    const loading = [...this.initializing];
    this.toolchainController?.abort();
    this.disposal = Promise.allSettled([this.stop(), ...loading]).then(() => {
      this.latest = undefined;
      this.flushReleases();
    });
    return this.disposal;
  }

  /** Load compiler resources. Optional input validation never invokes Clang, links, or runs a guest. */
  public prepareCompiler(options?: CompilerPreparationOptions): Promise<RunResult> {
    if (
      options?.validateCacheInputs !== undefined &&
      typeof options.validateCacheInputs !== 'boolean'
    )
      return Promise.resolve(errorResult(new TypeError('Invalid compiler preparation options')));
    const validate =
      options?.validateCacheInputs === true &&
      this.options?.toolchainProfile === 'stock-certified-v1' &&
      !!this.options?.objectCache &&
      this.objects.limits.maxEntries > 0 &&
      this.objects.limits.maxObjectBytes > 0 &&
      this.objects.limits.maxMetadataBytes > 0 &&
      this.objects.limits.maxEntryMetadataBytes > 0;
    return this.submit(true, true, validate);
  }

  /** Every Run links current objects and starts a fresh student runtime. */
  public run(): Promise<RunResult> {
    return this.submit(false);
  }

  /** Compiles Run-profile C++ translation units only; never links, runs, or opens devices. */
  public compile(): Promise<RunResult> {
    return this.submit(true);
  }

  private submit(
    buildOnly: boolean,
    prepareOnly = false,
    validateCacheInputs = false
  ): Promise<RunResult> {
    if (this.disposed) return Promise.resolve(errorResult(new Error('Engine is disposed')));
    if (this.active || this.cancellationDraining)
      return Promise.resolve(errorResult(new Error('An engine job is already active')));
    if (buildOnly && this.lang !== 'c')
      return Promise.resolve(errorResult(new Error('Compile-only is supported for C/C++')));
    this.cacheValidation = undefined;
    const omitInputs = prepareOnly && !validateCacheInputs;
    let snapshot: Pick<WorkerStart, 'fs' | 'binary_files' | 'cpp_artifacts'>;
    try {
      const artifacts = omitInputs || !this.cppArtifacts ? null : { ...this.cppArtifacts };
      if (artifacts?.cacheCertificate !== undefined) {
        const raw = artifacts.cacheCertificate;
        // Optional metadata cannot force an unbounded clone or a malformed
        // WorkerStart. Valid certificate strings remain byte-exact.
        if (
          typeof raw !== 'string' ||
          raw.length > 1024 * 1024 ||
          new TextEncoder().encode(raw).byteLength > 1024 * 1024
        )
          delete artifacts.cacheCertificate;
      }
      snapshot = {
        fs: omitInputs ? {} : structuredClone(this.fs),
        binary_files: Object.fromEntries(
          Object.entries(omitInputs ? {} : this.binaryFiles).map(([path, bytes]) => [
            path,
            new Uint8Array(bytes)
          ])
        ),
        cpp_artifacts: structuredClone(artifacts)
      };
    } catch (error) {
      return Promise.resolve(errorResult(error));
    }
    const debug = !buildOnly && this.debugger.enabled;
    const opener = buildOnly ? undefined : this.hostDevice;
    if (
      opener !== undefined &&
      (typeof opener !== 'function' || !['c', 'python'].includes(this.lang))
    )
      return Promise.resolve(errorResult(new TypeError('Invalid host device configuration')));
    clearTimeout(this.idleTimer);
    if (debug) {
      this.retireWorker();
    }
    this.stdin[Internals].clear();
    const reused = !!this.slot;
    let resolve!: (value: RunResult) => void;
    const promise = new Promise<RunResult>((done) => {
      resolve = done;
    });
    this.promise = promise;
    const job: Job = {
      generation: this.generation,
      scopeEpoch: this.scopeEpoch,
      operationId: ++this.operationId,
      cacheEpoch: this.generation,
      prepareOnly,
      validateCacheInputs,
      settled: false,
      buildOnly,
      debug,
      opener,
      startedAt: performance.now(),
      resolve,
      stats: {
        operation: prepareOnly ? 'prepare_compiler' : buildOnly ? 'compile' : 'run',
        workerReadyMs: 0,
        workerReused: reused
      },
      start: {
        ...snapshot,
        lang: this.lang,
        prepare_compiler: prepareOnly,
        validate_cache_inputs: validateCacheInputs,
        scope_epoch: this.scopeEpoch,
        operation_id: this.operationId,
        cache_epoch: this.generation,
        is_debug: debug,
        build_only: buildOnly,
        stdin_buffer: this.stdin[Internals].buffer,
        cpp_cache:
          this.options?.objectCache && !debug && !prepareOnly
            ? {
                generation: this.generation,
                entries: this.objects.snapshot(),
                max_entry_metadata_bytes: Math.min(
                  this.objects.limits.maxEntryMetadataBytes,
                  this.objects.limits.maxMetadataBytes,
                  1024 * 1024
                )
              }
            : null
      }
    };
    this.objects.beginBatch();
    this.active = job;
    try {
      if (!this.slot) this.makeWorker();
      if (!this.current(job)) return promise;
      const slot = this.slot!;
      if (this.lang === 'c' && !this.toolchain) {
        // Fetch/verify immutable code while the fresh worker initializes. The
        // ready event still gates dispatch, configuration and every guest job.
        void this.ensureToolchain().catch((error) => {
          if (this.current(job) && this.slot === slot) this.finish(job, errorResult(error));
        });
      }
      if (this.slot!.ready) void this.dispatch(job);
    } catch (error) {
      this.finish(job, errorResult(error));
    }
    return promise;
  }

  private current(job: Job): boolean {
    return this.active === job && !job.settled;
  }

  private makeWorker(): void {
    const worker = createEngineWorker();
    const slot: WorkerSlot = { worker, ready: false, remove: () => {} };
    this.slot = slot;
    const onMessage = (event: MessageEvent<InternalMessage>) => {
      if (this.slot !== slot) return;
      const message = event.data;
      const job = this.active;
      if (message.type === 'ready') {
        if (slot.ready) return;
        slot.ready = true;
        if (job && this.current(job)) {
          void this.dispatch(job);
        }
        return;
      }
      if (!job || !this.current(job)) return;
      if (!slot.ready && message.type === 'error') {
        this.finish(job, errorResult(new Error(message.message)));
        return;
      }
      if (message.operationId !== job.operationId || message.scopeEpoch !== job.scopeEpoch) return;
      try {
        if (message.type === 'cpp_cache_eligibility') {
          if (
            this.options?.toolchainProfile !== 'stock-certified-v1' ||
            (job.prepareOnly && !job.validateCacheInputs)
          )
            return;
          const digest = (value: unknown): value is string =>
            typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);
          const valid =
            message.eligible === true &&
            digest(message.certificate_sha256) &&
            digest(message.snapshot);
          this.cacheValidation = {
            eligible: valid,
            certificate: digest(message.certificate_sha256)
              ? message.certificate_sha256
              : undefined,
            snapshot: valid ? message.snapshot! : undefined,
            operationId: job.operationId,
            reason: valid
              ? undefined
              : typeof message.reason === 'string'
                ? message.reason.slice(0, 128)
                : 'stock-input-validation'
          };
        } else if (message.type === 'stop') {
          this.finish(job, {
            type: 'completed',
            exitCode: message.exit_code,
            timing: {
              totalMs: performance.now() - job.startedAt,
              buildMs: message.build_ms,
              runMs: message.run_ms
            }
          });
        } else if (message.type === 'error') {
          this.finish(job, errorResult(new Error(message.message)));
        } else if (
          message.type === 'cpp_cache_object' &&
          message.generation === this.generation &&
          job.generation === this.generation
        ) {
          try {
            this.objects.stage(message);
          } catch {
            this.objects.abandonBatch();
          }
        } else if (message.type === 'cpp_cache_stats' && message.generation === job.generation) {
          job.stats.cache = message.stats;
        } else if (message.type === 'cpp_cache_abandon') {
          this.objects.abandonBatch();
        } else if (message.type === 'object_build_complete') {
          job.publication ??= this.publishObjects(job, slot);
        } else if (message.type === 'compiler_module_stats') {
          job.stats.compilerModule = message;
        }
      } catch (error) {
        this.finish(job, errorResult(error));
      }
    };
    const onError = (event: ErrorEvent) => {
      if (this.slot !== slot) return;
      this.clearCaches();
      if (this.active)
        this.finish(this.active, errorResult(event.error ?? new Error(event.message)));
      else this.retireWorker();
    };
    const onMessageError = () =>
      onError({ message: 'Unreadable execution worker message' } as ErrorEvent);
    // DAP must observe completion before central cleanup retires a Debug worker.
    this.debugger[Internals].attach(worker);
    worker.addEventListener('message', onMessage);
    worker.addEventListener('error', onError);
    worker.addEventListener('messageerror', onMessageError);
    slot.remove = () => {
      worker.removeEventListener('message', onMessage);
      worker.removeEventListener('error', onError);
      worker.removeEventListener('messageerror', onMessageError);
      this.debugger[Internals].detach(worker);
    };
  }

  private async publishObjects(job: Job, slot: WorkerSlot): Promise<void> {
    let valid = false;
    try {
      valid = await this.objects.validateBatch();
    } catch {
      /* Cache failure is optional. */
    }
    if (!this.current(job) || this.slot !== slot) return;
    try {
      if (valid && job.cacheEpoch === this.generation) this.objects.commitBatch();
      else {
        this.objects.abandonBatch();
        if (this.cacheValidation?.operationId === job.operationId)
          this.cacheValidation = {
            ...this.cacheValidation,
            eligible: false,
            snapshot: undefined,
            reason: 'stock-publication-budget-or-validation'
          };
      }
    } catch {
      // A Map insertion can fail after partial publication. Clear both maps
      // and accounting before acknowledging optional-cache failure.
      this.objects.clear();
      if (this.cacheValidation?.operationId === job.operationId)
        this.cacheValidation = {
          ...this.cacheValidation,
          eligible: false,
          snapshot: undefined,
          reason: 'stock-publication-failure'
        };
    }
    if (this.current(job))
      slot.worker.postMessage({
        type: 'object_build_ack',
        scopeEpoch: job.scopeEpoch,
        operationId: job.operationId,
        cacheEpoch: job.cacheEpoch
      });
  }

  private ensureToolchain(): Promise<VerifiedToolchain> {
    if (this.toolchain) return Promise.resolve(this.toolchain);
    if (this.toolchainLoading) return this.toolchainLoading;
    this.toolchainController = new AbortController();
    const pending = VerifiedToolchain.load(
      prefetch_urls('c'),
      !!this.options?.cacheCompilerModule,
      this.toolchainController.signal,
      cpp_exception_hash(),
      this.options?.toolchainProfile
    );
    this.toolchainLoading = pending;
    this.initializing.add(pending);
    void pending.then(
      () => this.initializing.delete(pending),
      () => {
        this.initializing.delete(pending);
        // A cancelled job may no longer have a dispatch catch, but its rejected
        // load must not poison the next submission or clear a newer load.
        if (this.toolchainLoading === pending) this.toolchainLoading = undefined;
      }
    );
    return pending;
  }

  private async dispatch(job: Job): Promise<void> {
    const slot = this.slot;
    if (!this.current(job) || !slot) return;
    try {
      if (this.lang === 'c') {
        const scopeEpoch = this.scopeEpoch;
        const cacheEpoch = this.generation;
        const pending = this.ensureToolchain();
        const toolchain = this.toolchain ?? (await pending);
        if (!this.current(job) || this.slot !== slot || scopeEpoch !== this.scopeEpoch) return;
        const expectation = job.start.cpp_artifacts?.requiredToolchain;
        if (expectation && !toolchain.matchesExpectation(expectation)) {
          this.finish(job, {
            type: 'error',
            error: {
              type: 'artifact-toolchain-integrity',
              message: 'C++ artifacts require a different compiler, sysroot, or exception runtime'
            }
          });
          return;
        }
        if (cacheEpoch === this.generation) {
          if (this.toolchainIdentity && this.toolchainIdentity !== toolchain.identity) {
            this.objects.clear();
            job.start.cpp_cache = null;
          }
          if (this.toolchain !== toolchain) {
            this.toolchainIngestions++;
            this.toolchainHashCount += toolchain.hashCount;
            this.toolchainHashMs += toolchain.hashMs;
          }
          this.toolchainIdentity = toolchain.identity;
          this.toolchain = toolchain;
          if (this.toolchainLoading === pending) this.toolchainLoading = undefined;
        }
        if (slot.toolchain !== toolchain) {
          this.compilerModuleCloned = toolchain.configure(slot.worker, {
            type: 'configure_cpp',
            cacheCompilerModule: !!this.options?.cacheCompilerModule
          });
          slot.toolchain = toolchain;
        }
      }
      if (!this.current(job) || this.slot !== slot) return;
      const worker = slot.worker;
      job.stats.workerReadyMs = performance.now() - job.startedAt;
      const accepts = (message: Partial<OperationIdentity>) =>
        this.current(job) &&
        message.scopeEpoch === job.scopeEpoch &&
        message.operationId === job.operationId;
      this.stdout[Internals].attach(worker, accepts);
      this.stderr[Internals].attach(worker, accepts);
      if (!job.prepareOnly) this.debugger[Internals].beginRun();
      if (job.opener) {
        job.device = new HostDeviceSession((error) => this.finish(job, errorResult(error)));
        job.device.attach(worker);
        job.device.open(job.opener);
        if (!this.current(job)) return;
        job.start.host_device = job.device.workerStart;
      }
      if (this.current(job)) {
        worker.postMessage(job.start);
        // Structured clone has consumed the snapshot; do not retain a second job copy.
        job.start.fs = {};
        job.start.binary_files = {};
        job.start.cpp_artifacts = null;
        job.start.cpp_cache = null;
      }
    } catch (error) {
      if (this.current(job)) {
        this.toolchainLoading = undefined;
        this.finish(job, errorResult(error));
      }
    }
  }

  private finish(job: Job, result: RunResult): void {
    if (!this.current(job)) return;
    if (result.type !== 'completed' || result.exitCode !== 0) this.cacheValidation = undefined;
    // Fence before cleanup or any observable callback can dispatch queued-ready work.
    job.settled = true;
    const worker = this.slot?.worker;
    if (worker) {
      this.stdout[Internals].detach(worker);
      this.stderr[Internals].detach(worker);
    }
    const cleanupError = job.device?.close();
    if (cleanupError) result = errorResult(cleanupError);
    const healthy =
      result.type === 'completed' &&
      result.exitCode === 0 &&
      !job.debug &&
      job.generation === this.generation &&
      !this.disposed;
    if (job.generation === this.generation)
      for (const source of job.stats.cache?.invalid_sources ?? []) this.objects.delete(source);
    if (healthy) {
      for (const source of job.stats.cache?.hit_sources ?? []) this.objects.touch(source);
    }
    this.objects.discardBatch();
    this.latest = job.stats;
    this.stdin[Internals].clear();
    if (!healthy || !this.options?.retainWorker || this.lang !== 'c') this.retireWorker();
    if (
      !this.disposed &&
      job.cacheEpoch === this.generation &&
      this.toolchain &&
      this.options?.compilerLifetime !== 'project' &&
      (this.options?.retainWorker || this.options?.cacheCompilerModule)
    )
      this.idleTimer = setTimeout(() => {
        this.retireWorker();
        this.toolchain = undefined;
        this.toolchainLoading = undefined;
      }, this.options.idleTimeoutMs);
    if (!this.options?.retainWorker && !this.options?.cacheCompilerModule)
      this.toolchain = undefined;
    if (job.cacheEpoch !== this.generation) {
      this.toolchainController?.abort();
      this.toolchainLoading = undefined;
    }
    this.active = undefined;
    this.promise = undefined;
    this.flushReleases();
    job.resolve(result);
  }

  private retireWorker(): void {
    clearTimeout(this.idleTimer);
    const slot = this.slot;
    if (!slot) return;
    this.slot = undefined;
    slot.remove();
    this.stdout[Internals].detach(slot.worker);
    this.stderr[Internals].detach(slot.worker);
    // Termination is asynchronous; queued errors still belong to this retired
    // worker. Keep them from bubbling without retaining the Engine or its job.
    slot.worker.onerror = (event) => event.preventDefault();
    slot.worker.terminate();
    this.retirements++;
  }
}

export class Stdout extends EventEmitter<{ data: [Uint8Array] }> {
  private readonly callback: (event: MessageEvent<WorkerOut & Partial<OperationIdentity>>) => void;
  private accepts?: (message: Partial<OperationIdentity>) => boolean;

  [Internals]: {
    attach(worker: Worker, accepts?: (message: Partial<OperationIdentity>) => boolean): void;
    detach(worker: Worker): void;
  };

  constructor(private readonly mode: StdoutMode) {
    super();
    this.callback = ((event: MessageEvent<WorkerOut & Partial<OperationIdentity>>) => {
      const msg = event.data;
      if (this.accepts && !this.accepts(msg)) return;
      if (msg.type !== 'stdout') return;
      if (msg.mode !== this.mode) return;
      const chunk = msg.data as Uint8Array<ArrayBuffer>;
      this.emit('data', chunk);
    }).bind(this);

    this[Internals] = {
      attach: this.attach.bind(this),
      detach: this.detach.bind(this)
    };
  }

  private attach(worker: Worker, accepts?: (message: Partial<OperationIdentity>) => boolean) {
    this.accepts = accepts;
    worker.addEventListener('message', this.callback);
  }

  private detach(worker: Worker) {
    worker.removeEventListener('message', this.callback);
  }
}

export class Stdin {
  /**
   * Ring buffer to store stdin data.
   *
   * - TypeScript controls write_index, Rust controls read_index
   * - One slot is always kept empty to distinguish full from empty
   */

  private static readonly BUFFER_SIZE = 2048;
  private static readonly HEADER_SIZE = 8; // 2 x i32
  private static readonly DATA_SIZE = Stdin.BUFFER_SIZE - Stdin.HEADER_SIZE;
  private static readonly READ_IDX = 0;
  private static readonly WRITE_IDX = 1;
  private static encoder = new TextEncoder();

  private buffer = new SharedArrayBuffer(Stdin.BUFFER_SIZE);
  private generation = 0;
  private indices: Int32Array;
  private data: Int8Array;

  [Internals]: {
    clear(): void;
    buffer: SharedArrayBuffer;
  };

  constructor() {
    this.indices = new Int32Array(this.buffer, 0, 2);
    this.data = new Int8Array(this.buffer, Stdin.HEADER_SIZE);
    const getBuffer = () => this.buffer;
    this[Internals] = {
      clear: this.clear.bind(this),
      get buffer() {
        return getBuffer();
      }
    };
  }

  private clear() {
    this.generation++;
    Atomics.notify(this.indices, Stdin.READ_IDX);
    this.buffer = new SharedArrayBuffer(Stdin.BUFFER_SIZE);
    this.indices = new Int32Array(this.buffer, 0, 2);
    this.data = new Int8Array(this.buffer, Stdin.HEADER_SIZE);
  }

  public async write(value: Uint8Array | string): Promise<void> {
    const chunk = typeof value === 'string' ? Stdin.encoder.encode(value) : value;
    return this.writeBytes(chunk);
  }

  private async writeBytes(chunk: Uint8Array): Promise<void> {
    const { DATA_SIZE, READ_IDX, WRITE_IDX } = Stdin;
    let offset = 0;
    const generation = this.generation;

    while (offset < chunk.length && generation === this.generation) {
      const readIdx = Atomics.load(this.indices, READ_IDX);
      let writeIdx = Atomics.load(this.indices, WRITE_IDX);

      if (writeIdx === DATA_SIZE - 1 && readIdx > 0) writeIdx = 0;
      const available = readIdx <= writeIdx ? DATA_SIZE - writeIdx - 1 : readIdx - writeIdx - 1;

      if (available === 0) {
        await Atomics.waitAsync(this.indices, READ_IDX, readIdx).value;
        continue;
      }

      const toWrite = Math.min(chunk.length - offset, available);
      this.data.set(chunk.subarray(offset, offset + toWrite), writeIdx);

      // Write index & notify reader
      Atomics.store(this.indices, WRITE_IDX, (writeIdx + toWrite) % DATA_SIZE);
      Atomics.notify(this.indices, WRITE_IDX);
      offset += toWrite;
    }
  }
}

export * from './debugger.js';
export { HOST_DEVICE_PATH } from './host-device.js';
export type { HostDevice, HostDeviceOpener } from './host-device.js';
