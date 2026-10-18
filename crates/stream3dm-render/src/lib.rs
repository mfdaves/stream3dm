//! Software rendering of conditioning passes from a model's saved views.
//!
//! The [`Renderer`] is a [`MeshSink`]: the streaming core hands it one visible mesh at a time
//! and it rasterizes the triangles straight into per-view buffers (depth, normal, material,
//! layer and object per pixel), so memory is the buffers only, whatever the model size.
//! [`Renderer::finish`] turns the buffers into images:
//!
//! - `clay`: grey, lit from the upper left;
//! - `depth`: inverse depth (near bright) for perspective views, linear for parallel ones;
//! - `normals`: camera-space normals, red = right, green = up, blue = toward the viewer;
//! - `materials`: each surface in its material's colour; neutral grey without a material;
//! - `layers`: each surface in its layer's colour;
//! - `lines`: white on black where the surface folds (> 35°), depth jumps (second
//!   difference) or another object starts (unless it continues the same surface flush);
//! - `preview`: clay with the lines drawn over it.
//!
//! Depth, normals, materials and lines are rendered without anti-aliasing, so every pass lines
//! up pixel for pixel.

use std::io::BufWriter;
use std::path::Path;
use stream3dm::{DocumentInfo, MeshSink, PlacedMesh, Projection, View, ViewKind};

/// Geometry closer to a perspective camera than this is clipped (metres).
const NEAR_CLIP: f64 = 0.01;
/// Surfaces folding more than this between neighbouring pixels draw a line.
const CREASE_COS: f32 = 0.819_152; // cos 35°
/// A depth second difference above this share of the depth draws a line.
const DEPTH_JUMP: f32 = 0.02;
/// Material pass colour of surfaces without a material.
const NO_MATERIAL: [u8; 3] = [190, 190, 190];
/// Layer pass colour of surfaces without a layer colour.
const NO_LAYER: [u8; 3] = [128, 128, 128];

#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub width: u32,
    pub height: u32,
    /// Render only the views with this name; otherwise named views, or viewports when the
    /// file has none.
    pub view: Option<String>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            width: 1536,
            height: 1024,
            view: None,
        }
    }
}

/// An RGB image, 8 bits per channel, rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

impl Image {
    fn new(width: u32, height: u32, fill: [u8; 3]) -> Self {
        let rgb = fill.repeat((width * height) as usize);
        Self { width, height, rgb }
    }

    fn set(&mut self, i: usize, c: [u8; 3]) {
        self.rgb[i * 3..i * 3 + 3].copy_from_slice(&c);
    }

    pub fn write_png(&self, path: &Path) -> std::io::Result<()> {
        let file = std::fs::File::create(path)?;
        let mut encoder = png::Encoder::new(BufWriter::new(file), self.width, self.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(std::io::Error::other)?;
        writer
            .write_image_data(&self.rgb)
            .map_err(std::io::Error::other)
    }
}

/// The passes of one view.
#[derive(Debug, Clone)]
pub struct Passes {
    pub view: View,
    pub clay: Image,
    pub depth: Image,
    pub normals: Image,
    pub materials: Image,
    pub layers: Image,
    pub lines: Image,
    pub preview: Image,
    /// Pixels covered by geometry.
    pub coverage: usize,
}

impl Passes {
    pub fn images(&self) -> [(&'static str, &Image); 7] {
        [
            ("clay", &self.clay),
            ("depth", &self.depth),
            ("normals", &self.normals),
            ("materials", &self.materials),
            ("layers", &self.layers),
            ("lines", &self.lines),
            ("preview", &self.preview),
        ]
    }
}

pub struct Renderer {
    options: RenderOptions,
    targets: Vec<Target>,
    material_colors: Vec<Option<[u8; 3]>>,
    layer_colors: Vec<Option<[u8; 3]>>,
    triangles: u64,
}

impl Renderer {
    pub fn new(options: RenderOptions) -> Self {
        Self {
            options,
            targets: Vec::new(),
            material_colors: Vec::new(),
            layer_colors: Vec::new(),
            triangles: 0,
        }
    }

