//! Report types. Lengths are metres, areas square metres, angles degrees.

use serde::Serialize;

/// What the reader extracts from a model: file facts, document tables, an inventory of the
/// objects and figures computed from their visible displayable meshes. Values are rounded
/// (most lengths to 1e-6 m, ratios to 1e-4, angles to 1e-3°) so native and WebAssembly builds
/// give the same report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// Version of this layout ([`SCHEMA_VERSION`](crate::SCHEMA_VERSION)).
    pub schema_version: u32,
    /// Size, version, saving application, units and tolerance.
    pub file: FileInfo,
    /// Layers, groups, object names and user strings.
    pub structure: Structure,
    /// Block definitions, sorted by name (for IFC models: the IFC types).
    pub blocks: Vec<Block>,
    /// The material table, in archive index order.
    pub materials: Vec<Material>,
    /// Saved viewports, then named views, each in file order. Views without a camera
    /// location, direction or up vector are left out.
    pub views: Vec<View>,
    /// The file's lights (left out when it has none).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lights: Vec<Light>,
    /// Rhino's Sun panel and render ground plane (left out when the file has no RDK document
    /// data, as files from before Rhino 6 and most files not saved by Rhino).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render: Option<RenderSettings>,
    /// Objects counted by kind.
    pub inventory: Inventory,
    /// Size and floor levels of the visible displayable meshes; `None` when there are none.
    pub proportions: Option<Proportions>,
    /// Orientation, glazing and repetition figures.
    pub style: StyleSignals,
    /// The visible geometry broken down per object or per group.
    pub elements: Elements,
    /// What the reader could not read or had to assume.
    pub diagnostics: Vec<Diagnostic>,
}

/// Rhino's render settings that set the light of a view.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RenderSettings {
    /// The Sun panel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sun: Option<Sun>,
    /// The render ground plane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ground_plane: Option<GroundPlane>,
}

/// Rhino's sun, as the Sun panel saved it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sun {
    /// The panel's "On": whether Rhino lights the model with this sun.
    pub enabled: bool,
    /// The position is set by hand (`azimuth`, `altitude`) rather than by date, time and place.
    pub manual: bool,
    /// Compass bearing of the sun, degrees clockwise from north (east 90), rounded to 1e-3.
    /// Rhino stores it in both modes; outside manual mode it is Rhino's last computed value.
    pub azimuth: f64,
    /// Degrees above the horizon, rounded to 1e-3 (stored as `azimuth` is).
    pub altitude: f64,
    /// Local date, `YYYY-MM-DD`.
    pub date: String,
    /// Local time in decimal hours, rounded to 1e-6.
    pub hours: f64,
    /// Hours from UTC of local standard time.
    pub utc_offset: f64,
    /// Minutes added to the clock when daylight saving is on; `None` when off.
    pub daylight_saving_minutes: Option<i32>,
    /// Degrees, north positive; `None` when the earth anchor holds the place.
    pub latitude: Option<f64>,
    /// Degrees, east positive; `None` when the earth anchor holds the place.
    pub longitude: Option<f64>,
    /// Direction of north, degrees counter-clockwise from the model's +x axis (90 is +y);
    /// `None` when the earth anchor holds it.
    pub north: Option<f64>,
    /// Strength Rhino gives the sun (1 by default).
    pub intensity: f64,
    /// Whether the sky lights the model too (the panel's skylight).
    pub skylight: bool,
}

/// Rhino's render ground plane: an endless horizontal plane under the model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroundPlane {
    /// Whether it is on.
    pub enabled: bool,
    /// Its height, rounded to 1e-6 m; with `auto_altitude` Rhino puts it at the model's lowest
    /// point instead.
    pub altitude_m: f64,
    /// The plane follows the model's lowest point.
    pub auto_altitude: bool,
    /// It only catches shadows (otherwise it shows its material).
    pub shadow_only: bool,
    /// It is visible from below.
    pub show_underside: bool,
}

