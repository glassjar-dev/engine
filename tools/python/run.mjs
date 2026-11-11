import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { dirname, extname, join, normalize, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

// Python runtime checks against a built release (npm run build), in Chromium:
// zero-timeout polls and sleep(0), /dev/null, and installing pytest from PyPI (needs network).
assert.ok(process.env.PLAYWRIGHT_MODULE, 'Set PLAYWRIGHT_MODULE to an installed Playwright module');
const { chromium } = await import(pathToFileURL(resolve(process.env.PLAYWRIGHT_MODULE)).href);
const dist = dirname(process.argv[2] ?? fileURLToPath(new URL('../../dist/glassjar.js', import.meta.url)));

const html = `<script type="module">
import { Engine } from '/dist/glassjar.js';
const notes = [];
const engine = await Engine.create('python', { wasmUrl: '/dist/engine_bg.wasm', python: { packages: true, onPackageStatus: (s) => notes.push(s.message) } });
engine.debugger.enabled = false;
const decoder = new TextDecoder();
let stdout = '', stderr = '';
engine.stdout.on('data', (b) => (stdout += decoder.decode(b)));
engine.stderr.on('data', (b) => (stderr += decoder.decode(b)));
window.run = async (files) => {
  stdout = ''; stderr = ''; notes.length = 0;
  engine.fs = files;
  const result = await engine.run();
  return { stdout: stdout.replace(/\\x1b\\[[0-9;]*m/g, ''), stderr: stderr.replace(/\\x1b\\[[0-9;]*m/g, ''), notes: [...notes], exitCode: result.exitCode, type: result.type, error: result.error?.message };
};
window.ready = true;
</script>`;
const types = { '.js': 'text/javascript', '.wasm': 'application/wasm' };
const server = createServer(async (request, response) => {
  response.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
  response.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
  const path = new URL(request.url, 'http://x').pathname;
  if (path === '/') return response.setHeader('Content-Type', 'text/html'), response.end(html);
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
const browser = await chromium.launch(process.env.CHROME_PATH ? { executablePath: process.env.CHROME_PATH } : {});
try {
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  await page.waitForFunction(() => window.ready, null, { timeout: 120_000 });
  const run = (files) => page.evaluate((files) => window.run(files), files);

  const polls = await run({
    'main.py': [
      'import time, select, selectors, subprocess',
      'time.sleep(0)',
      'print(select.select([], [], [], 0), select.poll().poll(0))',
      'print(open("/dev/null", "w").write("gone"), repr(open("/dev/null").read()))'
    ].join('\n')
  });
  assert.equal(polls.exitCode, 0, polls.error ?? polls.stderr);
  assert.equal(polls.stdout, "([], [], []) []\n4 ''\n");

  const program = [
    'import pytest',
    'def test_add():',
    '    assert 2 + 3 == 5',
    'def test_broken():',
    '    assert 2 + 2 == 5',
    'if __name__ == "__main__":',
    '    raise SystemExit(pytest.main(["-q", __file__]))'
  ].join('\n');
  const tested = await run({ 'main.py': program });
  assert.match(tested.stdout, /1 failed, 1 passed/, tested.stderr);
  assert.ok(tested.notes.some((n) => /Installing pytest/.test(n)), tested.notes.join('; '));
  const cached = await run({ 'main.py': program });
  assert.deepEqual(cached.notes, [], 'a second run reuses the installed packages');

  const compiled = await run({ 'main.py': 'import numpy\n' });
  assert.ok(compiled.notes.some((n) => /numpy has compiled code/.test(n)), compiled.notes.join('; '));
  assert.match(compiled.stderr, /ModuleNotFoundError/);
  console.log('ok: zero-timeout polls, sleep(0), /dev/null; pytest installed from PyPI and ran; compiled packages reported');
} finally {
  await browser.close();
  server.close();
}
