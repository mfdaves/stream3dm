//! Access to a model's displayable geometry while it is inspected.

use crate::View;
use std::cell::RefCell;
use std::rc::Rc;

/// Receives the visible displayable geometry of a model as it streams, in model space
/// (metres), block placements already applied.
pub trait MeshSink {
    /// Called once, before any mesh.
    fn document(&mut self, document: &DocumentInfo);
    /// One visible mesh.
    fn mesh(&mut self, mesh: &PlacedMesh);
    /// One visible curve, sampled into a polyline: consecutive points are joined.
    fn curve(&mut self, _curve: &PlacedWire) {}
    /// Visible points: a point object, or a point cloud.
    fn points(&mut self, _points: &PlacedWire) {}
    /// What a selectable unit is (see [`Pick`]): sent once per unit that has visible meshes,
    /// curves or points, before or after them.
    fn pick(&mut self, _pick: &Pick) {}
}

/// Lets a caller keep a handle on a sink it hands to an [`crate::Inspector`].
impl<T: MeshSink> MeshSink for Rc<RefCell<T>> {
    fn document(&mut self, document: &DocumentInfo) {
        self.borrow_mut().document(document);
    }
    fn mesh(&mut self, mesh: &PlacedMesh) {
        self.borrow_mut().mesh(mesh);
    }
    fn curve(&mut self, curve: &PlacedWire) {
        self.borrow_mut().curve(curve);
    }
    fn points(&mut self, points: &PlacedWire) {
        self.borrow_mut().points(points);
    }
    fn pick(&mut self, pick: &Pick) {
        self.borrow_mut().pick(pick);
    }
}

/// Hands everything to two sinks (for example the viewer's mesh collector and a
/// [`crate::Fingerprinter`]).
pub struct Tee<A: MeshSink, B: MeshSink>(pub A, pub B);

impl<A: MeshSink, B: MeshSink> MeshSink for Tee<A, B> {
    fn document(&mut self, document: &DocumentInfo) {
        self.0.document(document);
        self.1.document(document);
    }
    fn mesh(&mut self, mesh: &PlacedMesh) {
        self.0.mesh(mesh);
        self.1.mesh(mesh);
    }
    fn curve(&mut self, curve: &PlacedWire) {
        self.0.curve(curve);
        self.1.curve(curve);
    }
    fn points(&mut self, points: &PlacedWire) {
        self.0.points(points);
        self.1.points(points);
    }
    fn pick(&mut self, pick: &Pick) {
        self.0.pick(pick);
        self.1.pick(pick);
    }
}

/// Document tables a sink needs before geometry arrives.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentInfo {
    /// The views the report lists ([`Report::views`](crate::Report::views)).
    pub views: Vec<View>,
    /// Indexed by [`PlacedMesh::layer`].
    pub layers: Vec<LayerInfo>,
    /// Indexed by [`PlacedMesh::material`].
    pub materials: Vec<MaterialInfo>,
}

/// A layer as a sink sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerInfo {
    /// Full path from the root layer, `::`-separated.
    pub path: String,
    /// Layer colour as `[r, g, b]`.
    pub color: Option<[u8; 3]>,
}

/// A material as a sink sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct MaterialInfo {
    /// Name as saved (empty when unnamed).
    pub name: String,
    /// The PBR base colour when present, else the diffuse colour, as `[r, g, b]`.
    pub color: Option<[u8; 3]>,
}

/// One visible mesh in model space.
#[derive(Debug, Clone, Copy)]
pub struct PlacedMesh<'a> {
    /// Metres.
    pub positions: &'a [[f64; 3]],
    /// Counter-clockwise seen from outside.
    pub triangles: &'a [[u32; 3]],
    /// Identifies the object (or block placement member) the mesh belongs to.
    pub object: u64,
    /// The selectable unit the mesh belongs to: the object itself in model space, the
    /// top-level block instance for block members (nested blocks included).
    pub pick: u64,
    /// Index into [`DocumentInfo::layers`]: the object's own layer (a block member's, not the
    /// instance's).
    pub layer: Option<usize>,
    /// Index into [`DocumentInfo::materials`]: the object's material, its layer's, or the
    /// placing instance's when the object takes it from its parent; `None` without one.
    pub material: Option<usize>,
    /// Display colour given with the geometry, before the material's or layer's.
    pub color: Option<[u8; 3]>,
}

/// One visible curve (a polyline) or set of points in model space, like [`PlacedMesh`] without
/// triangles.
#[derive(Debug, Clone, Copy)]
pub struct PlacedWire<'a> {
    /// Metres.
    pub positions: &'a [[f64; 3]],
    /// As in [`PlacedMesh::object`].
    pub object: u64,
    /// As in [`PlacedMesh::pick`].
    pub pick: u64,
    /// As in [`PlacedMesh::layer`].
    pub layer: Option<usize>,
    /// As in [`PlacedMesh::material`].
    pub material: Option<usize>,
}

/// A selectable unit of the model: an object in model space, or a top-level block instance
/// with every member it places. Layer and material are the object's or the instance's own.
#[derive(Debug, Clone, Copy)]
pub struct Pick<'a> {
    /// As in [`PlacedMesh::pick`].
    pub pick: u64,
    /// UUID as lowercase hyphenated text (for IFC products, the GlobalId); `None` when the
    /// object has none.
    pub uuid: Option<&'a str>,
    /// Object or instance name, trimmed; `None` when empty.
    pub name: Option<&'a str>,
    /// `brep` (polysurfaces, surfaces and extrusions), `mesh`, `subd`, `block`, or `other`.
    pub kind: &'static str,
    /// Index into [`DocumentInfo::layers`].
    pub layer: Option<usize>,
    /// Index into [`DocumentInfo::materials`].
    pub material: Option<usize>,
    /// The block definition's name, for block instances; the IFC type, for IFC products.
    pub block: Option<&'a str>,
    /// The IFC class (`IfcWall`, …), for IFC products.
    pub class: Option<&'a str>,
    /// The Rhino groups it belongs to, as indices into the report's `structure.groups`.
    pub groups: &'a [usize],
    /// Attribute user strings as `(key, value)`, in file order; for IFC products, attributes
    /// then `Set.Property` values.
    pub user_strings: &'a [(String, String)],
}

/// FNV-1a, for stable object identifiers.
pub(crate) fn object_id(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.iter().chain(b"\x1f") {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}
