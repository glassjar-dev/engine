/** Cache storage is private to one Engine; it is never persisted or shared. */
export interface CppCacheLimits {
  maxObjectBytes: number;
  maxEntries: number;
  maxMetadataBytes: number;
  maxEntryMetadataBytes: number;
}

export const DEFAULT_CPP_CACHE_LIMITS: Readonly<CppCacheLimits> = Object.freeze({
  maxObjectBytes: 128 * 1024 * 1024,
  maxEntries: 256,
  maxMetadataBytes: 8 * 1024 * 1024,
  maxEntryMetadataBytes: 1024 * 1024
});

export interface ObjectRecord {
  source: string;
  key: string;
  object_hash: string;
  object: Uint8Array;
  /** Internal validated compiler metadata; never a caller-supplied capability. */
  receipt?: string | null;
}

export interface ObjectCacheStats {
  objectBytes: number;
  metadataBytes: number;
  entries: number;
  stagingEntries: number;
  stagingObjectBytes: number;
  stagingMetadataBytes: number;
  evictions: number;
  oversized: number;
}

export class CppObjectCache {
  private records = new Map<string, ObjectRecord & { metadataBytes: number }>();
  private staged = new Map<string, ObjectRecord & { metadataBytes: number }>();
  private stagingFailed = false;
  private objectBytes = 0;
  private metadataBytes = 0;
  private evictions = 0;
  private oversized = 0;
  public readonly limits: Readonly<CppCacheLimits>;

  constructor(limits: Partial<CppCacheLimits> = {}) {
    this.limits = Object.freeze({ ...DEFAULT_CPP_CACHE_LIMITS, ...limits });
    for (const value of Object.values(this.limits))
      if (!Number.isSafeInteger(value) || value < 0)
        throw new RangeError('C++ cache limits must be nonnegative safe integers');
  }

  beginBatch(): void {
    this.discardBatch();
    this.stagingFailed = false;
  }

  /** Stage against the same storage budget, before copying any optional object. */
  stage(value: ObjectRecord): void {
    if (this.stagingFailed) return;
    const size = this.recordSize(value);
    if (size === undefined || this.staged.has(value.source)) {
      this.failBatch();
      return;
    }
    const limits = this.limits;
    while (
      this.records.size &&
      (this.records.size + this.staged.size >= limits.maxEntries ||
        this.objectBytes + value.object.byteLength > limits.maxObjectBytes ||
        this.metadataBytes + size > limits.maxMetadataBytes)
    ) {
      this.remove(this.records.keys().next().value!);
      this.evictions++;
    }
    if (
      this.records.size + this.staged.size >= limits.maxEntries ||
      this.objectBytes + value.object.byteLength > limits.maxObjectBytes ||
      this.metadataBytes + size > limits.maxMetadataBytes ||
      size > limits.maxEntryMetadataBytes
    ) {
      this.oversized++;
      this.failBatch();
      return;
    }
    this.staged.set(value.source, {
      ...value,
      object: new Uint8Array(value.object),
      metadataBytes: size
    });
    this.objectBytes += value.object.byteLength;
    this.metadataBytes += size;
  }

  abandonBatch(): void {
    this.failBatch();
  }

  private failBatch(): void {
    this.discardBatch();
    this.stagingFailed = true;
  }

  private recordSize(value: ObjectRecord): number | undefined {
    if (
      typeof value.source !== 'string' ||
      !value.source.startsWith('/') ||
      value.source
        .split('/')
        .slice(1)
        .some((part) => !part || part === '.' || part === '..') ||
      typeof value.key !== 'string' ||
      typeof value.object_hash !== 'string' ||
      !/^[a-f0-9]{64}$/.test(value.key) ||
      !/^[a-f0-9]{64}$/.test(value.object_hash) ||
      (value.receipt != null && typeof value.receipt !== 'string') ||
      !(value.object instanceof Uint8Array)
    )
      return;
    return new TextEncoder().encode(
      JSON.stringify({
        source: value.source,
        key: value.key,
        object_hash: value.object_hash,
        ...(value.receipt == null ? {} : { receipt: value.receipt })
      })
    ).byteLength;
  }

  async validateBatch(): Promise<boolean> {
    if (this.stagingFailed) return false;
    const batch = [...this.staged.values()];
    for (const value of batch) {
      if (!relocatableObject(value.object)) return false;
      const hash = await crypto.subtle.digest('SHA-256', value.object as Uint8Array<ArrayBuffer>);
      const hex = Array.from(new Uint8Array(hash), (byte) =>
        byte.toString(16).padStart(2, '0')
      ).join('');
      if (hex !== value.object_hash) return false;
    }
    return true;
  }