    /// Triangles rasterized (per view).
    pub fn triangles(&self) -> u64 {
        self.triangles
    }

    pub fn finish(self) -> Vec<Passes> {
        let colors = |m: u32, l: u32| -> ([u8; 3], [u8; 3]) {
            let material = m
                .checked_sub(1)
                .and_then(|m| self.material_colors.get(m as usize).copied().flatten());
            let layer = l
                .checked_sub(1)
                .and_then(|l| self.layer_colors.get(l as usize).copied().flatten());
            (material.unwrap_or(NO_MATERIAL), layer.unwrap_or(NO_LAYER))
        };
        self.targets
            .into_iter()
            .map(|t| t.passes(&colors))
            .collect()
    }
}

impl MeshSink for Renderer {
    fn document(&mut self, document: &DocumentInfo) {
        self.material_colors = document.materials.iter().map(|m| m.color).collect();
        self.layer_colors = document.layers.iter().map(|l| l.color).collect();
        let wanted: Vec<&View> = match &self.options.view {
            Some(name) => document.views.iter().filter(|v| &v.name == name).collect(),
            None => {
                let named: Vec<&View> = document
                    .views
                    .iter()
                    .filter(|v| v.kind == ViewKind::Named)
                    .collect();
                if named.is_empty() {
                    document.views.iter().collect()
                } else {
                    named
                }
            }
        };
        let (w, h) = (self.options.width, self.options.height);
        self.targets = wanted
            .into_iter()
            .filter_map(|v| Camera::new(v, w, h).map(|c| Target::new(v.clone(), c, w, h)))
            .collect();
    }

    fn mesh(&mut self, mesh: &PlacedMesh) {
        let material = mesh.material.map_or(0, |m| m as u32 + 1);
        let layer = mesh.layer.map_or(0, |l| l as u32 + 1);
        for tri in mesh.triangles {
            let [Some(a), Some(b), Some(c)] = tri.map(|i| mesh.positions.get(i as usize).copied())
            else {
                continue;
            };
            let n = cross(sub(b, a), sub(c, a));
            let len = dot(n, n).sqrt();
            if len.is_nan() || len <= 1e-18 {
                continue;
            }
            let n = n.map(|v| v / len);
            self.triangles += 1;
            for t in &mut self.targets {
                t.draw([a, b, c], n, material, layer, mesh.object);
            }
        }
    }
}

/// A camera fitted to the output aspect ratio.
#[derive(Debug, Clone, Copy)]
struct Camera {
    origin: [f64; 3],
    right: [f64; 3],
    up: [f64; 3],
    forward: [f64; 3],
    perspective: bool,
    /// Frustum centre and half extents: tangents (perspective) or metres (parallel).
    cx: f64,
    cy: f64,
    hx: f64,
    hy: f64,
}

impl Camera {
    fn new(view: &View, width: u32, height: u32) -> Option<Self> {
        let forward = normalize(view.direction)?;
        let right = normalize(cross(forward, view.up))?;
        let up = cross(right, forward);
        let perspective = match view.projection {
            Projection::Perspective => true,
            Projection::Parallel => false,
            Projection::Other => return None,
        };
        let [l, r, b, t, n, _] = view.frustum?;
        let scale = if perspective {
            if n <= 0.0 {
                return None;
            }
            1.0 / n
        } else {
            1.0
        };
        let (cx, cy) = ((l + r) / 2.0 * scale, (b + t) / 2.0 * scale);
        let (mut hx, mut hy) = ((r - l) / 2.0 * scale, (t - b) / 2.0 * scale);
        // Keep the whole saved framing visible in the output aspect ratio.
        let aspect = f64::from(width) / f64::from(height);
        if aspect > hx / hy {
            hx = hy * aspect;
        } else {
            hy = hx / aspect;
        }
        Some(Self {
            origin: view.camera,
            right,
            up,
            forward,
            perspective,
            cx,
            cy,
            hx,
            hy,
        })
    }

