import { $ } from 'bun';
import chalk from 'chalk';
import { existsSync } from 'node:fs';
import { mkdir, readdir, readFile, rm, stat, writeFile } from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';
import stripJsonComments from 'strip-json-comments';

import { createDebugpyBackend } from './backends/debugpy.ts';
import { createEngineBackend } from './backends/engine.ts';
import { createLldbBackend } from './backends/lldb.ts';
import { CaptureMap, executeSnippet, match, MatchResult, substitutePlaceholders } from './matcher';

export type Json = null | boolean | number | string | Json[] | { [k: string]: Json };

export type RequestStep = {
  type: 'request';
  command: string;
  arguments?: Json;
};
export type ResponseStep = {
  type: 'response';
  success?: boolean;
  command?: string;
  body?: Json;
};
export type EventStep = {
  type: 'event';
  event: string;
  body?: Json;
  $timeout?: number;
};
export type ExpectStep = {
  type: 'expect';
  run: string;
  expect?: Json;
};
export type Step = RequestStep | ResponseStep | EventStep | ExpectStep;

type TestFile = { steps: Step[]; lang?: Lang };

export type Lang = 'c' | 'python' | 'rust';

export type BackendOptions = {
  testDir: string;
  testOutputDir: string;
  fsNode: Record<string, Json>;
  lang: Lang;
};

export interface Backend {
  send(req: Json): Promise<Json>;
  onEvent(cb: (e: Json) => void): void;
  shutdown(): Promise<void>;
}

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '../..');
const TESTS_DIR = path.join(ROOT, 'tools/dap/tests');
const OUTPUT_DIR = path.join(ROOT, 'tools/dap/output');
const DAP_TIMEOUT_MS = 1000;
// Cold compiler download/JIT and C++ header compilation are part of debugger
// initialization. Both the original and current bundles can exceed 30 seconds
// in a full-suite process; keep protocol-response deadlines separate.
const INITIALIZATION_TIMEOUT_MS = 90000;
const DEV_BUILD_MARKER = path.join(ROOT, 'node_modules/build.lock');
const COMMON_INIT_STEPS: Step[] = [
  {
    type: 'request',
    command: 'initialize',
    arguments: {
      clientID: 'dap-harness',
      clientName: 'dap-harness',
      adapterID: 'lldb-dap',
      pathFormat: 'path',
      linesStartAt1: true,
      columnsStartAt1: true
    }
  },
  {
    type: 'response',
    success: true,
    command: 'initialize'
  },
  {
    type: 'request',
    command: 'launch',
    arguments: {
      stopOnEntry: false
    }
  },
  { type: 'event', event: 'initialized', $timeout: INITIALIZATION_TIMEOUT_MS }
];

type CliOpts = {
  tests: string[];
  lldb: boolean;
  debugpy: boolean;
};

function logInfo(msg: string) {
  console.log(`${chalk.cyan('info')} ${msg}`);
}

function logStep(msg: string) {
  console.log(` ${chalk.blue('└─')} ${chalk.dim(msg)}`);
}

function logOk(msg: string) {
  console.log(`${chalk.green('ok')} ${msg}`);
}

function die(msg: string): never {
  console.error(`${chalk.red('error')} ${msg}`);
  process.exit(1);
}

function parseCli(argv: string[]): CliOpts {
  let lldb = false;
  let debugpy = false;
  const tests: string[] = [];
  for (const arg of argv) {
    if (arg === '--lldb') {
      lldb = true;
    } else if (arg === '--debugpy') {
      debugpy = true;
    } else if (arg.startsWith('--')) {
      die(`unknown flag: ${arg}`);
    } else {
      tests.push(arg);
    }
  }
  return { tests, lldb, debugpy };
}

async function ensureEngineLinked() {
  logInfo('installing library...');
  await $`npm link`.cwd(ROOT).quiet();
  await $`npm link glassjar`.cwd(HERE).quiet();
}

