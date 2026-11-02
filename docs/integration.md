# Integration Guide

This guide is for teams building an IDE or editor on top of this engine. It covers setting up code execution and the debugger.

---

## Installation

```sh
npm install glassjar
```

The package ships a WebAssembly binary and TypeScript bindings. Initialize it once before use:

```ts
import { Engine } from 'glassjar';

const engine = await Engine.create('c'); // or 'python'
```

---

## Running Code

Set the virtual filesystem, then call `run()`. The program sees `/main.c` as its source file.

```ts
engine.fs = {
  'main.c': `#include <iostream>\nint main() { std::cout << "hello\\n"; }`
};

await engine.run();
```

**stdout / stderr** use a small event-style API: subscribe with `on('data', …)` and unsubscribe with `off` using the same listener function. Each chunk is a `Uint8Array` of UTF-8 bytes.

```ts
const decoder = new TextDecoder();
const onOut = (chunk: Uint8Array) => {
  console.log(decoder.decode(chunk));
};
engine.stdout.on('data', onOut);
engine.stderr.on('data', onOut);

// When tearing down (optional if the engine is discarded):
engine.stdout.off('data', onOut);
engine.stderr.off('data', onOut);
```

**stdin** exposes `write(value: string | Uint8Array)` (UTF-8 for strings):

```ts
await engine.stdin.write('hello\n');
await engine.stdin.write(new TextEncoder().encode('hello\n'));
```

To stop a running program:

```ts
engine.stop();
```

---

## Debugger (DAP)

The debugger exposes a [Debug Adapter Protocol](https://microsoft.github.io/debug-adapter-protocol/) interface. Requests are sent synchronously and return a response. DAP messages (events, and optionally routed responses) are emitted asynchronously through the `event` listener.

```ts
const dbg = engine.debugger;

dbg.on('event', (msg) => {
  // receives both events (type: 'event') and — if you choose to route them here — responses
  console.log(msg);
});
```

### Initialization Sequence

Order matches the DAP test harness (`tools/dap/run.ts` + `tools/dap/backends/engine.ts`):

1. **Client** registers `dbg.on('event', …)` for `initialized`, `stopped`, and `terminated`.
2. **Client** starts **`engine.run()`** without awaiting it yet — the worker compiles (C) or boots the debug bridge (Python) and then **blocks** until step **9**.
3. **Client →** `initialize` request (can be sent while `run()` is in flight).
4. **Adapter →** `initialize` response (body includes **Capabilities**, e.g. `supportsConfigurationDoneRequest`).
5. **Adapter** builds the internal debugger when the worker sends its `debug` / `python_debug` message (instrumented binary or Python SAB ready).
6. **Adapter →** `initialized` event — emitted only after steps **4** and **5** (client has initialized **and** a debugger backend is attached).
7. **Client →** `setBreakpoints` (zero or more; one request per source file).
8. **Client →** `setExceptionBreakpoints` when you have filters to set (empty `filters: []` is fine).
9. **Client →** `configurationDone`
10. **Adapter →** `configurationDone` response — the debuggee then leaves its initial wait and **starts running**.

Until step **9** succeeds, `configurationDone` returns an error (`debugger not ready`). The worker is blocked on the main thread, so you must complete the handshake while `run()` is still pending — typically by running `run()` and the handshake **in parallel**.

Do **not** rely on the `initialized` event alone: also retry steps **7–9** until `configurationDone` returns `success: true` (the IDE and test harness both poll). The event is a useful early signal, but the worker may attach slightly before or after the event is delivered to your listener.

```ts
let seq = 1;
let handshakeDone = false;

const completeHandshake = () => {
  if (handshakeDone) return true;

  dbg.send({
    type: 'request',
    seq: seq++,
    command: 'setBreakpoints',
    arguments: {
      source: { path: '/main.c' },
      breakpoints: [{ line: 5 }]
    }
  });

  dbg.send({
    type: 'request',
    seq: seq++,
    command: 'setExceptionBreakpoints',
    arguments: { filters: [] }
  });

  const res = dbg.send({
    type: 'request',
    seq: seq++,
    command: 'configurationDone',
    arguments: {}
  }) as { success?: boolean };

  if (res?.success) handshakeDone = true;
  return handshakeDone;
};

dbg.on('event', (msg: { type: string; event?: string }) => {
  if (msg.type === 'event' && msg.event === 'initialized') completeHandshake();
});

const runPromise = engine.run();

dbg.send({ type: 'request', seq: seq++, command: 'initialize', arguments: {} });