/// Visible displayable geometry broken down for reading: per object (and per block placement)
/// when the model has at most [`MAX_OBJECT_ELEMENTS`] of them, otherwise grouped by layer,
/// material and block.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Elements {
    /// Listed per object and per top-level block placement; `false` when grouped.
    pub per_object: bool,
    /// Per object: sorted by layer, block, name, then lowest first and larger first. Grouped:
    /// largest area first.
    pub items: Vec<Element>,
}

/// Objects and top-level block placements listed one by one at most; beyond this, elements
/// are grouped by layer, material and block.
pub const MAX_OBJECT_ELEMENTS: usize = 300;

/// One object, block placement or group of them, with figures from its visible meshes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Element {
    /// Object name (per-object listing only).
    pub name: Option<String>,
    /// Layer path; for a block placement listed per object, the instance's layer.
    pub layer: Option<String>,
    /// Material name; for a block placement listed per object, the instance's material.
    pub material: Option<String>,
    /// Block that places the geometry.
    pub block: Option<String>,
    /// Objects or placements in the element (1 in the per-object listing).
    pub count: usize,
    /// Visible mesh area.
    pub area_m2: f64,
    /// Size of the bounding box of its visible meshes (x, y, z).
    pub size: [f64; 3],
    /// Lowest point.
    pub base_z: f64,
    /// Share of the area facing up or down (normals within 10° of the vertical axis).
    pub horizontal_share: f64,
    /// Share of the area facing sideways (normals within 10° of the horizontal plane).
    pub vertical_share: f64,
    /// Share of the area that is neither: `1 − horizontal − vertical`.
    pub sloped_share: f64,
}

/// Facts about the file as a whole.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileInfo {
    /// Size of the input in bytes.
    pub size_bytes: u64,
    /// `.3dm` archive version: 1 to 4 for Rhino 1 to 4, then 50, 60, 70, 80 from Rhino 5;
    /// `None` for IFC files.
    pub archive_version: Option<u32>,
    /// Name of the application that saved the file (`Rhinoceros 8`); for IFC files, the
    /// originating system. `None` when the file does not say.
    pub application: Option<String>,
    /// What the application wrote with its name (Rhino: licence and build date); for IFC
    /// files, the schema (`IFC2X3`, `IFC4`, …).
    pub application_details: Option<String>,
    /// Unit system name as saved in the file (`millimeters`, `meters`, `none`, ...).
    pub unit_system: String,
    /// Length of one model unit; `None` when the file has no unit system.
    pub metres_per_unit: Option<f64>,
    /// The model's absolute tolerance; `None` without a unit system or a positive tolerance.
    pub absolute_tolerance_m: Option<f64>,
    /// Where the model sits on Earth, when the file sets it (left out otherwise).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub earth_anchor: Option<EarthAnchor>,
}

/// Rhino's earth anchor point: latitude and longitude in degrees, the model's north and east
/// as unit vectors in model coordinates.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EarthAnchor {
    /// Degrees, north positive, rounded to 1e-6.
    pub latitude: f64,
    /// Degrees, east positive, rounded to 1e-6.
    pub longitude: f64,
    /// Elevation as saved, rounded to 1e-3 m.
    pub elevation_m: f64,
    /// Unit vector pointing north, rounded to 1e-6.
    pub north: [f64; 3],
    /// Unit vector pointing east, rounded to 1e-6.
    pub east: [f64; 3],
}