/** Relative path from `tools/dap/tests` using `/` (e.g. `formatting/string`). */
function testId(...segments: string[]): string {
  return path
    .join(...segments)
    .split(path.sep)
    .join('/');
}

async function discoverTests(dir: string, prefix = ''): Promise<string[]> {
  const tests: string[] = [];
  const entries = (await readdir(dir, { withFileTypes: true })).sort((a, b) =>
    a.name.localeCompare(b.name)
  );
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const rel = prefix ? testId(prefix, entry.name) : entry.name;
    const abs = path.join(dir, entry.name);
    if (existsSync(path.join(abs, 'dap.jsonc'))) tests.push(rel);
    tests.push(...(await discoverTests(abs, rel)));
  }
  return tests;
}

async function listTestNames(): Promise<string[]> {
  return discoverTests(TESTS_DIR);
}

async function readTestLang(testName: string): Promise<Lang> {
  const dapPath = path.join(testDirFor(testName), 'dap.jsonc');
  if (!existsSync(dapPath)) return 'c';
  const file = await readJsonFile<TestFile>(dapPath);
  return file.lang ?? 'c';
}

function expandTestSelection(requested: string[], available: string[]): string[] {
  const selected = new Set<string>();
  for (const name of requested) {
    const matches = available.filter((t) => t === name || t.startsWith(`${name}/`));
    if (matches.length === 0) {
      die(`unknown test '${name}'. Available: ${available.join(', ')}`);
    }
    for (const m of matches) selected.add(m);
  }
  return [...selected].sort();
}

function testDirFor(testName: string): string {
  return path.join(TESTS_DIR, ...testName.split('/'));
}

