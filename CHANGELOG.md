# Changelog

## 0.1.0 (2026-10-18)

First public version.

- `.3dm` reader of our own for Rhino 1 to 8 files, streamed in slices: document tables, meshes,
  Breps and extrusions (cached render meshes, or tessellated when there are none), SubD
  (subdivided control nets), curves, points, blocks, annotations, user strings, lights and the
  earth anchor, Rhino's Sun panel and render ground plane (from the RDK document data); cadmpeg
  as an optional fallback and test reference.
- The report (`schema_version` 3): units, structure, blocks, materials, views, inventory,
  proportions, style signals, diagnostics; a compact context derived from it.
- Meshes for drawing through `MeshSink`, with what each selectable unit is; the WebAssembly
  module adds display colours and feature edges.
- The fingerprint and its diff ([docs/fingerprint.md](docs/fingerprint.md)).
- IFC metadata reader (storeys, classes, types, materials, property sets, quantities).
- The `stream3dm` command; a CPU rasterizer for view passes; raw WebAssembly exports with a
  JavaScript wrapper (`js/`) and a browser example.

The API may still change before 1.0.