  commitBatch(): void {
    for (const [source, record] of this.staged) {
      this.remove(source);
      this.records.set(source, record);
    }
    this.staged.clear();
    this.stagingFailed = false;
  }

  discardBatch(): void {
    for (const record of this.staged.values()) {
      this.objectBytes -= record.object.byteLength;
      this.metadataBytes -= record.metadataBytes;
    }
    this.staged.clear();
  }

  snapshot(): ObjectRecord[] {
    return [...this.records.values()].map(({ source, key, object_hash, object, receipt }) => ({
      source,
      key,
      object_hash,
      ...(receipt == null ? {} : { receipt }),
      object: object.slice()
    }));
  }

  /** Worker hits are the only accesses that move an entry to the LRU tail. */
  touch(source: string): void {
    const record = this.records.get(source);
    if (record) {
      this.records.delete(source);
      this.records.set(source, record);
    }
  }

  put(value: ObjectRecord): void {
    // Treat unreadable records as misses. The worker also verifies object digest and schema.
    if (
      typeof value.source !== 'string' ||
      !value.source.startsWith('/') ||
      value.source
        .split('/')
        .slice(1)
        .some((part) => !part || part === '.' || part === '..') ||
      typeof value.key !== 'string' ||
      typeof value.object_hash !== 'string' ||
      !/^[a-f0-9]{64}$/.test(value.object_hash) ||
      !/^[a-f0-9]{64}$/.test(value.key) ||
      !(value.object instanceof Uint8Array)
    )
      return;
    const metadataBytes = this.recordSize(value);
    if (metadataBytes === undefined) return;
    const limits = this.limits;
    // Replacing an oversized record removes its stale predecessor, but never changes compilation.
    this.remove(value.source);
    if (
      limits.maxEntries === 0 ||
      value.object.byteLength > limits.maxObjectBytes ||
      metadataBytes > limits.maxMetadataBytes ||
      metadataBytes > limits.maxEntryMetadataBytes
    ) {
      this.oversized++;
      return;
    }
    while (
      this.records.size >= limits.maxEntries ||
      this.objectBytes + value.object.byteLength > limits.maxObjectBytes ||
      this.metadataBytes + metadataBytes > limits.maxMetadataBytes
    ) {
      this.remove(this.records.keys().next().value!);
      this.evictions++;
    }
    const record = { ...value, object: new Uint8Array(value.object), metadataBytes };
    this.records.set(value.source, record);
    this.objectBytes += record.object.byteLength;
    this.metadataBytes += metadataBytes;
  }

  delete(source: string): void {
    this.remove(source);
  }

  private remove(source: string): void {
    const previous = this.records.get(source);
    if (!previous) return;
    this.objectBytes -= previous.object.byteLength;
    this.metadataBytes -= previous.metadataBytes;
    this.records.delete(source);
  }

  clear(): void {
    this.records.clear();
    this.staged.clear();
    this.stagingFailed = false;
    this.objectBytes = this.metadataBytes = 0;
  }

  stats(): ObjectCacheStats {
    return {
      objectBytes: this.objectBytes,
      metadataBytes: this.metadataBytes,
      entries: this.records.size,
      stagingEntries: this.staged.size,
      stagingObjectBytes: [...this.staged.values()].reduce(
        (total, value) => total + value.object.byteLength,
        0
      ),
      stagingMetadataBytes: [...this.staged.values()].reduce(
        (total, value) => total + value.metadataBytes,
        0
      ),
      evictions: this.evictions,
      oversized: this.oversized
    };
  }
}

/** The trusted worker also performs wasmparser's full relocation/linking validation. */
function relocatableObject(bytes: Uint8Array): boolean {
  if (!WebAssembly.validate(bytes as Uint8Array<ArrayBuffer>) || bytes.length < 8) return false;
  let offset = 8,
    linking = false;
  const integer = (): number => {
    let value = 0,
      shift = 0;
    for (let count = 0; count < 5 && offset < bytes.length; count++, shift += 7) {
      const byte = bytes[offset++];
      value += (byte & 127) * 2 ** shift;
      if (!(byte & 128)) return value;
    }
    throw new Error('Malformed object section');
  };
  try {
    while (offset < bytes.length) {
      const id = bytes[offset++],
        size = integer(),
        end = offset + size;
      if (end > bytes.length || id === 8) return false;
      if (id === 0) {
        const length = integer();
        if (offset + length > end) return false;
        const name = new TextDecoder().decode(bytes.subarray(offset, offset + length));
        offset += length;
        if (name === 'linking') {
          if (linking || integer() !== 2) return false;
          linking = true;
        }
      }
      offset = end;
    }
    return linking;
  } catch {
    return false;
  }
}
