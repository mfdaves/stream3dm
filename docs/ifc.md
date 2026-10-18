# IFC

`crates/stream3dm/src/ifc/` reads IFC files (IFC2X3, IFC4, IFC4X3) in slices: `step.rs` splits
the STEP physical file into records and arguments as bytes arrive, `mod.rs` builds the model.
It reads no geometry; the meshes come from a geometry engine, product by product, and enter the
same pipeline as a `.3dm`'s objects (report, picks, fingerprint).

## What is read

| | |
|---|---|
| Spatial structure | storeys through containment, aggregation, nesting, voids, fills and projections; the file's storeys as `proportions.storeys` |
| Products | class (`IfcWall`, …), name, global id, type (an `IfcTypeObject` acts as a block definition), material |
| Materials | the thickest layer of a layer set, the first of a list or of a constituent or profile set |
| Properties | property sets and quantities, door and window panel and lining properties, as `Set.Property` user strings (built on demand, up to 1,000 per product) |
| Units | SI prefixes, conversion-based units (feet) |

In the report, layers are `storey::class` (`Level 1::IfcWall`), types are blocks and each
product's parts carry their colours.

## Use

```sh
stream3dm ifc model.ifc              # summary and read speed
stream3dm ifc model.ifc --products   # every product as JSON lines
```

In Rust, `IfcInspector` takes the bytes (`push`), resolves the model (`prepare`), then each
product's mesh from the geometry engine (`add_product`) and `finish`es with a `Report`. The
raw WebAssembly exports `stream3dm_ifc_new_with_meshes`, `stream3dm_ifc_prepare` and
`stream3dm_ifc_product` do the same from JavaScript, for example with the meshes web-ifc makes;
the `js/stream3dm.js` wrapper reads `.3dm` files only.

## Measured

On web-ifc's 28 public test files (`samples/fetch.sh ifc.tsv`, 415 MB), AMD Ryzen 5 PRO 7540U,
October 2026:

- Speed (`stream3dm ifc`, push and prepare, the files in the page cache, median of five rounds):
  258 MB/s over the 28 files; the 73 MB model in 0.27–0.29 s with a 40.8 MB peak heap.
- Against IfcOpenShell 0.9.0 on the 55,091 products of the 27 files it opens (not the IFC4X3_RC2
  road): storey 99.99 % (the 5 others are IfcProxy objects to which IFC2X3 gives no containment
  inverse; this reader follows the relationship), type 100 %, material 100 %, property and
  quantity values 100 % (1,218,924; where two of a product's sets give one property, the later
  relationship's value stands, as in IfcOpenShell). `tools/bench/ifc/ifcopenshell_meta.py` makes the
  comparison per file: `python3 -I tools/bench/ifc/ifcopenshell_meta.py <dir holding the
  ifcopenshell package> target/release/stream3dm <file.ifc>…` prints one JSON line per file
  with the products agreeing on each field.
- web-ifc's own property API (`properties.getPropertySets`) costs 0.07–52 ms per product; this
  reader resolves all of them in the same pass as the rest.

## Not yet

Geometry of its own (extrusions, face sets and mapped items first, checked against IfcOpenShell
product by product), presentation layers (`IfcPresentationLayerAssignment`), spaces as rooms,
transparency from styles.