    fn camera_space(&self, p: [f64; 3]) -> [f64; 3] {
        let d = sub(p, self.origin);
        [dot(d, self.right), dot(d, self.up), dot(d, self.forward)]
    }
}

/// Per-view buffers.
struct Target {
    view: View,
    camera: Camera,
    width: u32,
    height: u32,
    /// Camera-space depth; infinity where empty.
    depth: Vec<f32>,
    /// Camera-space normal facing the viewer.
    normal: Vec<[f32; 3]>,
    /// Material index + 1 (0 = none).
    material: Vec<u32>,
    /// Layer index + 1 (0 = none).
    layer: Vec<u32>,
    object: Vec<u64>,
}

impl Target {
    fn new(view: View, camera: Camera, width: u32, height: u32) -> Self {
        let n = (width * height) as usize;
        Self {
            view,
            camera,
            width,
            height,
            depth: vec![f32::INFINITY; n],
            normal: vec![[0.0; 3]; n],
            material: vec![0; n],
            layer: vec![0; n],
            object: vec![0; n],
        }
    }

    fn draw(
        &mut self,
        tri: [[f64; 3]; 3],
        normal: [f64; 3],
        material: u32,
        layer: u32,
        object: u64,
    ) {
        // The view shows only what lies on the kept side of its clipping planes.
        let mut world = tri.to_vec();
        for plane in &self.view.clipping_planes {
            world = clip_plane(&world, plane);
            if world.len() < 3 {
                return;
            }
        }
        let cam = &self.camera;
        let verts: Vec<[f64; 3]> = world.iter().map(|&p| cam.camera_space(p)).collect();
        let mut n = [
            dot(normal, cam.right),
            dot(normal, cam.up),
            dot(normal, cam.forward),
        ];
        // Two-sided: show the side facing the viewer.
        let view_dir = if cam.perspective {
            verts[0]
        } else {
            [0.0, 0.0, 1.0]
        };
        if dot(n, view_dir) > 0.0 {
            n = n.map(|v| -v);
        }
        let n = n.map(|v| v as f32);

        let polygon = if cam.perspective {
            clip_near(&verts, NEAR_CLIP)
        } else {
            verts
        };
        if polygon.len() < 3 {
            return;
        }
        let (w, h) = (f64::from(self.width), f64::from(self.height));
        // Screen x, y and the depth term interpolated linearly in screen space.
        let projected: Vec<[f64; 3]> = polygon
            .iter()
            .map(|&[x, y, z]| {
                if cam.perspective {
                    let (sx, sy) = (x / z, y / z);
                    [
                        w * (0.5 + 0.5 * (sx - cam.cx) / cam.hx),
                        h * (0.5 - 0.5 * (sy - cam.cy) / cam.hy),
                        1.0 / z,
                    ]
                } else {
                    [
                        w * (0.5 + 0.5 * (x - cam.cx) / cam.hx),
                        h * (0.5 - 0.5 * (y - cam.cy) / cam.hy),
                        z,
                    ]
                }
            })
            .collect();
        for k in 1..projected.len() - 1 {
            self.fill(
                [projected[0], projected[k], projected[k + 1]],
                n,
                material,
                layer,
                object,
            );
        }
    }