/// A light of the file's light table, in model coordinates (metres).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Light {
    /// Name as saved (may be empty).
    pub name: String,
    /// `directional`, `point`, `spot`, `linear`, `rectangular`, `ambient`, or `unknown`.
    pub kind: String,
    /// Fixed to the camera instead of the model (Rhino's camera lights).
    pub camera_relative: bool,
    /// Switched on, on a visible layer and not hidden.
    pub on: bool,
    /// Path of the light's layer; `None` when the layer table does not list it.
    pub layer: Option<String>,
    /// Intensity as stored in the file (`ON_Light` intensity), rounded to 1e-6.
    pub intensity: f64,
    /// Power in watts as stored in the file (0 when not finite), rounded to 1e-3.
    pub watts: f64,
    /// Diffuse colour, sRGB.
    pub color: [u8; 3],
    /// Position: a linear light's start, a rectangular light's corner.
    pub location: [f64; 3],
    /// Unit vector the light points along (zero for point and ambient lights).
    pub direction: [f64; 3],
    /// Spot lights: the cone's half angle.
    pub spot_angle_deg: Option<f64>,
    /// Spot lights: the share of the cone at full strength, 0 to 1, when the file stores it.
    pub hotspot: Option<f64>,
    /// Linear and rectangular lights: the length side, as a vector from `location`.
    pub length: Option<[f64; 3]>,
    /// Linear and rectangular lights: the width side, as a vector from `location`.
    pub width: Option<[f64; 3]>,
}

/// The document's organisation: layers, groups, object names and user strings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Structure {
    /// The layer table, in archive index order.
    pub layers: Vec<Layer>,
    /// The group table, in archive index order.
    pub groups: Vec<Group>,
    /// Distinct object names, sorted, at most [`MAX_LISTED`].
    pub object_names: Vec<String>,
    /// User-string keys of model-space objects, sorted; at most 100 (the first in sort order).
    pub user_strings: Vec<UserStringKey>,
}

/// Upper bound for listed names and sample values.
pub const MAX_LISTED: usize = 50;

/// A layer with the objects and visible area on it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Layer {
    /// Index in the file's layer table (`-1` when the file gives none).
    pub index: i64,
    /// Name as saved (empty when unnamed).
    pub name: String,
    /// Full path from the root layer, `::`-separated as in Rhino.
    pub path: String,
    /// `#rrggbb`.
    pub color: Option<String>,
    /// The layer's own visibility (a layer under a hidden parent can be `true`).
    pub visible: bool,
    /// Locked against editing.
    pub locked: bool,
    /// Name of the layer's render material.
    pub material: Option<String>,
    /// Model-space objects on the layer, hidden ones included (block members not counted).
    pub objects: usize,
    /// Visible mesh area on the layer, block members once per placement.
    pub mesh_area_m2: f64,
    /// Bounding box of the visible meshes on the layer; `None` when there are none.
    pub bbox: Option<BBox>,
}

/// A group of the file's group table.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Group {
    /// Name as saved (empty when unnamed).
    pub name: String,
    /// Model-space objects in the group (block members not counted).
    pub objects: usize,
}

/// A user-string key and the objects that carry it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UserStringKey {
    /// The key, as saved.
    pub key: String,
    /// Model-space objects with this key.
    pub objects: usize,
    /// Distinct values, sorted, at most five.
    pub values: Vec<String>,
}

/// A block definition and how often it is placed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Block {
    /// Definition name (empty when unnamed); for IFC models, the type's name.
    pub name: String,
    /// Block instances that place it, in model space or inside other definitions (each
    /// instance once: nesting does not multiply); for IFC types, the products of the type.
    pub placements: usize,
    /// Objects the definition lists (0 for IFC types).
    pub members: usize,
    /// Size of the definition's displayable geometry (nested blocks included), unplaced.
    pub size: Option<[f64; 3]>,
}

/// A material of the file's material table, with where it is used.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Material {
    /// Index in the file's material table (`-1` when the file gives none).
    pub index: i64,
    /// Name as saved (empty when unnamed).
    pub name: String,
    /// `#rrggbb`: the PBR base colour when present, else the diffuse colour.
    pub color: Option<String>,
    /// Carries physically based (PBR) parameters.
    pub physically_based: bool,
    /// PBR metallic, 0 to 1, rounded to 1e-4.
    pub metallic: Option<f64>,
    /// PBR roughness, 0 to 1, rounded to 1e-4.
    pub roughness: Option<f64>,
    /// PBR opacity, 0 to 1, rounded to 1e-4.
    pub opacity: Option<f64>,
    /// Texture file names (no directories).
    pub textures: Vec<String>,
    /// Layers that use it as their material.
    pub layers: usize,
    /// Model-space objects that use it, through their own material or their layer's.
    pub objects: usize,
    /// Visible mesh area that uses it, block members once per placement.
    pub mesh_area_m2: f64,
}

