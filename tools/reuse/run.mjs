import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { dirname, extname, join, normalize, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

// A C++ Run with nothing changed reuses the worker's last build; any change rebuilds.
// Runs a built release (npm run build) through its public Engine API in Chromium.
assert.ok(process.env.PLAYWRIGHT_MODULE, 'Set PLAYWRIGHT_MODULE to an installed Playwright module');
const { chromium } = await import(pathToFileURL(resolve(process.env.PLAYWRIGHT_MODULE)).href);
const dist = dirname(
  process.argv[2] ?? fileURLToPath(new URL('../../dist/glassjar.js', import.meta.url))
);

const html = `<script type="module">
import { Engine } from '/dist/glassjar.js';
const engine = await Engine.create('c', { wasmUrl: '/dist/engine_bg.wasm' });
const decoder = new TextDecoder();
let stdout = '';
engine.stdout.on('data', (bytes) => (stdout += decoder.decode(bytes)));
let stopped = false;
engine.debugger.on('event', (event) => {
  if (event?.event !== 'stopped') return;
  stopped = true;
  // Continue outside the event callback, as a client would.
  setTimeout(() => engine.debugger.send({ seq: 9, type: 'request', command: 'continue', arguments: { threadId: event.body.threadId } }), 0);
});
window.run = async ({ files, binaryFiles = {}, debug = false, breakpointLine }) => {
  stdout = ''; stopped = false;
  engine.fs = files;
  engine.binaryFiles = Object.fromEntries(Object.entries(binaryFiles).map(([path, text]) => [path, new TextEncoder().encode(text)]));
  engine.debugger.enabled = debug;
  const running = engine.run();
  if (debug) {
    await new Promise((r) => setTimeout(r, 0));
    engine.debugger.send({ seq: 1, type: 'request', command: 'initialize', arguments: { adapterID: 'glassjar', linesStartAt1: true, columnsStartAt1: true } });
    let settled = false; running.finally(() => (settled = true));
    while (!settled) {
      engine.debugger.send({ seq: 2, type: 'request', command: 'setBreakpoints', arguments: { source: { path: '/main.cpp' }, breakpoints: [{ line: breakpointLine }] } });
      if (engine.debugger.send({ seq: 3, type: 'request', command: 'configurationDone', arguments: {} })?.success) break;
      await new Promise((r) => setTimeout(r, 2));
    }
  }
  const result = await running;
  return { stdout, stopped, exitCode: result.exitCode, type: result.type, buildMs: result.timing?.buildMs };
};
window.ready = true;
</script>`;
const types = { '.js': 'text/javascript', '.wasm': 'application/wasm' };
const server = createServer(async (request, response) => {
  response.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
  response.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
  const path = new URL(request.url, 'http://x').pathname;
  if (path === '/') {
    response.setHeader('Content-Type', 'text/html');
    return response.end(html);
  }
  if (!path.startsWith('/dist/')) return response.writeHead(404).end();
  try {
    const file = join(dist, normalize(path.slice('/dist/'.length)).replace(/^(\.\.[/\\])+/, ''));
    response.setHeader('Content-Type', types[extname(file)] ?? 'application/octet-stream');
    response.end(await readFile(file));
  } catch {
    response.writeHead(404).end();
  }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const browser = await chromium.launch(
  process.env.CHROME_PATH ? { executablePath: process.env.CHROME_PATH } : {}
);
const page = await browser.newPage();
const pageErrors = [];
page.on('pageerror', (error) => pageErrors.push(String(error)));
if (process.env.VERBOSE) page.on('console', (m) => console.log('page:', m.text().slice(0, 200)));
try {
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  await page.waitForFunction(() => window.ready, null, { timeout: 120_000 });
  const run = (options) => {
    if (process.env.VERBOSE) console.log('run', JSON.stringify(options).slice(0, 80));
    return page.evaluate((options) => window.run(options), options);
  };

  const program = (word) => ({
    'main.cpp': `#include <cstdio>\n#include <iostream>\n#include "word.h"\nint main() {\n  std::FILE *f = std::fopen("/data.txt", "r");\n  char data[16] = {};\n  if (f) std::fgets(data, sizeof data, f);\n  std::cout << WORD << "${word}" << data << std::endl;\n  return 0;\n}\n`,
    'word.h': '#define WORD "a"\n'
  });
  const data = { '/data.txt': 'x' };

  const first = await run({ files: program('1'), binaryFiles: data });
  assert.equal(first.stdout, 'a1x\n');
  const again = await run({ files: program('1'), binaryFiles: data });
  assert.equal(again.stdout, 'a1x\n', 'an unchanged run prints the same output');
  assert.ok(
    again.buildMs < first.buildMs / 4,
    `an unchanged run reuses the build (${again.buildMs}ms vs ${first.buildMs}ms)`
  );

  const edited = await run({ files: program('2'), binaryFiles: data });
  assert.equal(edited.stdout, 'a2x\n', 'a source edit rebuilds');
  const header = await run({
    files: { ...program('2'), 'word.h': '#define WORD "b"\n' },
    binaryFiles: data
  });
  assert.equal(header.stdout, 'b2x\n', 'an included header edit rebuilds');
  const binary = await run({
    files: { ...program('2'), 'word.h': '#define WORD "b"\n' },
    binaryFiles: { '/data.txt': 'y' }
  });
  assert.equal(binary.stdout, 'b2y\n', 'a binary input edit is seen');

  const plain = await run({ files: program('3'), binaryFiles: data });
  assert.equal(plain.stdout, 'a3x\n');
  const debug = await run({
    files: program('3'),
    binaryFiles: data,
    debug: true,
    breakpointLine: 8
  });
  assert.equal(debug.stdout, 'a3x\n');
  assert.ok(
    debug.stopped,
    'Debug after a plain run rebuilds with debug info and stops at the breakpoint'
  );

  const broken = await run({
    files: { ...program('4'), 'main.cpp': 'int main() { return x; }\n' }
  });
  assert.notEqual(broken.exitCode, 0, 'a compile error fails');
  const brokenAgain = await run({
    files: { ...program('4'), 'main.cpp': 'int main() { return x; }\n' }
  });
  assert.notEqual(brokenAgain.exitCode, 0, 'a failed build is never reused');

  assert.deepEqual(pageErrors, []);
  console.log(
    `ok: unchanged C++ run reused its build (${Math.round(again.buildMs)}ms vs ${Math.round(first.buildMs)}ms); edits, binary inputs, Debug and errors rebuild`
  );
} finally {
  await browser.close();
  server.close();
}