    fn fill(&mut self, p: [[f64; 3]; 3], n: [f32; 3], material: u32, layer: u32, object: u64) {
        let area = edge(p[0], p[1], p[2]);
        if area.abs() < 1e-12 || !area.is_finite() {
            return;
        }
        let (w, h) = (self.width as i64, self.height as i64);
        let min_x = p
            .iter()
            .map(|q| q[0])
            .fold(f64::INFINITY, f64::min)
            .floor()
            .max(0.0) as i64;
        let max_x = p
            .iter()
            .map(|q| q[0])
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil()
            .min(w as f64) as i64;
        let min_y = p
            .iter()
            .map(|q| q[1])
            .fold(f64::INFINITY, f64::min)
            .floor()
            .max(0.0) as i64;
        let max_y = p
            .iter()
            .map(|q| q[1])
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil()
            .min(h as f64) as i64;
        for y in min_y..max_y {
            for x in min_x..max_x {
                let c = [x as f64 + 0.5, y as f64 + 0.5, 0.0];
                let w0 = edge(p[1], p[2], c) / area;
                let w1 = edge(p[2], p[0], c) / area;
                let w2 = edge(p[0], p[1], c) / area;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let term = w0 * p[0][2] + w1 * p[1][2] + w2 * p[2][2];
                let z = if self.camera.perspective {
                    1.0 / term
                } else {
                    term
                } as f32;
                let i = (y * w + x) as usize;
                if z.is_finite() && z < self.depth[i] {
                    self.depth[i] = z;
                    self.normal[i] = n;
                    self.material[i] = material;
                    self.layer[i] = layer;
                    self.object[i] = object;
                }
            }
        }
    }

    fn passes(self, colors: &impl Fn(u32, u32) -> ([u8; 3], [u8; 3])) -> Passes {
        let (w, h) = (self.width as usize, self.height as usize);
        let covered = |i: usize| self.depth[i].is_finite();
        let mut clay = Image::new(self.width, self.height, [245, 245, 245]);
        let mut depth = Image::new(self.width, self.height, [0, 0, 0]);
        let mut normals = Image::new(self.width, self.height, [0, 0, 0]);
        let mut materials = Image::new(self.width, self.height, [0, 0, 0]);
        let mut layers = Image::new(self.width, self.height, [0, 0, 0]);
        let mut lines = Image::new(self.width, self.height, [0, 0, 0]);

        let (mut near, mut far) = (f32::INFINITY, 0.0f32);
        for &z in self.depth.iter().filter(|z| z.is_finite()) {
            near = near.min(z);
            far = far.max(z);
        }
        let light = {
            let l = [-0.45f32, 0.65, -0.6];
            let len = (l[0] * l[0] + l[1] * l[1] + l[2] * l[2]).sqrt();
            l.map(|v| v / len)
        };
        let mut coverage = 0;
        for i in 0..w * h {
            if !covered(i) {
                continue;
            }
            coverage += 1;
            let n = self.normal[i];
            let lambert = (n[0] * light[0] + n[1] * light[1] + n[2] * light[2]).max(0.0);
            let grey = (0.92 * (0.3 + 0.7 * lambert) * 255.0).round() as u8;
            clay.set(i, [grey; 3]);
            let z = self.depth[i];
            let v = if far <= near {
                1.0
            } else if self.camera.perspective {
                (1.0 / z - 1.0 / far) / (1.0 / near - 1.0 / far)
            } else {
                (far - z) / (far - near)
            };
            depth.set(i, [(v.clamp(0.0, 1.0) * 255.0).round() as u8; 3]);
            let enc = |c: f32| ((0.5 + 0.5 * c).clamp(0.0, 1.0) * 255.0).round() as u8;
            normals.set(i, [enc(n[0]), enc(n[1]), enc(-n[2])]);
            let (material, layer) = colors(self.material[i], self.layer[i]);
            materials.set(i, material);
            layers.set(i, layer);
        }

        // Lines between neighbouring pixels.
        let line = |a: usize, b: usize, before: Option<usize>| -> bool {
            match (covered(a), covered(b)) {
                (false, false) => false,
                (true, false) | (false, true) => true,
                (true, true) => {
                    let (na, nb) = (self.normal[a], self.normal[b]);
                    let facing = na[0] * nb[0] + na[1] * nb[1] + na[2] * nb[2];
                    if facing < CREASE_COS {
                        return true;
                    }
                    // Another object draws a line unless it continues the same surface (flush
                    // pieces of one wall): same material, same direction, same depth.
                    if self.object[a] != self.object[b] {
                        let (za, zb) = (self.depth[a], self.depth[b]);
                        let flush = self.material[a] == self.material[b]
                            && self.layer[a] == self.layer[b]
                            && facing > 0.9995
                            && (za - zb).abs() <= 0.002 * za.min(zb);
                        if !flush {
                            return true;
                        }
                    }
                    match before.filter(|&o| covered(o)) {
                        Some(o) => {
                            let d2 = (self.depth[b] - 2.0 * self.depth[a] + self.depth[o]).abs();
                            d2 > DEPTH_JUMP * self.depth[a]
                        }
                        None => false,
                    }
                }
            }
        };
        let mut is_line = vec![false; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if x + 1 < w && line(i, i + 1, (x > 0).then(|| i - 1)) {
                    let mark = if covered(i) { i } else { i + 1 };
                    is_line[mark] = true;
                }
                if y + 1 < h && line(i, i + w, (y > 0).then(|| i - w)) {
                    let mark = if covered(i) { i } else { i + w };
                    is_line[mark] = true;
                }
            }
        }
        let mut preview = clay.clone();
        for (i, &on) in is_line.iter().enumerate() {
            if on {
                lines.set(i, [255; 3]);
                preview.set(i, [55, 55, 55]);
            }
        }
        Passes {
            view: self.view,
            clay,
            depth,
            normals,
            materials,
            layers,
            lines,
            preview,
            coverage,
        }
    }
}

/// Clips a polygon to `z >= near` (Sutherland–Hodgman against one plane).
fn clip_near(verts: &[[f64; 3]], near: f64) -> Vec<[f64; 3]> {
    let mut out = Vec::with_capacity(4);
    for i in 0..verts.len() {
        let (a, b) = (verts[i], verts[(i + 1) % verts.len()]);
        let (ina, inb) = (a[2] >= near, b[2] >= near);
        if ina {
            out.push(a);
        }
        if ina != inb {
            let t = (near - a[2]) / (b[2] - a[2]);
            out.push([a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1]), near]);
        }
    }
    out
}

