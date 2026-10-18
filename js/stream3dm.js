// stream3dm in JavaScript: a small wrapper around the raw WebAssembly exports of
// crates/stream3dm-wasm (its module documentation describes the protocol underneath). It runs
// in browsers, Web Workers and Node 20 or later, and has no dependencies. Types:
// stream3dm.d.ts.
//
//   import { init } from './stream3dm.js';
//   const reader = await init();                       // stream3dm_wasm.wasm next to this file
//   const { report, meshes } = await reader.read(file, { meshes: true });

/** Bytes pushed to the reader at a time. */
const SLICE = 1 << 20;
const decoder = new TextDecoder();

/** An error from the reader: not a .3dm file, truncated or invalid. */
export class Stream3dmError extends Error {
  name = 'Stream3dmError';
}

/**
 * Loads the module. `wasm` is the module's URL (default: `stream3dm_wasm.wasm` next to this
 * file; a relative string resolves against this file's URL, not the page's), a `Response` or
 * its promise, its bytes, or a compiled `WebAssembly.Module`. `full` gives the full build (with
 * the cadmpeg decoder, for the rare record versions the small module leaves to it), loaded only
 * when a file needs it; without it such records are reported as undecoded objects.
 */
export async function init(wasm = new URL('./stream3dm_wasm.wasm', import.meta.url), { full = null } = {}) {
  return new Stream3dm(await load(wasm), full);
}

/** The compiled module and an instance of it. */
async function load(source) {
  const module = await compile(source);
  return { module, exports: (await WebAssembly.instantiate(module, {})).exports };
}

async function compile(source) {
  if (source instanceof WebAssembly.Module) return source;
  if (typeof source === 'string') source = new URL(source, import.meta.url);
  if (source instanceof URL) {
    if (source.protocol === 'file:') {
      // Node's fetch does not read files. Bundlers leave this import alone.
      const { readFile } = await import(/* webpackIgnore: true */ /* @vite-ignore */ 'node:fs/promises');
      source = await readFile(source);
    } else {
      source = fetch(source);
    }
  }
  if (source instanceof Promise) source = await source;
  if (typeof Response !== 'undefined' && source instanceof Response) {
    if (!source.ok) throw new Stream3dmError(`cannot load the module: HTTP ${source.status}`);
    // Streaming compilation needs the right media type; otherwise compile the bytes.
    if (source.headers.get('content-type')?.startsWith('application/wasm')) {
      return WebAssembly.compileStreaming(source);
    }
    source = await source.arrayBuffer();
  }
  return WebAssembly.compile(source);
}

/** The reader, bound to one instance of the module (and of the full build once loaded). */
export class Stream3dm {
  /** `{ module, exports }` in use. */
  #loaded;
  #fullSource;
  /** The full build's `{ module, exports }`, loading or loaded (one for all reads). */
  #full = null;

  constructor(loaded, full = null) {
    this.#loaded = loaded;
    this.#fullSource = full;
  }

