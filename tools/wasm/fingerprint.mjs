// Checks that the stream3dm-wasm module and the native `reader fingerprint` produce the same bytes
// (docs/fingerprint.md: the fingerprint must not depend on the platform).
//
//   node tools/wasm/fingerprint.mjs <stream3dm_wasm.wasm> <reader binary> <file.3dm>...
//
// Both use the public key and hashed names. Exits 1 when any file differs.
import { execFileSync } from "node:child_process";
import { closeSync, openSync, readFileSync, readSync } from "node:fs";

const [wasmPath, stream3dmPath, ...files] = process.argv.slice(2);
if (!wasmPath || !stream3dmPath || files.length === 0) {
  console.error("usage: node fingerprint.mjs <stream3dm_wasm.wasm> <reader> <file.3dm>...");
  process.exit(2);
}
const SLICE = 1024 * 1024;
const { instance } = await WebAssembly.instantiate(readFileSync(wasmPath), {});
const w = instance.exports;

function wasmFingerprint(path) {
  const session = w.stream3dm_new_fingerprint(0);
  w.stream3dm_fingerprint_options(session, 0, 0);
  const slice = w.stream3dm_alloc(SLICE);
  const fd = openSync(path, "r");
  try {
    for (;;) {
      let position = 0;
      for (;;) {
        const view = new Uint8Array(w.memory.buffer, slice, SLICE);
        const n = readSync(fd, view, 0, SLICE, position);
        if (n === 0) break;
        position += n;
        if (w.stream3dm_push(session, slice, n) !== 0) throw new Error("push failed");
      }
      const status = w.stream3dm_finish(session);
      if (status === 1) continue;
      if (status !== 0) throw new Error("finish failed");
      const bytes = new Uint8Array(w.memory.buffer, w.stream3dm_fingerprint_ptr(session), w.stream3dm_fingerprint_len(session));
      return new TextDecoder().decode(bytes.slice());
    }
  } finally {
    closeSync(fd);
    w.stream3dm_drop(session);
  }
}

let differ = 0;
for (const file of files) {
  const started = performance.now();
  const wasm = wasmFingerprint(file);
  const seconds = (performance.now() - started) / 1000;
  const native = execFileSync(stream3dmPath, ["fingerprint", file], { stdio: ["ignore", "pipe", "ignore"], maxBuffer: 1 << 28 })
    .toString()
    .trimEnd();
  const same = wasm === native;
  if (!same) differ++;
  const objects = JSON.parse(wasm).objects.length;
  console.log(`${same ? "same" : "DIFFERENT"}  ${file}  (${objects} objects, ${wasm.length} bytes, wasm ${seconds.toFixed(2)} s)`);
}
process.exit(differ ? 1 : 0);