/// The part of a convex polygon where `a·x + b·y + c·z + d ≥ 0`.
fn clip_plane(verts: &[[f64; 3]], &[a, b, c, d]: &[f64; 4]) -> Vec<[f64; 3]> {
    let side = |p: [f64; 3]| a * p[0] + b * p[1] + c * p[2] + d;
    let mut out = Vec::with_capacity(verts.len() + 1);
    for i in 0..verts.len() {
        let (p, q) = (verts[i], verts[(i + 1) % verts.len()]);
        let (sp, sq) = (side(p), side(q));
        if sp >= 0.0 {
            out.push(p);
        }
        if (sp >= 0.0) != (sq >= 0.0) {
            let t = sp / (sp - sq);
            out.push([0, 1, 2].map(|k| p[k] + t * (q[k] - p[k])));
        }
    }
    out
}

fn edge(a: [f64; 3], b: [f64; 3], c: [f64; 3]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(v: [f64; 3]) -> Option<[f64; 3]> {
    let len = dot(v, v).sqrt();
    (len > 0.0 && len.is_finite()).then(|| v.map(|c| c / len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stream3dm::{FieldOfView, LayerInfo, MaterialInfo};

    fn view(projection: Projection, frustum: [f64; 6]) -> View {
        View {
            name: "test".into(),
            kind: ViewKind::Named,
            projection,
            camera: [0.0, -10.0, 0.5],
            direction: [0.0, 1.0, 0.0],
            up: [0.0, 0.0, 1.0],
            target: Some([0.0, 0.0, 0.5]),
            fov_deg: Some(FieldOfView {
                horizontal: 0.0,
                vertical: 0.0,
            }),
            lens_35mm: None,
            frustum: Some(frustum),
            clipping_planes: Vec::new(),
        }
    }

    /// A unit cube in front of the camera covers the image centre; its silhouette and the
    /// edge between its visible faces draw lines.
    #[test]
    fn renders_a_cube() {
        let mut r = Renderer::new(RenderOptions {
            width: 64,
            height: 64,
            view: None,
        });
        r.document(&DocumentInfo {
            views: vec![view(
                Projection::Perspective,
                [-0.1, 0.1, -0.1, 0.1, 1.0, 100.0],
            )],
            layers: vec![LayerInfo {
                path: "L".into(),
                color: Some([10, 20, 30]),
            }],
            materials: vec![MaterialInfo {
                name: "M".into(),
                color: Some([200, 100, 50]),
            }],
        });
        let p = |x: f64, y: f64, z: f64| [x - 0.5, y - 0.5, z];
        let positions = [
            p(0., 0., 0.),
            p(1., 0., 0.),
            p(0., 1., 0.),
            p(1., 1., 0.),
            p(0., 0., 1.),
            p(1., 0., 1.),
            p(0., 1., 1.),
            p(1., 1., 1.),
        ];
        let quads = [
            [0, 2, 3, 1],
            [4, 5, 7, 6],
            [0, 1, 5, 4],
            [2, 6, 7, 3],
            [0, 4, 6, 2],
            [1, 3, 7, 5],
        ];
        let triangles: Vec<[u32; 3]> = quads
            .iter()
            .flat_map(|q| [[q[0], q[1], q[2]], [q[0], q[2], q[3]]])
            .collect();
        r.mesh(&PlacedMesh {
            positions: &positions,
            triangles: &triangles,
            object: 7,
            pick: 7,
            layer: Some(0),
            material: Some(0),
            color: None,
        });
        let passes = r.finish();
        assert_eq!(passes.len(), 1);
        let pass = &passes[0];
        let centre = (32 * 64 + 32) * 3;
        assert_eq!(&pass.materials.rgb[centre..centre + 3], &[200, 100, 50]);
        // The front face looks straight at the camera: normal blue, near-white depth.
        assert_eq!(&pass.normals.rgb[centre..centre + 3], &[128, 128, 255]);
        assert_eq!(pass.depth.rgb[centre], 255);
        assert!(pass.coverage > 0 && pass.coverage < 64 * 64);
        assert!(pass.lines.rgb.contains(&255));
        assert_eq!(&pass.lines.rgb[centre..centre + 3], &[0, 0, 0]);
    }

    /// A view clipping plane through the cube's middle (x ≥ 0 kept) empties the left half of
    /// the image and leaves the right half drawn.
    #[test]
    fn view_clipping_planes_cut_geometry() {
        let mut r = Renderer::new(RenderOptions {
            width: 64,
            height: 64,
            view: None,
        });
        let mut v = view(Projection::Parallel, [-1.0, 1.0, -1.0, 1.0, 0.1, 100.0]);
        v.clipping_planes = vec![[1.0, 0.0, 0.0, 0.0]];
        r.document(&DocumentInfo {
            views: vec![v],
            layers: Vec::new(),
            materials: Vec::new(),
        });
        // A square facing the camera, spanning x from -0.8 to 0.8.
        let positions = [
            [-0.8, 0.0, -0.3],
            [0.8, 0.0, -0.3],
            [0.8, 0.0, 1.3],
            [-0.8, 0.0, 1.3],
        ];
        r.mesh(&PlacedMesh {
            positions: &positions,
            triangles: &[[0, 1, 2], [0, 2, 3]],
            object: 1,
            pick: 1,
            layer: None,
            material: None,
            color: None,
        });
        let pass = &r.finish()[0];
        let at = |x: usize, y: usize| pass.depth.rgb[(y * 64 + x) * 3];
        assert_eq!(at(16, 32), 0, "left half clipped away");
        assert!(at(48, 32) > 0, "right half kept");
    }

    #[test]
    fn clipping_keeps_the_visible_part() {
        let clipped = clip_near(&[[0.0, 0.0, -1.0], [1.0, 0.0, 1.0], [0.0, 1.0, 1.0]], 0.5);
        assert_eq!(clipped.len(), 4);
        assert!(clipped.iter().all(|v| v[2] >= 0.5 - 1e-12));
    }
}
