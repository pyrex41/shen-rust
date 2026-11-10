// Time a WASI command module under Node and write its stdout to argv[3].
// Usage: node wasm-profile-boot.mjs <fib.wasm> <stdout-file>
import { WASI } from 'node:wasi';
import { readFile, open } from 'node:fs/promises';
import { performance } from 'node:perf_hooks';

const wasmPath = process.argv[2];
const outPath = process.argv[3];
if (!wasmPath || !outPath) {
  console.error('usage: node wasm-profile-boot.mjs <wasm> <stdout-file>');
  process.exit(2);
}

const out = await open(outPath, 'w');
const err = await open(outPath + '.err', 'w');
const wasi = new WASI({
  version: 'preview1',
  args: ['fib.wasm'],
  env: {},
  stdout: out.fd,
  stderr: err.fd,
  returnOnExit: true,
});

const bytes = await readFile(wasmPath);
const t0 = performance.now();
const module = await WebAssembly.compile(bytes);
const tCompile = performance.now();
const imports = wasi.getImportObject();
const instance = await WebAssembly.instantiate(module, imports);
const tInst = performance.now();
const exitCode = wasi.start(instance);
const tRun = performance.now();
await out.close();
await err.close();

const stdout = await readFile(outPath, 'utf8');
process.stdout.write(JSON.stringify({
  wasm: wasmPath,
  bytes: bytes.length,
  compile_ms: +(tCompile - t0).toFixed(2),
  instantiate_ms: +(tInst - tCompile).toFixed(2),
  run_ms: +(tRun - tInst).toFixed(2),
  total_ms: +(tRun - t0).toFixed(2),
  exit: exitCode,
  stdout,
}) + '\n');
