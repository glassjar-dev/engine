/** Guest-visible path of the optional, per-run duplex byte device. */
export const HOST_DEVICE_PATH = '/dev/debugger-sh-host';

export interface HostDevice {
  /** Aborted on completion, stop, or callback failure. */
  readonly signal: AbortSignal;
  /** Arbitrary byte chunks. Returns an unsubscribe function. */
  onData(listener: (chunk: Uint8Array) => void): () => void;
  /** Snapshots bytes; resolves when published. Await before writing again. */
  write(chunk: Uint8Array): Promise<void>;
}

export type HostDeviceOpener = (device: HostDevice) => void | (() => void);

// Each direction has one fixed slot: length 0 = empty, -1 = closed.
class Mailbox {
  readonly buffer = new SharedArrayBuffer(4 + 64 * 1024);
  readonly length = new Int32Array(this.buffer, 0, 1);
  readonly bytes = new Uint8Array(this.buffer, 4);

  publish(length: number) {
    Atomics.store(this.length, 0, length);
    Atomics.notify(this.length, 0);
  }
}

function callbackError(reason: unknown): Error {
  try {
    return new Error(
      `Host device callback failed: ${reason instanceof Error ? reason.message : String(reason)}`
    );
  } catch {
    return new Error('Host device callback failed');
  }
}

function runCleanup(cleanup?: () => void): Error | undefined {
  try {
    const result: unknown = cleanup?.();
    if (result && typeof (result as { then?: unknown }).then === 'function') {
      void Promise.resolve(result).catch(() => {});
      return new TypeError('Host device cleanup must be synchronous');
    }
  } catch (reason) {
    return callbackError(reason);
  }
}

/** Internal transport; framing belongs to the application. */
export class HostDeviceSession {
  private readonly incoming = new Mailbox();
  private readonly outgoing = new Mailbox();
  readonly workerStart = {
    guest_to_host: this.incoming.buffer,
    host_to_guest: this.outgoing.buffer
  };
  private readonly abort = new AbortController();
  private readonly listeners = new Set<(chunk: Uint8Array) => void>();
  private cleanup?: () => void;
  private worker?: Worker;
  private writing = false;

  constructor(private readonly onFailure: (error: Error) => void) {}

  open(opener: HostDeviceOpener) {
    let cleanup: ReturnType<HostDeviceOpener>;
    try {
      cleanup = opener({
        signal: this.abort.signal,
        onData: (listener) => {
          this.assertOpen();
          if (typeof listener !== 'function')
            throw new TypeError('Host device listener must be a function');
          this.listeners.add(listener);
          return () => {
            this.listeners.delete(listener);
          };
        },
        write: (bytes) => this.write(bytes)
      });
    } catch (reason) {
      throw callbackError(reason);
    }
    if (cleanup !== undefined && typeof cleanup !== 'function') {
      void Promise.resolve(cleanup).catch(() => {});
      throw new TypeError('Host device opener must return a cleanup function or undefined');
    }
    if (typeof cleanup === 'function') {
      if (this.abort.signal.aborted) {
        const error = runCleanup(cleanup);
        if (error) throw error;
      } else this.cleanup = cleanup;
    }
  }

  attach(worker: Worker) {
    this.assertOpen();
    this.worker = worker;
    worker.addEventListener('message', this.onMessage);
  }

  private assertOpen() {
    if (this.abort.signal.aborted) throw new Error('Host device is closed');
  }

  private async write(bytes: Uint8Array): Promise<void> {
    this.assertOpen();
    if (!(bytes instanceof Uint8Array))
      throw new TypeError('Host device writes require Uint8Array');
    if (this.writing) throw new Error('Await the previous host device write before writing again');
    this.writing = true;
    try {
      const snapshot = new Uint8Array(bytes),
        slot = this.outgoing;
      let offset = 0;
      while (offset < snapshot.length) {
        this.assertOpen();
        const length = Atomics.load(slot.length, 0);
        if (length !== 0) {
          await Atomics.waitAsync(slot.length, 0, length).value;
          continue;
        }
        const chunk = snapshot.subarray(offset, offset + slot.bytes.length);
        slot.bytes.set(chunk);
        slot.publish(chunk.length);
        offset += chunk.length;
      }
    } finally {
      this.writing = false;
    }
  }

  private readonly onMessage = (event: MessageEvent<{ type: string }>) => {
    if (this.abort.signal.aborted || event.data?.type !== 'host_wake') return;
    try {
      const length = Atomics.load(this.incoming.length, 0);
      if (length <= 0) return;
      // Copy before releasing the slot; callbacks may synchronously write back.
      const bytes = this.incoming.bytes.slice(0, length);
      this.incoming.publish(0);
      for (const listener of this.listeners) {
        if (this.abort.signal.aborted) break;
        void Promise.resolve(listener(bytes)).catch((reason: unknown) => this.fail(reason));
      }
    } catch (reason) {
      this.fail(reason);
    }
  };

  private fail(reason: unknown) {
    if (this.abort.signal.aborted) return;
    this.close();
    this.onFailure(callbackError(reason));
  }

  close(): Error | undefined {
    if (this.abort.signal.aborted) return;
    this.worker?.removeEventListener('message', this.onMessage);
    this.worker = undefined;
    this.incoming.publish(-1);
    this.outgoing.publish(-1);
    this.abort.abort();
    this.listeners.clear();
    const cleanup = this.cleanup;
    this.cleanup = undefined;
    return runCleanup(cleanup);
  }
}