  /**
   * Reads a `.3dm` file. `source`: its bytes (`Uint8Array`, any typed array or `DataView`,
   * `ArrayBuffer`), a `Blob` or `File`, or a function returning an iterable or async iterable of
   * `Uint8Array` slices (called once per pass: files with blocks are read twice). Options:
   *
   * - `meshes`: collect the visible meshes, curves and points (and the fingerprint);
   * - `edges`: with meshes, the feature edges as line segments (default true);
   * - `fingerprint`: compute the fingerprint (docs/fingerprint.md); `true`, or `{ key, names }`
   *   with a 32-byte owner key (default: the public key) and names kept in clear;
   * - `onProgress({ pass, bytes, total })`: after each slice (`total` when known).
   */
  async read(source, options = {}) {
    const { meshes = false, edges = true, fingerprint = false, onProgress = null } = options;
    const loaded = this.#loaded;
    const w = loaded.exports;
    const session = (meshes ? w.stream3dm_new_with_meshes(0) : fingerprint ? w.stream3dm_new_fingerprint(0) : w.stream3dm_new(0, 0)) >>> 0;
    const slice = w.stream3dm_alloc(SLICE) >>> 0;
    // Ten u32 or three f64 at a time, aligned to 8 bytes.
    const raw = w.stream3dm_alloc(48) >>> 0;
    const scratch = raw + (-raw & 7);
    // The full build is worth loading only when it was given.
    const needsFull = () => this.#fullSource !== null && w.stream3dm_needs_full(session) === 1;
    let trapped = false;
    try {
      if (meshes && !edges) w.stream3dm_edges(session, 0);
      if (fingerprint && typeof fingerprint === 'object') fingerprintOptions(w, session, fingerprint);
      const chunks = [];
      const total = sizeOf(source);
      for (let pass = 1; ; pass++) {
        let bytes = 0;
        for await (const part of slices(source)) {
          new Uint8Array(w.memory.buffer, slice, part.length).set(part);
          const pushed = w.stream3dm_push(session, slice, part.length);
          if (needsFull()) return this.#again(source, options);
          if (pushed !== 0) throw new Stream3dmError(text(w, session));
          if (meshes) drain(w, session, scratch, chunks);
          bytes += part.length;
          onProgress?.({ pass, bytes, total });
        }
        const status = w.stream3dm_finish(session);
        if (needsFull()) return this.#again(source, options);
        if (status === 1) continue;
        if (status !== 0) throw new Stream3dmError(text(w, session));
        if (meshes) drain(w, session, scratch, chunks);
        break;
      }
      const result = {
        report: JSON.parse(text(w, session)),
        context: JSON.parse(read(w, w.stream3dm_context_ptr(session), w.stream3dm_context_len(session))),
        stats: stats(w, session, scratch),
      };
      if (meshes) {
        w.stream3dm_offset(session, scratch);
        result.meshes = {
          origin: Array.from(new Float64Array(w.memory.buffer, scratch, 3)),
          chunks,
          picks: JSON.parse(read(w, w.stream3dm_picks_ptr(session), w.stream3dm_picks_len(session)) || '[]'),
        };
      }
      if (meshes || fingerprint) {
        result.fingerprint = JSON.parse(read(w, w.stream3dm_fingerprint_ptr(session), w.stream3dm_fingerprint_len(session)));
        result.objectKeys = JSON.parse(read(w, w.stream3dm_object_keys_ptr(session), w.stream3dm_object_keys_len(session)) || '[]');
      }
      return result;
    } catch (error) {
      if (!(error instanceof WebAssembly.RuntimeError)) throw error;
      // A trap can leave the instance's memory inconsistent: later reads get a fresh one.
      trapped = true;
      await this.#renew(loaded);
      throw new Stream3dmError(`the reader stopped on this file: ${error.message}`);
    } finally {
      if (!trapped) {
        w.stream3dm_free(slice, SLICE);
        w.stream3dm_free(raw, 48);
        w.stream3dm_drop(session);
      }
    }
  }

  /** Compares two fingerprints (objects or JSON texts, `before` then `after`). */
  diff(before, after) {
    const w = this.#loaded.exports;
    const encoder = new TextEncoder();
    const [a, b] = [before, after].map((f) => encoder.encode(typeof f === 'string' ? f : JSON.stringify(f)));
    const pa = place(w, a);
    const pb = place(w, b);
    const t = w.stream3dm_diff(pa, a.length, pb, b.length) >>> 0;
    w.stream3dm_free(pa, a.length);
    w.stream3dm_free(pb, b.length);
    const result = JSON.parse(read(w, w.stream3dm_text_ptr(t), w.stream3dm_text_len(t)));
    w.stream3dm_text_drop(t);
    if (result.error) throw new Stream3dmError(result.error);
    return result;
  }

