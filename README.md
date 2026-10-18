# stream3dm

Reads Rhino `.3dm` files, and the metadata of IFC files, in Rust and in WebAssembly: a structured
report of the model (units, layers, materials, views, blocks, geometry, proportions), its meshes
ready to draw, and a fingerprint that tells two versions of a model apart without containing it.
In a browser the file is read on the user's machine, in slices, and never uploaded.

```sh
cargo run --release -p stream3dm-cli -- inspect model.3dm          # summary
cargo run --release -p stream3dm-cli -- inspect model.3dm --json   # the full report
```

## What it reads

| | |
|---|---|
| Files | Rhino 1 to 8 (archive versions 1, 2, 3, 4, 50, 60, 70, 80), read as a stream of slices: memory stays bounded by the largest record, not the file (Rhino 1 files, which have no tables to stream, are held whole) |
| Document | units and tolerances, the saving application, layers (tree, colours, visibility), materials (with PBR values and texture file names), named views and viewports, groups, block definitions, lights, the earth anchor (latitude, longitude, north), Rhino's Sun panel (on, date, time, time zone, manual position) and render ground plane |
| Objects | meshes; Breps and extrusions, from Rhino's cached render meshes or tessellated when the file has none ("Save Small"); SubD objects, subdivided from their control nets; curves and points; block instances (nested), annotations, hatches; names, layers, materials, groups and user strings per object |
| Report | the concepts above as JSON (`schema_version` versioned), plus proportions (box, size, storeys and levels where they can be told), style signals (planar and curved area, alignment, repetition, glazing share) and diagnostics (objects that cannot be drawn, unknown units) |
| Meshes | per visible object, block placements applied, in metres, with its layer, material and selectable unit (the object, or the block instance it belongs to); the WebAssembly module hands them over in buffers from an origin, with display colours and feature edges |
| Fingerprint | per object a keyed hash of its geometry and shape, so a diff tells moved from reshaped, added from removed, without revealing the geometry ([docs/fingerprint.md](docs/fingerprint.md)) |
| IFC | IFC2X3, IFC4, IFC4X3: storeys, classes, types, materials, property sets and quantities, read in slices (the model's entities are kept until it is resolved); the meshes come from a geometry engine (web-ifc, for example) and enter the same report ([docs/ifc.md](docs/ifc.md)) |

The decoder is our own, written from the openNURBS file format and checked record by record
against [cadmpeg](https://crates.io/crates/cadmpeg-codec-rhino) (Apache-2.0), which stays an
optional dependency (feature `cadmpeg`, on by default) as a fallback for rare record versions
and as the reference in tests. Without it the crate is pure Rust (its dependencies: serde and
serde_json, miniz_oxide for compressed buffers, crc32fast, sha2, and earcut and spade for
tessellation). Notes on the byte layouts: [docs/3dm-format.md](docs/3dm-format.md).

## Measured

On an AMD Ryzen 5 PRO 7540U (October 2026), against rhino3dm.js 8.35 (what three.js's
3DMLoader and most browser tools use), in Node, the same files, one process per run:

| File | Size | Read: stream3dm | Read: rhino3dm.js | Peak memory: stream3dm | Peak memory: rhino3dm.js | Triangles shown | Breps shown |
|---|---:|---:|---:|---:|---:|---|---|
| Synthetic neighbourhood (`npm run neighbourhood` in `tools/fixtures`) | 57 MB | 1.01 s | 1.31 s | 97 MB | 538 MB | 4,820,486 vs 4,820,486 | – |
| `v4_Wheel_PG.3dm` (openNURBS samples) | 11 MB | 0.15 s | 0.22 s | 110 MB | 159 MB | 519,498 vs 519,498 | 6 of 6 vs 2 of 2 |
| A real architectural model (Rhino 6, not public) | 189 MB | 2.09 s | 4.23 s | 137 MB | 1,441 MB | 8,045,956 vs 7,737,911 | 1,134 of 1,134 vs 0 of 1,033 |

Times include collecting the meshes and computing the fingerprint (feature edges off, as
rhino3dm.js has none); peak memory is the process's maximum resident set. rhino3dm.js shows only
the render meshes Rhino cached, so files saved with "Save Small", or Breps and extrusions written
by code, appear empty there.

Over all 193 public samples and two synthetic files: both open every file; stream3dm shows the
same triangles on 100 files and more on 95 (on 84 of them rhino3dm.js shows nothing), and never
fewer; Breps shown 1,255 of 1,257 against 277 of 941 (rhino3dm.js counts its 27 extrusions
apart: 285 of 968 with them); curves and points the same (5,079 and 389). The WebAssembly
module, without cadmpeg, reads all 193 public samples with no record left to the fallback.

To run the comparison: build the WebAssembly module (below), `npm ci` in `tools/fixtures` (it
installs rhino3dm.js), then `node tools/bench/rhino3dm.mjs <file.3dm>…`.

## Using it

### Rust

```toml
[dependencies]
stream3dm = { git = "https://github.com/mfdaves/stream3dm" }

# The reader is slow unoptimized; in a debug build of your crate, optimize it anyway:
[profile.dev.package.stream3dm]
opt-level = 2
```

The default `cadmpeg` feature builds C code (zstd-sys), so it needs a C compiler. For
WebAssembly, or for pure Rust, use `default-features = false`.

```rust,no_run
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read("model.3dm")?;
    let report = stream3dm::inspect(&bytes)?;
    println!("{} objects, {} layers", report.inventory.objects, report.structure.layers.len());
    Ok(())
}
```

`Inspector` reads in slices (`push`, then `finish`; files with blocks ask for a second pass);
a `MeshSink` receives each visible mesh as it is read; `fingerprint_bytes` and
`fingerprint::diff` compare versions; `IfcInspector` reads IFC files. The API documentation,
with examples: `cargo doc -p stream3dm --open`.

### Command line

```sh
stream3dm inspect model.3dm [--json] [--stats]   # the report; --stats: time, MB/s, peak heap
stream3dm fingerprint model.3dm > v1.json         # keyed with STREAM3DM_FINGERPRINT_KEY (64 hex digits)
stream3dm diff v1.json v2.json                    # what changed between two versions
stream3dm render model.3dm --out passes/          # per view: clay, depth, normals, materials, layers, lines, preview
stream3dm ifc model.ifc [--products]              # IFC metadata: summary, or every product as JSON
```

Build it with `cargo build --release -p stream3dm-cli` (the binary is `target/release/stream3dm`);
`stream3dm --help` lists every command.

### JavaScript

```sh
rustup target add wasm32-unknown-unknown   # once
cd js && npm run build                     # builds the WebAssembly module and copies it next to stream3dm.js
```

```js
import { init } from './js/stream3dm.js';
const reader = await init();
const { report, meshes, fingerprint } = await reader.read(file, { meshes: true });
```

`read` takes bytes, a `Blob` or `File`, or a function giving slices; the result's meshes are
typed arrays ready for a `BufferGeometry`. It runs in browsers, Web Workers and Node 20+, without
dependencies; types in `js/stream3dm.d.ts`. [examples/browser/](examples/browser/index.html)
opens a file and draws it with three.js.

The module is 1.7 MB (0.54 MB gzipped). The full build adds cadmpeg, for the rare record versions
the small module leaves to it: `cargo build -j 1 --release -p stream3dm-wasm --target
wasm32-unknown-unknown --features full --target-dir target/full` (16.7 MB, 2.9 MB gzipped; its C
code needs clang with the wasm32 target). Pass it as `init(url, { full })` and it is loaded only
for a file that needs it; without it such records are reported as undecoded objects.

## Building and testing

```sh
samples/fetch.sh                 # the public sample files (see samples/manifest.tsv)
cargo test -j 2                  # unit tests, every sample, golden reports, streaming invariance
cargo test -j 2 -p stream3dm --no-default-features   # the same without cadmpeg
cargo build -j 1 --release -p stream3dm-wasm --target wasm32-unknown-unknown
node --test js/test/stream3dm.test.mjs
```

The samples come from McNeel's openNURBS, rhino3dm and rhino-developer-samples repositories and
from three.js, pinned to upstream commits and checked by SHA-256 (`samples/fetch.sh ifc.tsv`
fetches the IFC files: web-ifc's public test models). They stay under their owners' licences and
are not part of this repository; `samples/synthetic/` holds fixtures generated by
`tools/fixtures/` with known values.

| Crate | |
|---|---|
| `stream3dm` | the reader and the report (no I/O, builds for `wasm32-unknown-unknown`) |
| `stream3dm-cli` | the `stream3dm` command |
| `stream3dm-render` | a CPU rasterizer for the passes of a view |
| `stream3dm-wasm` | raw WebAssembly exports (`js/stream3dm.js` wraps them) |

## Licence

Dual-licensed under the MIT licence ([LICENSE-MIT](LICENSE-MIT)) or the Apache License 2.0
([LICENSE-APACHE](LICENSE-APACHE)), at your option. Contributions are taken under the same terms
([CONTRIBUTING.md](CONTRIBUTING.md)).

---

Rhinoceros® and Rhino® are registered trademarks, and openNURBS a trademark, of Robert McNeel &
Associates. stream3dm is an independent project, not affiliated with or endorsed by McNeel. IFC is
a standard of buildingSMART International.
