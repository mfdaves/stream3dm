# stream3dm fingerprint, version 2

A fingerprint identifies one version of a Rhino `.3dm` model and lets two versions be compared,
without containing the model. It is computed where the file is read (in the browser, or by
`stream3dm fingerprint`), and it is what a versioning service stores instead of the file.

This document specifies version 2 completely, so that any implementation produces the same
bytes. The reference implementation is `crates/stream3dm/src/fingerprint.rs`; test vectors are in
`crates/stream3dm/tests/golden/` (see *Test vectors*).

## Properties

- **Identity.** Two files with the same visible geometry, document tables and identities give
  the same fingerprint, whatever the order of their records, the batch size used to read them,
  the platform (native or WebAssembly) or the unit system the model was drawn in.
- **Comparison.** Objects keep their identity across saves (Rhino object UUIDs), so a diff can
  say which objects were added, removed, moved without changing shape or reshaped (and by
  about how much), put on another layer, or re-created under a new identity, and which layers,
  materials, blocks and views changed.
- **Not invertible.** Geometry and names enter only through keyed hashes. In clear are a
  summary (counts, overall size to the metre, total area to two significant digits), the
  number of objects per layer and of placements and members per block, and for each object its
  layer's position in the list and its area to a factor of two. See *Threat model*.
- **Small.** About 110 bytes of JSON per object before compression, which roughly halves it.

## Inputs

1. **Objects.** The visible displayable geometry, as triangle meshes in model space, in
   metres, block placements applied. One *object* per model-space object, and one per
   block-placement member (a window block placed ten times gives ten objects). Objects that
   have no displayable mesh (curves, text, Breps without a render mesh that could not be
   tessellated) contribute only to the counts.
2. **Document tables.** Layers (path, colour, visible, locked, material), materials (name,
   colour, physically based parameters, texture file names), block definitions (name,
   placements, members, size), views (kind, name, projection, camera, direction, up, target,
   frustum, clipping planes).
3. **Counts.** The geometry inventory.
4. **A key** `K` of 32 bytes. It is chosen at random by the owner, once per project, and is
   **never** sent or stored with the fingerprint. Fingerprints made with different keys
   cannot be compared. Without a key, the *public key* (32 zero bytes) is used and the
   fingerprint says `"keyed": false`; it is meant for tests and public samples only.

## Primitives

- `H(tag, data)` = HMAC-SHA-256 with key `K` over the ASCII `tag`, one zero byte, then `data`.
- `h64(tag, data)`: the first 8 bytes of `H`, as 16 lowercase hexadecimal digits.
- `h32(tag, data)`: the first 4 bytes of `H`, as 8 lowercase hexadecimal digits.
- `h256(tag, data)`: all 32 bytes, as 64 lowercase hexadecimal digits.
- `text(a, b, ...)`: the UTF-8 text of each field joined by the byte `0x1f`. Integers are
  written in decimal without leading zeros (`-` for negatives); absent values are empty.
- `q(v)` = `v × 1000` rounded to the nearest integer, halves away from zero, as a signed 64-bit
  integer: a length in metres becomes millimetres.