/// A saved viewport or named view, in model coordinates.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct View {
    /// Name as saved (a viewport's title, such as `Top` or `Perspective`).
    pub name: String,
    /// Viewport or named view.
    pub kind: ViewKind,
    /// Parallel or perspective.
    pub projection: Projection,
    /// Camera location.
    pub camera: [f64; 3],
    /// Unit vector.
    pub direction: [f64; 3],
    /// Unit vector.
    pub up: [f64; 3],
    /// The camera's target point, when the file stores one.
    pub target: Option<[f64; 3]>,
    /// Perspective views: the frustum's angles.
    pub fov_deg: Option<FieldOfView>,
    /// 35 mm-equivalent lens length, from the smaller frustum side mapped to 24 mm.
    pub lens_35mm: Option<f64>,
    /// View frustum in metres: left, right, bottom, top, near, far (camera space).
    pub frustum: Option<[f64; 6]>,
    /// Enabled clipping planes of the view, as `[a, b, c, d]` with `(a, b, c)` a unit normal
    /// and lengths in metres: the view shows only points where `a·x + b·y + c·z + d ≥ 0`.
    pub clipping_planes: Vec<[f64; 4]>,
}

/// Where a [`View`] comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewKind {
    /// A viewport as it was open when the file was saved.
    Viewport,
    /// A named view.
    Named,
}

/// A view's projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Projection {
    /// Parallel (orthographic) projection.
    Parallel,
    /// Perspective projection.
    Perspective,
    /// Any other or unknown projection code.
    Other,
}

/// The full angles of a perspective frustum, in degrees.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldOfView {
    /// From the frustum's width at the near plane.
    pub horizontal: f64,
    /// From the frustum's height at the near plane.
    pub vertical: f64,
}

/// Model-space objects (block definition members are counted in `block_members`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Inventory {
    /// Model-space objects of every kind, hidden ones included.
    pub objects: usize,
    /// Breps, extrusions and surfaces.
    pub breps: usize,
    /// Breps that carry a cached render mesh.
    pub breps_with_render_mesh: usize,
    /// Breps without a cached render mesh that were tessellated here (wholly or in part).
    pub breps_tessellated: usize,
    /// Mesh objects (each IFC product counts as one).
    pub meshes: usize,
    /// SubD objects.
    pub subds: usize,
    /// Curve objects.
    pub curves: usize,
    /// Point objects and point clouds.
    pub points: usize,
    /// Text, leaders, dimensions and text dots.
    pub annotations: usize,
    /// Hatches.
    pub hatches: usize,
    /// Block instances in model space (nested instances are block members).
    pub block_instances: usize,
    /// Objects that are members of a block definition, of every kind.
    pub block_members: usize,
    /// Lights of the light table (not counted in `objects`).
    pub lights: usize,
    /// Other objects, such as layout details and clipping planes.
    pub other: usize,
    /// Object records the decoder retained without decoding their geometry.
    pub undecoded: usize,
    /// Vertices of the displayable meshes (render meshes, mesh objects, meshes made here from
    /// Breps and SubDs) placed in model space: hidden ones included, block members once per
    /// placement.
    pub mesh_vertices: usize,
    /// Triangles of the same meshes (a quad counts as two).
    pub mesh_triangles: usize,
}

/// An axis-aligned bounding box, in metres.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BBox {
    /// Lowest x, y and z.
    pub min: [f64; 3],
    /// Highest x, y and z.
    pub max: [f64; 3],
}

/// Overall size and floor levels of the visible displayable meshes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proportions {
    /// Bounding box of visible displayable meshes.
    pub bbox: BBox,
    /// Size of `bbox` (x, y, z).
    pub size: [f64; 3],
    /// Longer plan side over shorter plan side.
    pub plan_aspect: Option<f64>,
    /// Height over the longer plan side.
    pub height_to_plan: Option<f64>,
    /// Storeys, when the levels are regular enough (or, for IFC models, the file lists them).
    pub storeys: Option<Storeys>,
    /// Raw floor-level candidates: clusters of upward-facing horizontal area, lowest first
    /// (geometry on layers named like site or terrain is left out).
    pub levels: Vec<Level>,
}