  /** Whether the module in use is the full build. */
  get full() {
    return this.#loaded.exports.stream3dm_full() === 1;
  }

  /** Reads again with the full build (loaded once, whatever the number of reads asking). */
  async #again(source, options) {
    this.#full ??= load(this.#fullSource);
    this.#loaded = await this.#full;
    return this.read(source, options);
  }

  /** Replaces a trapped instance with a fresh one of the same module. */
  async #renew(trapped) {
    const fresh = { module: trapped.module, exports: (await WebAssembly.instantiate(trapped.module, {})).exports };
    if (this.#loaded === trapped) this.#loaded = fresh;
    if (this.#full && (await this.#full) === trapped) this.#full = Promise.resolve(fresh);
  }
}

function fingerprintOptions(w, session, { key = null, names = false }) {
  if (key && key.length !== 32) throw new Stream3dmError('a fingerprint key has 32 bytes');
  const at = key ? place(w, key) : 0;
  w.stream3dm_fingerprint_options(session, at, names ? 1 : 0);
  if (at) w.stream3dm_free(at, 32);
}

function stats(w, session, scratch) {
  w.stream3dm_reader_stats(session, scratch);
  const [own, delegated, failed] = new Uint32Array(w.memory.buffer, scratch, 3);
  return { own, delegated, failed, memory: w.memory.buffer.byteLength };
}

/** Copies the meshes collected since the last drain out of linear memory into `chunks`. */
function drain(w, session, scratch, chunks) {
  w.stream3dm_chunk(session, scratch);
  const [pp, pl, cp, cl, ip, il, rp, rl, lp, ll] = new Uint32Array(w.memory.buffer, scratch, 10);
  if (rl === 0) return;
  chunks.push({
    positions: new Float32Array(w.memory.buffer, pp, pl).slice(),
    colors: new Uint8Array(w.memory.buffer, cp, cl).slice(),
    indices: new Uint32Array(w.memory.buffer, ip, il).slice(),
    ranges: new Uint32Array(w.memory.buffer, rp, rl).slice(),
    lines: new Float32Array(w.memory.buffer, lp, ll).slice(),
  });
  w.stream3dm_chunk_clear(session);
}

/** The source in slices of at most SLICE bytes. */
async function* slices(source) {
  if (source instanceof ArrayBuffer) source = new Uint8Array(source);
  if (ArrayBuffer.isView(source)) {
    const bytes = new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
    for (let at = 0; at < bytes.length; at += SLICE) yield bytes.subarray(at, at + SLICE);
  } else if (typeof Blob !== 'undefined' && source instanceof Blob) {
    for (let at = 0; at < source.size; at += SLICE) yield new Uint8Array(await source.slice(at, at + SLICE).arrayBuffer());
  } else if (typeof source === 'function') {
    for await (const chunk of source()) {
      for (let at = 0; at < chunk.length; at += SLICE) yield chunk.subarray(at, at + SLICE);
    }
  } else {
    throw new TypeError('read() takes bytes, a Blob or a function returning an iterable of Uint8Array');
  }
}

function sizeOf(source) {
  if (source instanceof ArrayBuffer || ArrayBuffer.isView(source)) return source.byteLength;
  if (typeof Blob !== 'undefined' && source instanceof Blob) return source.size;
  return null;
}

// Pointers and lengths are u32, which the exports hand to JavaScript as i32: `>>> 0` reads them
// unsigned (from 2 GiB of linear memory up they would be negative).

function place(w, bytes) {
  const at = w.stream3dm_alloc(bytes.length) >>> 0;
  new Uint8Array(w.memory.buffer, at, bytes.length).set(bytes);
  return at;
}

const read = (w, ptr, len) => decoder.decode(new Uint8Array(w.memory.buffer, ptr >>> 0, len >>> 0));
const text = (w, session) => read(w, w.stream3dm_result_ptr(session), w.stream3dm_result_len(session));
