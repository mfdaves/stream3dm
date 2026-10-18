// Types of stream3dm.js. The report's and the context's full shapes are those of the Rust crate
// (`stream3dm::Report`, `stream3dm::Context`), serialized with serde; only their top level is
// spelled out here.

/** Where the module comes from: a URL, a response, its bytes or a compiled module. */
export type ModuleSource = URL | string | Response | Promise<Response> | BufferSource | WebAssembly.Module;

/**
 * Loads the module (default: `stream3dm_wasm.wasm` next to stream3dm.js; a relative string
 * resolves against stream3dm.js's URL). `full`: the full build, loaded only for a file that
 * needs it.
 */
export function init(wasm?: ModuleSource, options?: { full?: ModuleSource | null }): Promise<Stream3dm>;

/** Not a .3dm file, truncated or invalid. */
export class Stream3dmError extends Error {}

/** A file to read: its bytes, a Blob or File, or a function giving its slices (called once per pass). */
export type Source =
  | ArrayBuffer
  | ArrayBufferView
  | Blob
  | (() => AsyncIterable<Uint8Array> | Iterable<Uint8Array>);

export interface ReadOptions {
  /** Collect the visible meshes, curves and points (and the fingerprint). */
  meshes?: boolean;
  /** With meshes: the feature edges as line segments (default true). */
  edges?: boolean;
  /** Compute the fingerprint: with a 32-byte owner key (default the public key), names in clear. */
  fingerprint?: boolean | { key?: Uint8Array | null; names?: boolean };
  /** Called after each slice; `total` is null for a function source. */
  onProgress?: (progress: { pass: number; bytes: number; total: number | null }) => void;
}

/**
 * Buffers collected while reading. Positions are metres from `MeshSet.origin`. `ranges` has
 * eight u32 per mesh: first vertex, vertex count, layer + 1, material + 1 (0 = none; indices
 * into the report's `structure.layers` and `materials`), object id, pick, index count, line
 * segment count. A mesh's indices and segments follow the previous mesh's in the chunk; a curve
 * has segments only, a set of points vertices only.
 */
export interface MeshChunk {
  /** x, y, z per vertex. */
  positions: Float32Array;
  /** r, g, b per vertex (the display colour). */
  colors: Uint8Array;
  /** Three per triangle, into the chunk's vertices. */
  indices: Uint32Array;
  ranges: Uint32Array;
  /** Pairs of x, y, z: feature edges and curves. */
  lines: Float32Array;
}

/** What a pick (a selectable unit: an object, or the top-level block instance of a member) is. */
export interface Pick {
  kind: 'brep' | 'mesh' | 'subd' | 'block' | 'other';
  uuid?: string;
  name?: string;
  layer?: number;
  material?: number;
  block?: string;
  class?: string;
  groups?: number[];
  user_strings?: [string, string][];
}

export interface MeshSet {
  /** The origin (metres) subtracted from every position. */
  origin: [number, number, number];
  chunks: MeshChunk[];
  /** Pick n at position n - 1 (null when undescribed). */
  picks: (Pick | null)[];
}

/** The extraction report (stream3dm::Report). */
export interface Report {
  schema_version: number;
  [field: string]: unknown;
}

export interface ReadResult {
  report: Report;
  /** A compact summary of the model (stream3dm::Context). */
  context: Record<string, unknown>;
  /** Records decoded by our reader, by cadmpeg, failed; the module's memory in bytes. */
  stats: { own: number; delegated: number; failed: number; memory: number };
  meshes?: MeshSet;
  /** With meshes or fingerprint (docs/fingerprint.md). */
  fingerprint?: Record<string, unknown>;
  /** [object id as in the ranges, key] for the fingerprinted objects. */
  objectKeys?: [number, string][];
}

export class Stream3dm {
  /** Made by `init`. */
  private constructor();
  /** Reads a .3dm file. */
  read(source: Source, options?: ReadOptions): Promise<ReadResult>;
  /** Compares two fingerprints, `before` then `after` (objects or JSON texts). */
  diff(before: object | string, after: object | string): Record<string, unknown>;
  /** Whether the module in use is the full build. */
  readonly full: boolean;
}