while (!handshakeDone) {
  if (completeHandshake()) break;
  await new Promise((r) => setTimeout(r, 50));
}

await runPromise;
```

### Handling a pause (`stopped`)

Whenever the debuggee stops—on a **line breakpoint** or after a **step** request—the adapter emits a `stopped` event. Use `body.reason` to tell them apart:

- **`breakpoint`** — the worker paused in normal mode because execution reached a line where you set a breakpoint.
- **`step`** — the worker paused while a step mode was active (`next`, `stepIn`, or `stepOut`). The next section describes how those modes work internally.

`threadId` is always `1` (single-threaded engine).

```ts
if (msg.type === 'event' && msg.event === 'stopped') {
  const res = dbg.send({
    type: 'request',
    seq: n++,
    command: 'stackTrace',
    arguments: { threadId: 1 }
  }) as { body?: { stackFrames?: { id: number }[] } };
  const top = res.body?.stackFrames?.[0];
  if (!top) return;

  const scopesRes = dbg.send({
    type: 'request',
    seq: n++,
    command: 'scopes',
    arguments: { frameId: top.id }
  }) as { body?: { scopes?: { variablesReference: number }[] } };
  const localsRef = scopesRes.body?.scopes?.find((s) => s.name === 'Locals')?.variablesReference;
  if (localsRef == null) return;

  dbg.send({
    type: 'request',
    seq: n++,
    command: 'variables',
    arguments: { variablesReference: localsRef }
  });

  dbg.send({ type: 'request', seq: n++, command: 'continue', arguments: { threadId: 1 } });
}
```

### Stepping

Stepping does **not** use a separate single-stepping primitive in the CPU. The program is compiled with **instrumentation**: at each debuggable machine location there is a shared hook that can stop execution. The main thread and the worker coordinate through a small prefix on the **same `SharedArrayBuffer`** that also holds per-location breakpoint enable flags (see `DebugInfo` / `BP_PREFIX_BYTES` in the Rust sources).

That prefix (exposed to JS as the first elements of `get_bp_state()`, an `Int32Array` view) is laid out conceptually as:

| Index | Role                                                                                                                                                                                                |
| ----- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `0`   | Stack pointer handshake: non-zero while paused, cleared to resume                                                                                                                                   |
| `1`   | **Execution mode** — what to do at the next instrumented sites the worker reaches                                                                                                                   |
| `2`   | **`last_sp`** — stack pointer saved when the _previous_ pause ended; used to implement step-over and step-out                                                                                       |
| `3`   | **`last_stop_mode`** — mode that was active when the worker _decided_ to pause this time (written before mode is reset); the adapter uses this to set DAP `stopped.reason` (`breakpoint` vs `step`) |

**Modes** (`1`, written by the main-thread `Debugger` before waking the worker):

| Value | Name      | Meaning at instrumentation sites                                                                         |
| ----- | --------- | -------------------------------------------------------------------------------------------------------- |
| `0`   | Normal    | Stop only at locations where you have set a breakpoint (`setBreakpoints`).                               |
| `1`   | Step into | Stop at the next instrumented site that runs (enters callees if the next site is there).                 |
| `2`   | Step over | Stop only when the stack pointer is **≥ `last_sp`** (same or outer frame versus where you stepped from). |
| `3`   | Step out  | Stop only when the stack pointer is **> `last_sp`** (strictly outer frame).                              |

DAP wiring:

- **`continue`** — set mode to normal and wake the worker; variable handles from the previous pause are cleared.
- **`next`** — set mode to step-over, then wake.
- **`stepIn`** — set mode to step-into, then wake.
- **`stepOut`** — set mode to step-out, then wake.

After each successful stop, the worker resets mode to **normal** and updates **`last_sp`** to the current stack pointer so the next `next` / `stepOut` is relative to the line you actually landed on. The worker posts a minimal `breakpoint` message to the main thread; **pause classification for DAP** (`stopped.reason`) comes from reading **`last_stop_mode`** on that shared buffer, not from fields on the worker message.

**Caveats:**

- Stepping is **line-oriented** over instrumented WASM PCs, not a hardware single-step.
- Very dense control flow (e.g. multiple statements on one line) follows whatever the instrumentation map does—validate behavior with `npm run tools:dap` if you rely on edge cases.

### Supported Commands

| Command                   | Description                           |
| ------------------------- | ------------------------------------- |
| `initialize`              | Start session, returns capabilities   |
| `configurationDone`       | Signal setup complete, program starts |
| `setBreakpoints`          | Set breakpoints for a source file     |
| `setFunctionBreakpoints`  | Empty when advertised unsupported     |
| `setExceptionBreakpoints` | Accepted but no-op                    |
| `threads`                 | Returns a single `main` thread        |
| `stackTrace`              | Returns the current call stack        |
| `scopes`                  | Returns variable scopes for a frame   |
| `variables`               | Returns variables for a scope         |
| `continue`                | Resume execution                      |
| `next`                    | Step over                             |
| `stepIn`                  | Step into                             |
| `stepOut`                 | Step out                              |
| `disconnect`              | End session                           |

### Presentation filtering (Python)

When building a student-facing IDE, you usually want to hide debugger/runtime noise from `stackTrace` and `scopes` responses. The engine applies **Python-only** filters; C/C++ stacks come from DWARF and are not filtered the same way.

#### Stack frames — `debugger.filterInternals`

Set **before** `run()`:

```ts
engine.debugger.filterInternals = true; // recommended for student UIs
```

| `filterInternals` | `stackTrace` behaviour                                                                                                                                  |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `false` (default) | All frames are returned. Internal frames use `presentationHint: "subtle"`; user code uses `"normal"`. Clients can hide or dim subtle frames themselves. |
| `true`            | Frames where `user` is `false` are omitted from the response.                                                                                           |

A frame is **`user: true`** when its source path is `/main.py` (the student's virtual file). Everything else — the Bdb bridge (`/_bridge.py`), stdlib, etc. — is internal.

Internal frames always carry `presentationHint: "subtle"` even when `filterInternals` is `false`, so you can implement a “show internals” toggle without re-querying the worker:

```ts
const frames = res.body?.stackFrames ?? [];
const studentFrames = frames.filter((f) => f.presentationHint !== 'subtle');
```

#### Frame names

Python reports module-level code as `<module>`. The bridge renames frames for readability:

| Location                         | Python `co_name`   | Displayed name |
| -------------------------------- | ------------------ | -------------- |
| `/main.py` module scope          | `<module>`         | `__main__`     |
| `/main.py` function              | e.g. `main`, `add` | unchanged      |
| Other files (e.g. `/_bridge.py`) | `<module>`         | `<module>`     |

So a typical `def main():` + `if __name__ == "__main__": main()` stack shows **`main`** (the function) and **`__main__`** (the module-level caller) — not two frames both named `main`.

#### Locals — dunder names

Python `scopes` / `variables` responses **always** omit names matching `__*__` (e.g. `__builtins__`, `__file__`, `__name__`). There is no client toggle yet; those names are stripped in the adapter before DAP responses are built.

#### Locals — `list`, `dict`, and `tuple` (lazy expand)

Container locals are expandable in the variables tree (like C++ structs):

- **Scalars** — `variablesReference: 0`, value is a truncated `repr` (120 characters max).
- **`list` / `tuple`** — summary `list[N]` / `tuple[N]` with `objectRef`; children fetched on `variables` via worker **EXPAND** (one flat level per request).
- **`dict`** — summary `dict[N]` with named children loaded lazily the same way.

EXPAND responses must fit in the debug **SAB** (4084 bytes). The bridge returns an explicit error if a level would exceed that limit rather than truncating silently. Worker-side object refs are cleared when the debuggee resumes.

- **Custom instances** — summary `ClassName[N]` with named fields from `__dict__`, expanded lazily like dicts. Objects without `__dict__` (e.g. `__slots__`-only) stay as truncated `repr`.

#### C / C++

No `filterInternals` equivalent. `stackTrace` reflects the DWARF backtrace for instrumented user code. Variable formatting follows DWARF type info (see `tools/dap/tests/formatting/`).

### Program End

When the program finishes, a `terminated` event is emitted:

```ts
if (msg.type === 'event' && msg.event === 'terminated') {
  // clean up debugger UI
}
```

---

## Notes

- The engine compiles C++ to WASM in-browser using clang — the first run may take a few seconds.
- There is one thread (`id: 1`). Multi-threading is not supported.
- `send()` returns the response synchronously. DAP traffic that is pushed from the adapter arrives asynchronously via `on('event', ...)`.
- Variable handles (`variablesReference` from `scopes` / `variables`) are invalidated when you **`continue`** or issue a **step** request; always re-query after the next `stopped`.
- Scripted DAP scenarios live under `tools/dap/tests/`. From the repository root, run **`npm run tools:dap`** to execute the suite (optionally `npm run tools:dap -- <test-name>` for a single case).
