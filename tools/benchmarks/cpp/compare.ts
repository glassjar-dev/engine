// Compares the engine's isolated program-execution time against the native aarch64
// baseline. Native side reuses utils/run-benchmark.sh (the source of truth); engine
// side runs the same program N times and collects `timing.runMs`.
//
// Usage:  npx -y bun compare.ts <program.(cpp|c)>
// Env:    BENCH_ITERS=5  BENCH_OPT=O0   (engine compiles user code at -O0; match it)
import { $ } from 'bun';
import { Engine } from 'glassjar';
import { readFileSync } from 'node:fs';
import { basename, resolve } from 'node:path';

const arg = process.argv[2];
if (!arg) {
  console.error('usage: bun compare.ts <program.(cpp|c)>');
  process.exit(1);
}
const src = resolve(arg);
const iters = Number(process.env.BENCH_ITERS ?? 5);
const opt = process.env.BENCH_OPT ?? 'O0';
const source = readFileSync(src, 'utf8');

const stats = (xs: number[]) => ({
  min: Math.min(...xs),
  avg: xs.reduce((a, b) => a + b, 0) / xs.length
});

// --- native baseline: delegate to run-benchmark.sh, parse its report ---
const out =
  await $`BENCH_OPT=${opt} BENCH_ITERS=${iters} ${import.meta.dir}/utils/run-benchmark.sh ${src}`.text();
const m = out.match(/native\s+min\s+([\d.]+)\s+ms\s+avg\s+([\d.]+)\s+ms/);
if (!m) {
  console.error(`could not parse run-benchmark output:\n${out}`);
  process.exit(1);
}
const native = { min: Number(m[1]), avg: Number(m[2]) };

// --- engine: run N clean executions, collect the isolated runMs ---
const engine = await Engine.create('c');
engine.debugger.enabled = true; // debug instrumentation would inflate runMs
engine.fs = { [basename(src)]: source };

const runMs: number[] = [];
for (let i = 0; i < iters; i++) {
  const r = await engine.run();
  if (r.type !== 'completed' || r.exitCode !== 0) {
    console.error('engine run failed:', JSON.stringify(r));
    process.exit(1);
  }
  runMs.push(r.timing.runMs);
}
const engineStats = stats(runMs);

// --- report ---
const f = (n: number) => n.toFixed(3).padStart(9);
console.log(`\n== ${basename(src)}  (-${opt}, ${iters} iters) ==`);
console.log(`  native  min ${f(native.min)} ms   avg ${f(native.avg)} ms`);
console.log(
  `  engine  min ${f(engineStats.min)} ms   avg ${f(engineStats.avg)} ms   (runMs, isolated execution)`
);
console.log(
  `  ratio engine/native:  min ${(engineStats.min / native.min).toFixed(2)}x   avg ${(engineStats.avg / native.avg).toFixed(2)}x\n`
);