async function waitForDevBuild() {
  if (!existsSync(DEV_BUILD_MARKER)) return;
  logInfo('waiting for build to finish...');
  while (existsSync(DEV_BUILD_MARKER)) {
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

async function readJsonFile<T>(filePath: string): Promise<T> {
  const raw = await readFile(filePath, 'utf8');
  try {
    return JSON.parse(stripJsonComments(raw)) as T;
  } catch (err) {
    throw new Error(`invalid JSON in ${filePath}: ${String(err)}`);
  }
}

async function collectFsNode(dirPath: string): Promise<Record<string, Json>> {
  async function walk(current: string): Promise<Record<string, Json>> {
    const out: Record<string, Json> = {};
    const entries = (await readdir(current, { withFileTypes: true })).sort((a, b) =>
      a.name.localeCompare(b.name)
    );
    for (const entry of entries) {
      const abs = path.join(current, entry.name);
      if (entry.isDirectory()) {
        out[entry.name] = await walk(abs);
      } else if (entry.isFile()) {
        const content = await readFile(abs, 'utf8');
        out[entry.name] = content;
      }
    }
    return out;
  }
  return walk(dirPath);
}

function fmtJson(v: unknown): string {
  try {
    return JSON.stringify(v, null, 2);
  } catch {
    return String(v);
  }
}

function formatMismatch(
  expected: Json,
  received: Json,
  result: Extract<MatchResult, { success: false }>
): string {
  const bar = ` |`;
  const gbar = chalk.green(bar);
  const rbar = chalk.red(bar);

  const expectedLines = fmtJson(expected).split('\n');
  const receivedLines = fmtJson(received).split('\n');
  return [
    '',
    `${gbar} ${chalk.bold(chalk.green('Expected:'))}`,
    gbar,
    ...expectedLines.map((line) => `${gbar} ${line}`),
    gbar,
    `${rbar} ${chalk.bold(chalk.red('Received:'))}`,
    rbar,
    ...receivedLines.map((line) => `${rbar} ${line}`),
    rbar,
    chalk.red(`${bar} at ${chalk.underline(result.at)}: ${result.reason}`),
    ''
  ].join('\n');
}

function asError(err: unknown): Error {
  return err instanceof Error ? err : new Error(String(err));
}

async function waitForEvent(
  queue: Json[],
  waitForNext: () => Promise<Json>,
  eventName: string,
  timeoutMs: number
): Promise<Json> {
  const endAt = Date.now() + timeoutMs;
  while (Date.now() < endAt) {
    for (let i = 0; i < queue.length; i++) {
      const event = queue[i];
      if (
        eventName === 'initialized' &&
        event &&
        typeof event === 'object' &&
        !Array.isArray(event) &&
        event.type === 'event' &&
        event.event === 'runResult'
      ) {
        throw new Error(`engine finished before debugger initialization: ${fmtJson(event.body)}`);
      }
      if (
        event &&
        typeof event === 'object' &&
        !Array.isArray(event) &&
        event.type === 'event' &&
        event.event === eventName
      ) {
        queue.splice(i, 1);
        return event;
      }
    }
    const remaining = endAt - Date.now();
    if (remaining <= 0) break;
    await Promise.race([
      waitForNext(),
      new Promise((_, reject) => setTimeout(() => reject(new Error('timeout')), remaining))
    ]).catch((err) => {
      if (String(err).includes('timeout')) return;
      throw err;
    });
  }
  throw new Error(`timed out waiting for event '${eventName}' after ${timeoutMs}ms`);
}

async function runTest(testName: string, opts: CliOpts): Promise<void> {
  const testDir = testDirFor(testName);
  const testStat = await stat(testDir).catch(() => null);
  if (!testStat || !testStat.isDirectory()) throw new Error(`unknown test '${testName}'`);

  const dapPath = path.join(testDir, 'dap.jsonc');
  if (!existsSync(dapPath)) throw new Error(`missing ${dapPath}`);
  const file = await readJsonFile<TestFile>(dapPath);
  if (!Array.isArray(file.steps)) throw new Error(`${dapPath}: expected top-level steps[]`);
  const testOutputDir = path.join(OUTPUT_DIR, ...testName.split('/'));
  await rm(testOutputDir, { recursive: true, force: true });
  await mkdir(testOutputDir, { recursive: true });

  const lang: Lang = file.lang ?? 'c';
  const fsNode = await collectFsNode(testDir);
  const backendOpts: BackendOptions = { testDir, testOutputDir, fsNode, lang };
  const backend =
    lang === 'python'
      ? opts.debugpy
        ? await createDebugpyBackend(backendOpts)
        : await createEngineBackend(backendOpts)
      : opts.lldb
        ? await createLldbBackend(backendOpts)
        : await createEngineBackend(backendOpts);

  const eventQueue: Json[] = [];
  const rawDapLog: Json[] = [];
  let resolveEventWaiter: ((v: Json) => void) | null = null;

  backend.onEvent((event) => {
    rawDapLog.push(event);
    eventQueue.push(event);
    if (resolveEventWaiter) {
      const fn = resolveEventWaiter;
      resolveEventWaiter = null;
      fn(event);
    }
  });
  const waitForNextEvent = () =>
    new Promise<Json>((resolve) => {
      resolveEventWaiter = resolve;
    });

  const captures: CaptureMap = {};
  let seq = 1;
  let lastResponse: Json | null = null;

  const executeStep = async (step: Step, label: string, visible: boolean) => {
    if (step.type === 'request') {
      if (visible) logStep(`${label} ${step.command}`);
      const reqObj = substitutePlaceholders(
        {
          type: 'request',
          seq: seq++,
          command: step.command,
          arguments: step.arguments ?? {}
        },
        captures
      ) as Json;
      rawDapLog.push(reqObj);

      lastResponse = await backend.send(reqObj);
      rawDapLog.push(lastResponse);
      return;
    }

    if (step.type === 'response') {
      if (!lastResponse) throw new Error(`${label}: no prior request response available`);
      if (visible) logStep(label);
      const expected = substitutePlaceholders(step as unknown as Json, captures);
      const result = match(expected, lastResponse, 'response');
      if (!result.success) throw new Error(formatMismatch(expected, lastResponse, result));
      Object.assign(captures, result.captures);
      return;
    }

    if (step.type === 'event') {
      if (visible) logStep(`${label} ${step.event}`);
      const timeout = step.$timeout ?? DAP_TIMEOUT_MS;
      const actualEvent = await waitForEvent(eventQueue, waitForNextEvent, step.event, timeout);
      const { $timeout: _ignored, ...expectedStep } = step;
      const expected = substitutePlaceholders(expectedStep as unknown as Json, captures);
      const result = match(expected, actualEvent, 'event');
      if (!result.success) throw new Error(formatMismatch(expected, actualEvent, result));
      Object.assign(captures, result.captures);
      return;
    }

    if (step.type === 'expect') {
      if (visible) logStep(`${label}`);
      const resolvedRun = substitutePlaceholders(step.run as Json, captures);
      if (typeof resolvedRun !== 'string')
        throw new Error(`${label}: expect run must be a string after placeholder substitution`);
      const actual = executeSnippet(resolvedRun, captures);
      if (step.expect !== undefined) {
        const expected = substitutePlaceholders(step.expect, captures);
        const result = match(expected, actual, 'expect');
        if (!result.success) throw new Error(formatMismatch(expected, actual, result));
        Object.assign(captures, result.captures);
      }
    }
  };

  logInfo(`${chalk.bold(testName)} (${file.steps.length} steps)`);
  let failure: Error | null = null;
  try {
    logStep(`[0/${file.steps.length}] setup debugger session`);
    for (const step of COMMON_INIT_STEPS) {
      await executeStep(step, '[init]', false);
    }

    for (let i = 0; i < file.steps.length; i++) {
      const step = file.steps[i];
      const label = `[${i + 1}/${file.steps.length}] ${step.type}`;
      await executeStep(step, label, true);
    }
  } catch (err) {
    failure = asError(err);
  }

  await backend.shutdown();
  await writeFile(path.join(testOutputDir, 'log.json'), JSON.stringify(rawDapLog, null, 2));
  if (failure) throw failure;
  logOk(`${testName} passed`);
}

async function main() {
  const opts = parseCli(process.argv.slice(2));

  const available = await listTestNames();
  if (available.length === 0) die(`no tests found in ${TESTS_DIR}`);
  const tests = opts.tests.length ? expandTestSelection(opts.tests, available) : available;

  const langs = new Map<string, Lang>();
  for (const t of tests) langs.set(t, await readTestLang(t));
  const pythonTests = tests.filter((t) => langs.get(t) === 'python');
  const hasEngine = tests.some((t) => (langs.get(t) === 'python' ? !opts.debugpy : !opts.lldb));

  if (hasEngine) {
    await waitForDevBuild();
    if (!existsSync(path.join(ROOT, 'dist/glassjar.js')))
      die(`missing dist/glassjar.js. Run 'npm run build' first.`);
    await ensureEngineLinked();
  }
  if (opts.lldb)
    logInfo(`${chalk.bold('--lldb')}: running C/C++ against ${chalk.bold('lldb-dap')}`);
  if (opts.debugpy && pythonTests.length)
    logInfo(`${chalk.bold('--debugpy')}: running Python against ${chalk.bold('debugpy')}`);

  const failed: { name: string; error: string }[] = [];

  for (const test of tests) {
    try {
      await runTest(test, opts);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      failed.push({ name: test, error: message });
      console.error(` ${chalk.red('└─')} ${chalk.red('fail')} \n${chalk.dim(message)}`);
    }
  }

  if (failed.length > 0) {
    console.error(`\n${chalk.red(`${failed.length}/${tests.length} test(s) failed`)}`);
    for (const f of failed) {
      console.error(`${chalk.red('-')} ${f.name}`);
    }
    process.exit(1);
  }

  console.log(`\n${chalk.green(chalk.bold(`all ${tests.length} test(s) passed`))}`);
}

await main();