/// A floor-level candidate: a cluster of upward-facing horizontal mesh area.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Level {
    /// Area-weighted elevation of the cluster, rounded to 1e-4 m.
    pub z: f64,
    /// Upward-facing horizontal area in the cluster.
    pub area_m2: f64,
}

/// Floor levels found as clusters of upward-facing horizontal mesh area (heuristic): given
/// when at least three clusters with a fifth of the largest one's area or more are spaced
/// regularly, with a median spacing of 2 to 8 m. For IFC models, the file's building storeys
/// instead.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Storeys {
    /// Storeys between the levels (the top level read as a roof); for IFC models, the
    /// building storeys that give an elevation.
    pub count: usize,
    /// Median spacing between levels (for IFC models, of the spacings over 0.5 m; 0 when there
    /// are none).
    pub typical_height: f64,
    /// Elevations of the levels, lowest first.
    pub levels: Vec<f64>,
}

/// Figures about the form of the model, from its visible meshes and Brep faces.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StyleSignals {
    /// Share of mesh area aligned (within 10°) to the vertical axis or the dominant plan grid.
    pub orthogonality: Option<f64>,
    /// Share of mesh area facing up or down (normals within 10° of the vertical axis);
    /// `None` without mesh area.
    pub horizontal_area_share: Option<f64>,
    /// Share of mesh area facing sideways (normals within 10° of the horizontal plane);
    /// `None` without mesh area.
    pub vertical_area_share: Option<f64>,
    /// Share of model-space Brep faces on analytic planes (flat NURBS faces count as curved;
    /// block contents excluded).
    pub planar_face_share: Option<f64>,
    /// Share of mesh area on layers or materials named like glass or glazing.
    pub glazing_area_share: Option<f64>,
    /// How much the model repeats blocks.
    pub repetition: Repetition,
}

/// Block use across the model.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Repetition {
    /// Block definitions in the file (for IFC models: the types).
    pub block_definitions: usize,
    /// Block instances of every definition, nested ones included (for IFC models: the products
    /// of a type).
    pub block_placements: usize,
    /// Placements of the most placed definition.
    pub max_placements_per_block: usize,
}

/// Something the reader could not read, or had to assume.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Diagnostic {
    /// What kind of issue it is.
    pub code: DiagnosticCode,
    /// How much it affects the report.
    pub severity: Severity,
    /// One or two sentences in English, for people to read.
    pub message: String,
    /// The objects or records it concerns, when they are counted.
    pub count: Option<usize>,
}

/// The kind of a [`Diagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticCode {
    /// The file has no unit system; lengths assume 1 unit = 1 mm.
    UnknownUnits,
    /// A Rhino 1 file (its saved views are not read), or an archive version the decoder only
    /// partly supports.
    UnsupportedVersion,
    /// Breps whose faces without a cached render mesh could not all be tessellated (counted
    /// per Brep, listed per layer in the message).
    BrepsWithoutRenderMesh,
    /// SubD objects, shown by subdividing their control cage twice.
    SubdCageOnly,
    /// Object records retained without decoded geometry (block members included), listed by
    /// class in the message.
    UndecodedObjects,
    /// Object records the decoder rejected, skipped.
    RejectedRecords,
    /// Block placements beyond one million (nested blocks included) were left out; geometry
    /// totals are partial.
    BlockExpansionLimited,
    /// The file has objects, but no visible object has a displayable mesh.
    NoDisplayableGeometry,
    /// Records with an unexpected shape, skipped.
    UnparsedRecords,
}

/// How much a [`Diagnostic`] affects the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Severity {
    /// How the model was read or is shown.
    Info,
    /// Something is missing from the report or assumed.
    Warning,
}
