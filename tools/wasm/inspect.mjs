// Streams a .3dm through the stream3dm-wasm module in 1 MiB slices, as a Web Worker would.
//
//   node tools/wasm/inspect.mjs <stream3dm_wasm.wasm> <file.3dm> [--cadmpeg] [--json] [--warm]
//
// --cadmpeg decodes with cadmpeg; it needs the full module (built with --features full).
//
// --warm runs the file once first, so the timing excludes compiling the module's code (V8
// compiles functions on first call).
//
// Prints time, throughput and linear memory (WebAssembly memory never shrinks, so its final
// size is the peak) to stderr; with --json, the report to stdout.
import { openSync, readSync, closeSync, readFileSync, statSync } from "node:fs";

const [wasmPath, filePath, ...flags] = process.argv.slice(2);
if (!wasmPath || !filePath) {
  console.error("usage: node inspect.mjs <stream3dm_wasm.wasm> <file.3dm> [--cadmpeg] [--json]");
  process.exit(2);
}
const SLICE = 1024 * 1024;

const { instance } = await WebAssembly.instantiate(readFileSync(wasmPath), {});
const w = instance.exports;
const startMemory = w.memory.buffer.byteLength;

function run() {
  const session = w.stream3dm_new(0, flags.includes("--cadmpeg") ? 1 : 0);
  const slice = w.stream3dm_alloc(SLICE);
  const fd = openSync(filePath, "r");
  let status;
  for (;;) {
    let position = 0;
    for (;;) {
      // Linear memory may grow (and move) during a push: take a fresh view each time.
      const view = new Uint8Array(w.memory.buffer, slice, SLICE);
      const n = readSync(fd, view, 0, SLICE, position);
      if (n === 0) break;
      position += n;
      if (w.stream3dm_push(session, slice, n) !== 0) {
        status = -1;
        break;
      }
    }
    if (status === -1) break;
    status = w.stream3dm_finish(session);
    if (status !== 1) break;
  }
  closeSync(fd);
  w.stream3dm_free(slice, SLICE);
  return { session, status };
}

if (flags.includes("--warm")) w.stream3dm_drop(run().session);
const size = statSync(filePath).size;
const started = performance.now();
const { session, status } = run();
const elapsed = (performance.now() - started) / 1000;

const result = new TextDecoder().decode(
  new Uint8Array(w.memory.buffer, w.stream3dm_result_ptr(session), w.stream3dm_result_len(session)).slice()
);
const statsPtr = w.stream3dm_alloc(12);
w.stream3dm_reader_stats(session, statsPtr);
const [own, delegated, failed] = new Uint32Array(w.memory.buffer, statsPtr, 3);
if (status !== 0) {
  console.error(`${filePath}: ${result}`);
  process.exit(1);
}
const mb = size / 1e6;
const memory = w.memory.buffer.byteLength / 1e6;
console.error(
  `wasm stats: ${mb.toFixed(1)} MB in ${elapsed.toFixed(3)} s (${(mb / elapsed).toFixed(1)} MB/s), ` +
    `linear memory ${memory.toFixed(1)} MB (start ${(startMemory / 1e6).toFixed(1)} MB)`
);
console.error(`records: ${own} by our reader, ${delegated} delegated to cadmpeg, ${failed} failed in ours`);
if (flags.includes("--json")) console.log(JSON.stringify(JSON.parse(result), null, 2));