- `qn(v)` = `v × 10000` rounded the same way (unit vectors, ratios).
- `le64(n)`: `n` as 8 bytes little-endian (two's complement for signed values).

## Objects

### Identity

Each object has a 64-bit identity, FNV-1a 64 (offset `0xcbf29ce484222325`, prime
`0x100000001b3`) over a list of byte strings, each followed by the byte `0x1f`:

- a model-space object: its UUID as lowercase hyphenated text
  (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`), or `record N` when it has none (N = its position
  among the object records);
- a block-placement member: the UUID text of the top-level block instance that places it (or
  `instance N`), the UUID text of the definition that lists the member, `le32(k)` where `k`
  counts the earlier placements of that definition by the same top-level instance (0 for the
  first), and the member's UUID text;
- a tessellated face group without its own object: the reader's key for it.

`key = h64("object", le64(identity))`.

### Geometry

Every triangle is quantized: each vertex becomes `(q(x), q(y), q(z))`. A triangle with two
equal quantized vertices is skipped. The three vertices are rotated, keeping their order
(orientation), so that the smallest vertex `p` comes first (compare x, then y, then z), giving
`p, q, r`. Its nine coordinates, as unsigned 64-bit two's complement, feed two *position*
lanes; its two edge vectors `q − p` and `r − p` (six integers) feed two *shape* lanes:

```
mix(x):  x = x + 0x9E3779B97F4A7C15
         x = (x xor (x >> 30)) * 0xBF58476D1CE4E5B9
         x = (x xor (x >> 27)) * 0x94D049BB133111EB
         return x xor (x >> 31)                        (all arithmetic modulo 2^64)
lane(seed, values): h = seed; for each integer v of values: h = mix(h xor v); return h
t0 = lane(0x0123456789ABCDEF, position), t1 = lane(0xFEDCBA9876543210, position)
e0 = lane(0x5555555555555555, edges),    e1 = lane(0xAAAAAAAAAAAAAAAA, edges)
```

Per object, over its kept triangles: `S0 = Σ t0`, `S1 = Σ t1`, `R0 = Σ e0`, `R1 = Σ e1`
(modulo 2^64), `n` = their number, the corner sums `Σv` (3 values) and second moments
`Σ x², Σ y², Σ z², Σ xy, Σ xz, Σ yz` over the `3n` corners (each triangle contributes its three
quantized vertices; signed 64-bit, wrapping), the bounding box `min`/`max` of their quantized vertices, and `A2`, the sum over the
triangles of twice their area in mm²: with the edge vectors `u = b − a`, `v = c − a` as IEEE
doubles, the cross product computed in doubles (`u_y v_z − u_z v_y`, `u_z v_x − u_x v_z`,
`u_x v_y − u_y v_x`), its components squared and summed in x, y, z order, square-rooted and
rounded half away from zero, without fused multiply-adds.

- `geometry = h64("geometry", le64(S0) ‖ le64(S1) ‖ le64(n))`. Sums do not depend on triangle
  order, so the value only changes when the triangles do.
- `shape = h64("shape", le64(R0) ‖ le64(R1) ‖ le64(n) ‖ le64(C_xx) ‖ le64(C_yy) ‖ le64(C_zz)
  ‖ le64(C_xy) ‖ le64(C_xz) ‖ le64(C_yz))`, with the central moments
  `C_ij = 3n · Σ v_i v_j − Σ v_i · Σ v_j` (signed 64-bit, wrapping). Edge vectors and central
  moments are exactly invariant to translation, also modulo 2^64 (only additions,
  subtractions and products are involved): an object moved without changing form keeps its
  shape; moving one part of it changes the moments.
- `layer`: the index, in the fingerprint's `layers` array, of the layer of the object's first
  triangle, or `-1`.
- `cells`: the centre `c = floor((min + max) / 2)` per axis (millimetres), placed in four grids
  of `(size, offset)` = `(100, 0)`, `(100, 50)`, `(1000, 0)`, `(1000, 500)` millimetres:
  `cell = floor((c + offset) / size)` per axis, and
  `h32("cell", le32(size) ‖ le32(offset) ‖ le64(cell_x) ‖ le64(cell_y) ‖ le64(cell_z))`. The
  four values are concatenated (32 hexadecimal digits). Two grids per size, offset by half a
  cell, make "moved by less than about the cell size" robust to cell borders.
- `area_bits`: the number of significant bits of `A2` (0 when `A2` is 0): area to within a
  factor of two.

## Tables

All entries are sorted by `key` (as text).

| Section | key | Other fields |
|---|---|---|
| `layers` | `h64("layer", path)` (`::` between levels) | `props = h64("layer-props", text(colour, visible, locked, material))`; `objects`: number of objects on it; `content = h64("layer-content", the 16 bytes key ‖ geometry of its objects, sorted, concatenated)` |
| `materials` | `h64("material", name)` | `params = h64("material-params", text(colour, physically_based, qn(metallic), qn(roughness), qn(opacity), texture names sorted and joined by 0x1e))` |
| `blocks` | `h64("block", name)` | `placements`, `members` (counts); `shape = h64("block-shape", text(members, q(size_x), q(size_y), q(size_z)))` |
| `views` | `h64("view", text(kind, name))` | `camera = h64("view-camera", text(projection, q of camera, qn of direction, qn of up, q of target, q of frustum, qn/q of each clipping plane))` |

Colours are `#rrggbb` lowercase; booleans `true`/`false`; kind is `viewport` or `named`;
projection `parallel`, `perspective` or `other`. With `"names": "clear"` each entry also
carries its `name` (path for layers); the default, `"hashed"`, leaves names out.

## Digests

- `geometry = h256("geometry-set", every object's geometry, 8 bytes each, sorted, concatenated)`:
  the shape of the model alone, independent of identities and names. A model rebuilt with new
  objects of the same shape keeps it.
- `objects = h256("object-set", per object, in `objects` order: the ASCII hexadecimal text of
  key, then of geometry, then le64(layer index))`.
- `structure = h256("structure", per entry, in array order and in the section order of the
  table above: the ASCII hexadecimal text of each hashed field and le64 of each count, in the
  order the fields are listed in the table (layers: key, props, objects, content; materials:
  key, params; blocks: key, placements, members, shape; views: key, camera))`.
- `document = h256("document", geometry ‖ objects ‖ structure ‖ the summary's canonical JSON)`:
  equal exactly when two fingerprints are equal.
- `key_id = h64("key-id", "")`: tells whether two fingerprints were made with the same key,
  without revealing it.

## Format

UTF-8 JSON, keys in the order below, no insignificant whitespace in the canonical form. The
figures are those of `samples/synthetic/house.3dm` (public key):

```json
{
  "format": "stream3dm-fingerprint", "version": 2, "keyed": false, "key_id": "…", "names": "hashed",
  "summary": {
    "unit_system": "meters", "archive_version": 80,
    "size_m": [34, 25, 9], "area_m2": 2800, "triangles": 1586,
    "counts": { "objects": 75, "fingerprinted": 123, "layers": 23, "materials": 8,
                "blocks": 5, "placements": 20, "views": 4, "breps": 1, "meshes": 56,
                "subds": 0, "curves": 0, "points": 0, "annotations": 0, "hatches": 0,
                "lights": 0 }
  },
  "digests": { "document": "…", "geometry": "…", "objects": "…", "structure": "…" },
  "layers": [ { "key": "…", "props": "…", "objects": 17, "content": "…" } ],
  "materials": [ { "key": "…", "params": "…" } ],
  "blocks": [ { "key": "…", "placements": 6, "members": 2, "shape": "…" } ],
  "views": [ { "key": "…", "camera": "…" } ],
  "object_fields": ["key", "layer", "geometry", "shape", "cells", "area_bits"],
  "objects": [ ["…", 16, "…", "…", "…", 28] ]
}
```

`size_m` is the size of the objects' overall bounding box, `(max − min + 500) div 1000` per
axis (whole metres); `area_m2` is `(ΣA2 + 1,000,000) div 2,000,000` (square metres) rounded
half up to two significant digits. `counts.objects` and the counts by kind come from the
geometry inventory (all objects, hidden ones included); `counts.fingerprinted` is the length
of `objects`. `archive_version` is null when unknown.

## Comparing two fingerprints

Fingerprints are comparable when `version` and `key_id` are equal. Then:

- equal `document`: the versions are the same;
- summary fields that differ are listed (for example `archive_version` for the same model
  saved by another Rhino version);
- objects are matched by `key`. Present in both: *unchanged* (same geometry and layer),
  *moved to another layer* (same geometry), or *changed*, and then *moved* (same shape) or
  *reshaped*, with a distance: within about 0.1 m when one of its two 0.1 m cells is shared,
  within about 1 m when one of its two 1 m cells is, otherwise beyond; also whether its layer
  changed and whether `area_bits` changed (area changed by a factor of two or more);
- objects only in the first version and objects only in the second are paired as
  *re-created*: first those with equal `geometry` (in place), then those with equal `shape`
  (elsewhere), each group paired in key order; the rest are *removed* and *added*;
- equal `geometry` with different `objects`: the same shapes under new identities (objects
  re-created or copied);
- layers, materials, blocks and views are matched by `key`: *added*, *removed* or *changed*
  (any other field differs). A renamed layer appears as one removed and one added, its objects
  as moved to another layer.

## Threat model

The fingerprint is designed to be stored by a service that must not learn the model.

- Without `K`, every hash is a pseudorandom value: nothing about shapes, positions or names
  can be computed or tested from it, including guessing (is a given standard block in the
  file?), because each guess needs `K`.
- The clear part reveals the counts of objects by kind, of layers, materials, blocks and
  views, the overall size to the metre, the total area to two significant digits and the
  triangle count; the number of objects on each layer (`layers[].objects`) and of placements
  and members of each block; and, per object, the position of its layer in the sorted `layers`
  and `area_bits`, its area to a factor of two. So it shows how objects spread over layers and
  how large each is to within a factor of two, but no positions, no shapes and no names.
- Across versions, the service learns which objects changed and by roughly how much (the
  diff above), not where or into what.
- With `K` (the owner), the cells are coarse by design: positions to 0.1 m and 1 m; a
  fingerprint is still not a model.
- The public key offers none of this: with it, anyone can test guesses. Use it only for
  public files.

## Test vectors

`crates/stream3dm/tests/fingerprint.rs` checks, with the public key:

- `samples/synthetic/house.3dm` against `crates/stream3dm/tests/golden/house.fingerprint.json`;
- the same fingerprint for any batch and slice size, with the object records shuffled, and for
  the same model saved as Rhino 7 (`house_v7.3dm`, `geometry` and `objects` equal);
- `house_moved.3dm` (one object moved 1 cm): exactly that object changed, moved (same
  shape), within 0.1 m;
- synthetic objects fed straight to the fingerprinter: a translation is a move, a stretch and
  a part moved inside an object are reshapes, a layer change is flagged with a reshape, and
  copies under new identities are re-created (in place or elsewhere);
- `house_renamed.3dm` (one layer renamed): one layer removed and one added, its objects moved
  to another layer, `geometry` unchanged;
- `house_added.3dm` (one object added): exactly one object added;
- no name, path or coordinate of the model appears in the fingerprint, and every number in it
  is an integer;
- a key changes every hash but not the summary, and fingerprints made with different keys
  are refused by the comparison.

`tools/wasm/fingerprint.mjs` checks that the WebAssembly build and the native binary produce
the same bytes (all 193 public samples, the synthetic house and its variants, 2026-10-15).

## Versioning

`version` changes whenever any rule above changes the bytes produced. Readers reject versions
they do not know.
