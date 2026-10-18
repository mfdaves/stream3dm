// Generates synthetic Rhino models with known content, plus the values a correct extraction
// must report (computed here from the generator's own geometry, independently of stream3dm).
//
//   node generate.mjs house <dir>                         house.3dm, house_v7.3dm, house.expected.json
//   node generate.mjs neighbourhood <dir> --target-mb N   neighbourhood_<N>mb.3dm + .expected.json
//   node generate.mjs anchor <dir>                        anchor.3dm: a box with an earth anchor
//                                                         (north turned 30°) and three lights
//
// Geometry is in metres. Display geometry is written as meshes (rhino3dm cannot mesh Breps);
// one Brep without a render mesh is included on purpose.

import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const [command, outDir, ...rest] = process.argv.slice(2);
if (!['house', 'neighbourhood', 'anchor', 'lit_room'].includes(command) || !outDir) {
  console.error('usage: node generate.mjs house <dir> | neighbourhood <dir> --target-mb N [--seed S] | anchor <dir> | lit_room <dir>');
  process.exit(2);
}
const option = (name, fallback) => {
  const i = rest.indexOf(name);
  return i >= 0 ? Number(rest[i + 1]) : fallback;
};

// The 64-bit build holds files beyond the ~500 MB the default build can write.
const { default: rhino3dm } = command === 'neighbourhood'
  ? await import('rhino3dm/wasm64/rhino3dm.module.js')
  : await import('rhino3dm');
const rhino = await rhino3dm();
mkdirSync(outDir, { recursive: true });

// ── Matrices (row-major 4×4) ──────────────────────────────────────────────────

const identity = () => [[1, 0, 0, 0], [0, 1, 0, 0], [0, 0, 1, 0], [0, 0, 0, 1]];
const multiply = (a, b) => a.map((row, r) => b[0].map((_, c) => row.reduce((s, _, k) => s + a[r][k] * b[k][c], 0)));
const translate = (x, y, z) => [[1, 0, 0, x], [0, 1, 0, y], [0, 0, 1, z], [0, 0, 0, 1]];
const rotateZ = (deg) => {
  const t = (deg * Math.PI) / 180, c = Math.cos(t), s = Math.sin(t);
  return [[c, -s, 0, 0], [s, c, 0, 0], [0, 0, 1, 0], [0, 0, 0, 1]];
};
const mirrorX = () => [[-1, 0, 0, 0], [0, 1, 0, 0], [0, 0, 1, 0], [0, 0, 0, 1]];
const apply = (m, [x, y, z]) => [0, 1, 2].map((r) => m[r][0] * x + m[r][1] * y + m[r][2] * z + m[r][3]);
const det3 = (m) =>
  m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) -
  m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0]) +
  m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);

const xform = (m) => {
  const t = new rhino.Transform(1);
  for (let r = 0; r < 4; r++) for (let c = 0; c < 4; c++) t[`m${r}${c}`] = m[r][c];
  return t;
};

// ── Mesh descriptions (kept for expected values) ──────────────────────────────

/** Axis-aligned box: 8 vertices, 6 outward quads. */
function box([x0, y0, z0], [x1, y1, z1]) {
  const p = (i, j, k) => [[x0, x1][i], [y0, y1][j], [z0, z1][k]];
  const positions = [p(0, 0, 0), p(1, 0, 0), p(0, 1, 0), p(1, 1, 0), p(0, 0, 1), p(1, 0, 1), p(0, 1, 1), p(1, 1, 1)];
  const quads = [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]];
  return { positions, quads, tris: [] };
}

/** The same quads, each with its own four vertices (as one mesh per Brep face). */
function separateFaces(m) {
  const positions = [], quads = [];
  for (const q of m.quads) {
    quads.push([0, 1, 2, 3].map((k) => positions.length + k));
    positions.push(...q.map((i) => m.positions[i]));
  }
  return { positions, quads, tris: [] };
}

/** Rectangular grid in the XY plane at height z (n × n cells). */
function grid([x0, y0], [x1, y1], z, n) {
  const positions = [];
  for (let j = 0; j <= n; j++) for (let i = 0; i <= n; i++) positions.push([x0 + ((x1 - x0) * i) / n, y0 + ((y1 - y0) * j) / n, z]);
  const quads = [];
  for (let j = 0; j < n; j++) for (let i = 0; i < n; i++) {
    const a = j * (n + 1) + i;
    quads.push([a, a + 1, a + n + 2, a + n + 1]);
  }
  return { positions, quads, tris: [] };
}

function toRhinoMesh(m) {
  const mesh = new rhino.Mesh();
  const v = mesh.vertices(), f = mesh.faces();
  for (const [x, y, z] of m.positions) v.addPoint3d(x, y, z);
  for (const q of m.quads) f.addQuadFace(q[0], q[1], q[2], q[3]);
  for (const t of m.tris) f.addTriFace(t[0], t[1], t[2]);
  mesh.normals().computeNormals();
  v.delete(); f.delete();
  return mesh;
}

const triangles = (m) => [...m.quads.flatMap((q) => [[q[0], q[1], q[2]], [q[0], q[2], q[3]]]), ...m.tris];

// ── Expected values ───────────────────────────────────────────────────────────

const COS10 = Math.cos((10 * Math.PI) / 180);
const SIN10 = Math.sin((10 * Math.PI) / 180);

/** Accumulates what the report must say about displayable geometry. */
class Expect {
  constructor() {
    this.total = 0; this.horizontal = 0; this.vertical = 0; this.glazing = 0;
    this.materialArea = {}; this.layerArea = {};
    this.min = [Infinity, Infinity, Infinity]; this.max = [-Infinity, -Infinity, -Infinity];
    this.vertices = 0; this.triangles = 0;
  }
  /** A mesh placed by `m` (world matrix), with resolved attributes. */
  add(mesh, m, { visible, layer, material, glazing }) {
    this.vertices += mesh.positions.length;
    this.triangles += triangles(mesh).length;
    if (!visible) return;
    const pts = mesh.positions.map((p) => apply(m, p));
    for (const p of pts) for (let i = 0; i < 3; i++) { this.min[i] = Math.min(this.min[i], p[i]); this.max[i] = Math.max(this.max[i], p[i]); }
    const flip = det3(m) < 0;
    let area = 0;
    for (let [a, b, c] of triangles(mesh)) {
      if (flip) [b, c] = [c, b];
      const [A, B, C] = [pts[a], pts[b], pts[c]];
      const u = [B[0] - A[0], B[1] - A[1], B[2] - A[2]], w = [C[0] - A[0], C[1] - A[1], C[2] - A[2]];
      const n = [u[1] * w[2] - u[2] * w[1], u[2] * w[0] - u[0] * w[2], u[0] * w[1] - u[1] * w[0]];
      const len = Math.hypot(...n);
      if (len <= 1e-16) continue;
      const t = len / 2, nz = n[2] / len;
      area += t;
      if (Math.abs(nz) >= COS10) this.horizontal += t;
      else if (Math.abs(nz) <= SIN10) this.vertical += t;
    }
    this.total += area;
    if (glazing) this.glazing += area;
    if (material) this.materialArea[material] = (this.materialArea[material] ?? 0) + area;
    this.layerArea[layer] = (this.layerArea[layer] ?? 0) + area;
  }
}

const round = (v, d) => Math.round(v * 10 ** d) / 10 ** d;

// ── Model builder ─────────────────────────────────────────────────────────────

/** Wraps a File3dm with name-based layers, materials and blocks, recording expectations. */
class Model {
  constructor() {
    this.file = new rhino.File3dm();
    const s = this.file.settings();
    s.modelUnitSystem = rhino.UnitSystem.Meters;
    s.modelAbsoluteTolerance = 0.001;
    this.layers = {}; this.layerInfo = {}; this.materials = {}; this.blocks = {}; this.groups = {};
    this.expect = new Expect();
    this.counts = { objects: 0, meshes: 0, breps: 0, block_instances: 0, block_members: 0 };
    this.layerObjects = {};
    this.userStrings = {}; this.names = new Set();
    this.placements = {};
  }

  material(name, [r, g, b], pbr) {
    const m = new rhino.Material();
    m.name = name;
    m.diffuseColor = { r, g, b, a: 255 };
    if (pbr) {
      m.toPhysicallyBased();
      const p = m.physicallyBased();
      p.baseColor = { r: r / 255, g: g / 255, b: b / 255, a: 1 };
      p.metallic = pbr.metallic; p.roughness = pbr.roughness;
    }
    this.materials[name] = { index: this.file.materials().add(m), color: [r, g, b] };
    m.delete();
  }

  layer(path, [r, g, b], { material, visible = true } = {}) {
    const parts = path.split('::');
    const parent = parts.length > 1 ? this.layers[parts.slice(0, -1).join('::')] : undefined;
    const l = new rhino.Layer();
    l.name = parts[parts.length - 1];
    l.color = { r, g, b, a: 255 };
    l.visible = visible;
    if (parent !== undefined) l.parentLayerId = this.file.layers().findIndex(parent).id;
    if (material) l.renderMaterialIndex = this.materials[material].index;
    const index = this.file.layers().add(l);
    l.delete();
    this.layers[path] = index;
    const parentVisible = parent === undefined || this.layerInfo[parts.slice(0, -1).join('::')].visible;
    this.layerInfo[path] = { material, visible: visible && parentVisible, color: [r, g, b] };
    this.layerObjects[path] ??= 0;
  }

  group(name) {
    const g = new rhino.Group();
    g.name = name;
    this.file.groups().add(g);
    g.delete();
    this.groups[name] = { index: this.file.groups().count - 1, objects: 0 };
  }

  attributes(layer, { material, byParent, name, strings, group } = {}) {
    const a = new rhino.ObjectAttributes();
    a.layerIndex = this.layers[layer];
    if (byParent) a.materialSource = rhino.ObjectMaterialSource.MaterialFromParent;
    else if (material) {
      a.materialSource = rhino.ObjectMaterialSource.MaterialFromObject;
      a.materialIndex = this.materials[material].index;
    }
    if (name) a.name = name;
    for (const [k, v] of Object.entries(strings ?? {})) a.setUserString(k, v);
    if (group) a.addToGroup(this.groups[group].index);
    return a;
  }

  /** Resolved material of an object (null = none). */
  resolve(layer, { material, byParent }, parentMaterial) {
    if (byParent) return parentMaterial ?? null;
    return material ?? this.layerInfo[layer].material ?? null;
  }

  isGlazing(layer, material) {
    const words = ['glass', 'glazing', 'glazed', 'window', 'vetr', 'finestr', 'verre', 'vidrio', 'ventana', 'glas', 'fenster'];
    const hit = (s) => words.some((w) => s.toLowerCase().includes(w));
    return material ? hit(material) : hit(layer);
  }

  /** Records model-space object facts (not for block members). */
  record(layer, opts, kind) {
    this.counts.objects++;
    this.counts[kind]++;
    this.layerObjects[layer]++;
    if (opts.name) this.names.add(opts.name);
    for (const [k, v] of Object.entries(opts.strings ?? {})) {
      this.userStrings[k] ??= { objects: 0, values: new Set() };
      this.userStrings[k].objects++;
      this.userStrings[k].values.add(v);
    }
    if (opts.group) this.groups[opts.group].objects++;
  }

  mesh(layer, mesh, opts = {}) {
    const a = this.attributes(layer, opts);
    const rm = toRhinoMesh(mesh);
    this.file.objects().addMesh(rm, a);
    rm.delete(); a.delete();
    this.record(layer, opts, 'meshes');
    const material = this.resolve(layer, opts);
    this.expect.add(mesh, identity(), {
      visible: this.layerInfo[layer].visible, layer, material, glazing: this.isGlazing(layer, material),
    });
  }

  brep(layer, min, max, opts = {}) {
    const a = this.attributes(layer, opts);
    const brep = rhino.Brep.createFromBoundingBox(new rhino.BoundingBox(min, max));
    this.file.objects().addBrep(brep, a);
    brep.delete(); a.delete();
    this.record(layer, opts, 'breps');
    // Written without a render mesh; the reader tessellates its six flat faces, one mesh each.
    const material = this.resolve(layer, opts);
    this.expect.add(separateFaces(box(min, max)), identity(), {
      visible: this.layerInfo[layer].visible, layer, material, glazing: this.isGlazing(layer, material),
    });
  }

  /** A block of member meshes (`{ mesh, layer, ...opts }`) or nested blocks (`{ block, matrix }`). */
  block(name, members) {
    const geometry = [], attrs = [], kept = [];
    for (const member of members) {
      if (member.block) {
        const iref = new rhino.InstanceReference(this.blocks[member.block].id, xform(member.matrix));
        geometry.push(iref);
        attrs.push(this.attributes(member.layer ?? 'Blocchi', { byParent: true }));
      } else {
        geometry.push(toRhinoMesh(member.mesh));
        attrs.push(this.attributes(member.layer ?? 'Blocchi', member));
      }
      kept.push(member);
    }
    const index = this.file.instanceDefinitions().add(name, '', '', '', [0, 0, 0], geometry, attrs);
    if (index < 0) throw new Error(`block ${name} was not created`);
    geometry.forEach((g) => g.delete()); attrs.forEach((a) => a.delete());
    this.blocks[name] = { id: this.file.instanceDefinitions().get(index).id, members: kept };
    this.counts.block_members += kept.length;
    this.placements[name] ??= 0;
  }

  /** Places a block; returns nothing. Nested blocks are expanded for the expectations. */
  place(name, matrix, layer, opts = {}) {
    const a = this.attributes(layer, opts);
    const iref = new rhino.InstanceReference(this.blocks[name].id, xform(matrix));
    this.file.objects().addInstanceObject(iref, a);
    iref.delete(); a.delete();
    this.record(layer, opts, 'block_instances');
    const visible = this.layerInfo[layer].visible;
    this.expand(name, matrix, visible, this.resolve(layer, opts));
  }

  expand(name, matrix, visible, material) {
    this.placements[name]++;
    for (const member of this.blocks[name].members) {
      if (member.block) {
        this.expand(member.block, multiply(matrix, member.matrix), visible, material);
      } else {
        const resolved = this.resolve(member.layer ?? 'Blocchi', member, material);
        this.expect.add(member.mesh, matrix, {
          visible: visible && this.layerInfo[member.layer ?? 'Blocchi'].visible,
          layer: member.layer ?? 'Blocchi', material: resolved, glazing: this.isGlazing(member.layer ?? 'Blocchi', resolved),
        });
      }
    }
  }

  namedView(name, { camera, target, lens, parallel }) {
    const vp = rhino.ViewportInfo.defaultPerspective();
    vp.screenPort = [0, 0, 1600, 900];
    vp.setFrustum(-1.6, 1.6, -0.9, 0.9, 0.1, 1000);
    if (parallel) vp.isParallelProjection = true;
    else vp.isPerspectiveProjection = true;
    if (lens) vp.camera35mmLensLength = lens;
    const dir = [0, 1, 2].map((i) => target[i] - camera[i]);
    const up = parallel ? [0, 1, 0] : [0, 0, 1];
    vp.setCameraLocation(camera); vp.setCameraDirection(dir); vp.setCameraUp(up);
    vp.targetPoint = target;
    const vi = new rhino.ViewInfo();
    vi.name = name; vi.setViewport(vp);
    this.file.namedViews().add(vi);
    vi.delete(); vp.delete();
    const len = Math.hypot(...dir);
    return { name, projection: parallel ? 'parallel' : 'perspective', lens: parallel ? null : lens, camera, target, direction: dir.map((d) => d / len) };
  }

  bytes(version) {
    if (!version) return this.file.toByteArray();
    const o = new rhino.File3dmWriteOptions();
    o.version = version;
    return this.file.toByteArrayOptions(o);
  }
}

// ── The house ─────────────────────────────────────────────────────────────────

const STOREY = 3.0, SLAB = 0.3, WALL = 0.3, W = 12, D = 9;

function defineMaterialsAndLayers(m) {
  m.material('Mattone', [160, 82, 45]);
  m.material('Calcestruzzo', [189, 189, 189]);
  m.material('Vetro', [168, 216, 234]);
  m.material('Legno', [139, 90, 43]);
  m.material('Coppi', [181, 83, 60], { metallic: 0, roughness: 0.8 });
  m.material('Prato', [76, 175, 80]);
  m.material('Fogliame', [46, 125, 50]);
  m.material('Tessuto', [109, 104, 117]);

  m.layer('00_Sito', [120, 120, 120]);
  m.layer('00_Sito::Terreno', [76, 175, 80], { material: 'Prato' });
  m.layer('00_Sito::Alberi', [46, 125, 50]);
  m.layer('00_Sito::Recinzione', [139, 90, 43], { material: 'Legno' });
  for (const floor of ['01_Piano terra', '02_Primo piano']) {
    m.layer(floor, [200, 160, 120]);
    m.layer(`${floor}::Muri`, [160, 82, 45], { material: 'Mattone' });
    m.layer(`${floor}::Solaio`, [189, 189, 189], { material: 'Calcestruzzo' });
    m.layer(`${floor}::Finestre`, [0, 160, 255]);
    m.layer(`${floor}::Arredi`, [109, 104, 117]);
  }
  m.layer('01_Piano terra::Porte', [139, 90, 43]);
  m.layer('03_Copertura', [181, 83, 60]);
  m.layer('03_Copertura::Solaio sottotetto', [189, 189, 189], { material: 'Calcestruzzo' });
  m.layer('03_Copertura::Coppi', [181, 83, 60], { material: 'Coppi' });
  m.layer('Scala', [189, 189, 189], { material: 'Calcestruzzo' });
  m.layer('Studio', [255, 0, 255]);
  m.layer('Studio::Volume', [255, 0, 255]);
  m.layer('Note nascoste', [255, 255, 0], { visible: false });
  m.layer('Blocchi', [0, 0, 0]);
  m.group('Facciata sud');
}

function defineBlocks(m) {
  // Window 1.2 × 1.5 m, 0.15 m deep: frame (Legno) around a glass pane (Vetro).
  const f = 0.06;
  m.block('Finestra 120x150', [
    { mesh: box([0, 0, 0], [1.2, 0.15, f]), material: 'Legno' },
    { mesh: box([0, 0, 1.5 - f], [1.2, 0.15, 1.5]), material: 'Legno' },
    { mesh: box([0, 0, f], [f, 0.15, 1.5 - f]), material: 'Legno' },
    { mesh: box([1.2 - f, 0, f], [1.2, 0.15, 1.5 - f]), material: 'Legno' },
    { mesh: box([f, 0.07, f], [1.2 - f, 0.08, 1.5 - f]), material: 'Vetro' },
  ]);
  m.block('Porta 90x210', [
    { mesh: box([0, 0, 0], [0.9, 0.12, 2.1]), material: 'Legno' },
  ]);
  m.block('Albero', [
    { mesh: box([-0.15, -0.15, 0], [0.15, 0.15, 2.5]), material: 'Legno' },
    { mesh: box([-1.2, -1.2, 2.5], [1.2, 1.2, 5]), material: 'Fogliame' },
  ]);
  m.block('Divano', [
    { mesh: box([0, 0, 0], [2.0, 0.9, 0.45]), byParent: true },
    { mesh: box([0, 0.7, 0.45], [2.0, 0.9, 0.85]), byParent: true },
  ]);
}

/** Facade with openings: piers between openings, sills below and lintels above them. */
function facade(m, layer, storeyZ, length, openings, place, opts) {
  // openings: [{ x, width, sill, height }] along the facade's local x.
  let x = 0;
  const parts = [];
  for (const o of [...openings].sort((a, b) => a.x - b.x)) {
    if (o.x > x) parts.push([[x, 0, 0], [o.x, WALL, STOREY]]);
    if (o.sill > 0) parts.push([[o.x, 0, 0], [o.x + o.width, WALL, o.sill]]);
    parts.push([[o.x, 0, o.sill + o.height], [o.x + o.width, WALL, STOREY]]);
    x = o.x + o.width;
  }
  if (x < length) parts.push([[x, 0, 0], [length, WALL, STOREY]]);
  for (const [a, b] of parts) {
    const mesh = box(a, b);
    mesh.positions = mesh.positions.map((p) => apply(place, [p[0], p[1], p[2] + storeyZ]));
    m.mesh(layer, mesh, opts);
  }
}

function buildHouse(m, origin = identity(), { storeys = 2, width = W, depth = D, prefix = '' } = {}) {
  const at = (mesh) => ({ ...mesh, positions: mesh.positions.map((p) => apply(origin, p)) });
  const floors = ['01_Piano terra', '02_Primo piano'];
  const wallStrings = { Fase: 'Progetto', Classe: 'REI 60' };
  const windowStrings = { Produttore: 'Serramenti Rossi' };
  for (let s = 0; s < storeys; s++) {
    const floor = floors[Math.min(s, 1)];
    const z = s * STOREY;
    // Slab inside the walls, top at the storey level.
    m.mesh(`${floor}::Solaio`, at(box([WALL, WALL, z - SLAB], [width - WALL, depth - WALL, z])), { name: `${prefix}Solaio ${s}` });
    // Facades: south (y = 0) has windows and, on the ground floor, the door.
    const windows = [{ x: 1.5, width: 1.2, sill: 0.9, height: 1.5 }, { x: width - 2.7, width: 1.2, sill: 0.9, height: 1.5 }];
    const south = s === 0 ? [...windows, { x: width / 2 - 0.45, width: 0.9, sill: 0, height: 2.1 }] : [...windows, { x: width / 2 - 0.6, width: 1.2, sill: 0.9, height: 1.5 }];
    const north = [{ x: width / 2 - 0.6, width: 1.2, sill: 0.9, height: 1.5 }];
    const faces = [
      { name: 'Muro sud', len: width, open: south, place: origin },
      { name: 'Muro nord', len: width, open: north, place: multiply(origin, multiply(translate(width, depth, 0), rotateZ(180))) },
      { name: 'Muro ovest', len: depth, open: [], place: multiply(origin, multiply(translate(0, depth, 0), rotateZ(-90))) },
      { name: 'Muro est', len: depth, open: [], place: multiply(origin, multiply(translate(width, 0, 0), rotateZ(90))) },
    ];
    for (const f of faces) {
      facade(m, `${floor}::Muri`, z, f.len, f.open, f.place, { name: `${prefix}${f.name}`, strings: wallStrings });
      for (const o of f.open) {
        const local = translate(o.x, 0.075, z + o.sill);
        if (o.sill === 0) {
          m.place('Porta 90x210', multiply(f.place, local), '01_Piano terra::Porte');
        } else {
          const south = f.name === 'Muro sud';
          m.place('Finestra 120x150', multiply(f.place, local), `${floor}::Finestre`, { strings: windowStrings, group: south && !prefix ? 'Facciata sud' : undefined });
        }
      }
    }
    m.place('Divano', multiply(origin, translate(2, depth - 2, z)), `${floor}::Arredi`, { material: 'Tessuto' });
  }
  // A window placed mirrored (east gable, top storey), and a double window from nested blocks.
  const topZ = (storeys - 1) * STOREY;
  m.place('Finestra 120x150', multiply(origin, multiply(translate(width + 1.2, 3, topZ + 0.9), mirrorX())), `${floors[Math.min(storeys - 1, 1)]}::Finestre`);
  if (m.blocks['Finestra doppia']) m.place('Finestra doppia', multiply(origin, translate(width - 0.1, 6, topZ + 0.9)), `${floors[Math.min(storeys - 1, 1)]}::Finestre`);
  // Attic slab and gable roof (ridge along x).
  const eaves = storeys * STOREY, ridge = eaves + 2.5;
  m.mesh('03_Copertura::Solaio sottotetto', at(box([WALL, WALL, eaves - SLAB], [width - WALL, depth - WALL, eaves])), { name: `${prefix}Solaio sottotetto` });
  const roof = {
    positions: [[0, 0, eaves], [width, 0, eaves], [width, depth / 2, ridge], [0, depth / 2, ridge], [width, depth, eaves], [0, depth, eaves]],
    quads: [[0, 1, 2, 3], [4, 5, 3, 2]],
    tris: [[5, 0, 3], [1, 4, 2]],
  };
  m.mesh('03_Copertura::Coppi', at(roof), { name: `${prefix}Tetto` });
  // Stairs: 17 steps from the ground floor to the first.
  if (storeys > 1) for (let i = 0; i < 17; i++) {
    m.mesh('Scala', at(box([width - 2.3 - i * 0.27, WALL + 0.2, 0], [width - 2.3 - (i - 1) * 0.27, WALL + 1.2, (i + 1) * (STOREY / 17)])));
  }
}

function buildSite(m, origin, width, depth) {
  const at = (mesh) => ({ ...mesh, positions: mesh.positions.map((p) => apply(origin, p)) });
  m.mesh('00_Sito::Terreno', at(grid([-8, -8], [width + 8, depth + 8], -0.02, 8)), { name: 'Terreno' });
  for (const [x, y] of [[-4, -4], [-4, depth + 4], [width + 4, -4], [width + 4, depth + 4], [width / 2, depth + 5], [-5, depth / 2]]) {
    m.place('Albero', multiply(origin, translate(x, y, 0)), '00_Sito::Alberi');
  }
  for (const [a, b] of [[[-7, -7.1, 0], [width + 7, -7, 1.2]], [[-7, depth + 7, 0], [width + 7, depth + 7.1, 1.2]]]) {
    m.mesh('00_Sito::Recinzione', at(box(a, b)));
  }
}

function house() {
  const m = new Model();
  defineMaterialsAndLayers(m);
  defineBlocks(m);
  try {
    m.block('Finestra doppia', [
      { block: 'Finestra 120x150', matrix: identity() },
      { block: 'Finestra 120x150', matrix: translate(1.3, 0, 0) },
    ]);
  } catch (e) {
    console.warn(`nested blocks unsupported here: ${e.message}`);
  }
  buildSite(m, identity(), W, D);
  buildHouse(m);
  m.brep('Studio::Volume', [20, 0, 0], [26, 6, 6], { name: 'Volume studio' });
  m.mesh('Note nascoste', box([0, -3, 0], [1, -2, 1]), { name: 'Nota' });
  const views = [
    m.namedView('Vista strada', { camera: [6, -25, 1.7], target: [6, 4.5, 3], lens: 35 }),
    m.namedView('Vista giardino', { camera: [-14, 20, 2], target: [6, 4.5, 3], lens: 24 }),
    m.namedView('Pianta', { camera: [6, 4.5, 60], target: [6, 4.5, 0], parallel: true }),
    m.namedView('Interno soggiorno', { camera: [3, 2, 1.6], target: [9, 7, 1.4], lens: 18 }),
  ];
  writeFileSync(join(outDir, 'house.3dm'), m.bytes());
  writeFileSync(join(outDir, 'house_v7.3dm'), m.bytes(7));
  writeFileSync(join(outDir, 'house.expected.json'), JSON.stringify(expected(m, views), null, 2) + '\n');
  console.log(`house: ${m.counts.objects} objects, ${m.expect.triangles} triangles`);
  writeVariants(m);
}

/**
 * Versions of the house with one known change each, edited in the same file so that every
 * other object keeps its UUID (the fingerprint test vectors, docs/fingerprint.md).
 */
function writeVariants(m) {
  const objects = m.file.objects();
  let studio = null;
  for (let i = 0; i < objects.count; i++) {
    if (objects.get(i).attributes().name === 'Volume studio') studio = objects.get(i);
  }
  if (!studio) throw new Error('Volume studio not found');
  // One object moved by 1 cm.
  studio.geometry().translate([0.01, 0, 0]);
  writeFileSync(join(outDir, 'house_moved.3dm'), m.bytes());
  studio.geometry().translate([-0.01, 0, 0]);
  // One layer renamed.
  const layers = m.file.layers();
  let volume = null;
  for (let i = 0; i < layers.count; i++) {
    if (layers.get(i).name === 'Volume') volume = layers.get(i);
  }
  if (!volume) throw new Error('layer Volume not found');
  volume.name = 'Massa';
  writeFileSync(join(outDir, 'house_renamed.3dm'), m.bytes());
  volume.name = 'Volume';
  // One object added.
  m.mesh('Studio::Volume', box([30, 0, 0], [31, 1, 1]), { name: 'Cubo aggiunto' });
  writeFileSync(join(outDir, 'house_added.3dm'), m.bytes());
  console.log('house variants: moved, renamed, added');
}

function expected(m, views) {
  const e = m.expect;
  const blockSize = (name) => {
    const min = [Infinity, Infinity, Infinity], max = [-Infinity, -Infinity, -Infinity];
    const walk = (n, matrix) => {
      for (const member of m.blocks[n].members) {
        if (member.block) walk(member.block, multiply(matrix, member.matrix));
        else for (const p of member.mesh.positions) {
          const q = apply(matrix, p);
          for (let i = 0; i < 3; i++) { min[i] = Math.min(min[i], q[i]); max[i] = Math.max(max[i], q[i]); }
        }
      }
    };
    walk(name, identity());
    return [0, 1, 2].map((i) => round(max[i] - min[i], 6));
  };
  return {
    note: 'Generated by tools/fixtures/generate.mjs; values computed from the generator geometry.',
    unit_system: 'meters',
    inventory: {
      ...m.counts, breps_with_render_mesh: 0, breps_tessellated: m.counts.breps,
      mesh_vertices: e.vertices, mesh_triangles: e.triangles,
    },
    layers: Object.fromEntries(Object.entries(m.layerInfo).map(([path, info]) => [path, {
      objects: m.layerObjects[path], visible: info.visible, material: info.material ?? null,
      color: '#' + info.color.map((c) => c.toString(16).padStart(2, '0')).join(''),
      mesh_area_m2: round(e.layerArea[path] ?? 0, 6),
    }])),
    materials: Object.fromEntries(Object.entries(m.materials).map(([name, info]) => [name, {
      color: '#' + info.color.map((c) => c.toString(16).padStart(2, '0')).join(''),
      mesh_area_m2: round(e.materialArea[name] ?? 0, 6),
    }])),
    blocks: Object.fromEntries(Object.keys(m.blocks).map((name) => [name, {
      placements: m.placements[name], members: m.blocks[name].members.length, size: blockSize(name),
    }])),
    groups: Object.fromEntries(Object.entries(m.groups).map(([name, g]) => [name, g.objects])),
    object_names: [...m.names].sort(),
    user_strings: Object.fromEntries(Object.entries(m.userStrings).map(([k, v]) => [k, { objects: v.objects, values: [...v.values].sort() }])),
    named_views: views,
    bbox: { min: e.min.map((v) => round(v, 6)), max: e.max.map((v) => round(v, 6)) },
    storeys: { count: 2, typical_height: 3.0, levels: [0, 3, 6] },
    style: {
      horizontal_area_share: round(e.horizontal / e.total, 4),
      vertical_area_share: round(e.vertical / e.total, 4),
      glazing_area_share: round(e.glazing / e.total, 4),
    },
  };
}

// ── The neighbourhood ─────────────────────────────────────────────────────────

function neighbourhood() {
  const targetMb = option('--target-mb', 64);
  let state = option('--seed', 1) >>> 0;
  const random = () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const m = new Model();
  defineMaterialsAndLayers(m);
  defineBlocks(m);
  // Each lot: a house of 1–3 storeys, its site, and a dense terrain grid (about 1 MB per
  // 40,000 vertices) that brings the file to its target size.
  const bytesPerVertex = 17;
  const terrainCells = 140;
  const perLot = (terrainCells + 1) ** 2 * bytesPerVertex + 200_000;
  const lots = Math.max(1, Math.ceil((targetMb * 1e6) / perLot));
  const side = Math.ceil(Math.sqrt(lots));
  for (let i = 0; i < lots; i++) {
    const width = 9 + Math.floor(random() * 7), depth = 7 + Math.floor(random() * 5);
    const storeys = 1 + Math.floor(random() * 3);
    const rotation = [0, 0, 15, 30, 45][Math.floor(random() * 5)];
    const origin = multiply(translate((i % side) * 45, Math.floor(i / side) * 45, 0), rotateZ(rotation));
    buildSite(m, origin, width, depth);
    buildHouse(m, origin, { storeys, width, depth, prefix: `Lotto ${i} ` });
    const at = (mesh) => ({ ...mesh, positions: mesh.positions.map((p) => apply(origin, p)) });
    m.mesh('00_Sito::Terreno', at(grid([-10, -10], [width + 10, depth + 10], -0.05, terrainCells)));
    if (i % 25 === 0) console.log(`lot ${i + 1}/${lots}`);
  }
  const name = `neighbourhood_${targetMb}mb`;
  const bytes = m.bytes();
  writeFileSync(join(outDir, `${name}.3dm`), bytes);
  const e = m.expect;
  writeFileSync(join(outDir, `${name}.expected.json`), JSON.stringify({
    note: 'Generated by tools/fixtures/generate.mjs; aggregates computed from the generator geometry.',
    lots,
    inventory: {
      ...m.counts, breps_with_render_mesh: 0, breps_tessellated: m.counts.breps,
      mesh_vertices: e.vertices, mesh_triangles: e.triangles,
    },
    blocks: Object.fromEntries(Object.keys(m.blocks).map((n) => [n, { placements: m.placements[n] }])),
    bbox: { min: e.min.map((v) => round(v, 6)), max: e.max.map((v) => round(v, 6)) },
  }, null, 2) + '\n');
  console.log(`${name}: ${lots} lots, ${(bytes.length / 1e6).toFixed(1)} MB, ${m.counts.objects} objects, ${e.triangles} triangles`);
}

/// A 10 × 6 × 4 m box in Rome with the model's north turned 30° counterclockwise from +Y, a
/// rectangular light under the ceiling, a spot light and a point light (values checked by
/// crates/stream3dm/tests/lights.rs).
function anchor() {
  const file = new rhino.File3dm();
  file.settings().modelUnitSystem = rhino.UnitSystem.Meters;
  const ea = file.settings().earthAnchorPoint;
  ea.earthBasepointLatitude = 41.9028;
  ea.earthBasepointLongitude = 12.4964;
  ea.earthBasepointElevation = 21;
  const t = (30 * Math.PI) / 180;
  ea.modelNorth = [-Math.sin(t), Math.cos(t), 0];
  ea.modelEast = [Math.cos(t), Math.sin(t), 0];
  file.settings().earthAnchorPoint = ea;
  const mesh = new rhino.Mesh();
  const v = [[0, 0, 0], [10, 0, 0], [10, 6, 0], [0, 6, 0], [0, 0, 4], [10, 0, 4], [10, 6, 4], [0, 6, 4]];
  for (const p of v) mesh.vertices().add(...p);
  for (const [a, b, c, d] of [[0, 3, 2, 1], [4, 5, 6, 7], [0, 1, 5, 4], [1, 2, 6, 5], [2, 3, 7, 6], [3, 0, 4, 7]]) mesh.faces().addQuadFace(a, b, c, d);
  file.objects().addMesh(mesh, null);
  const light = (style, name, set) => {
    const l = new rhino.Light();
    l.lightStyle = style;
    l.name = name;
    l.isEnabled = true;
    l.intensity = 1;
    set(l);
    file.objects().add(l, null);
  };
  light(rhino.LightStyle.WorldRectangular, 'Ceiling panel', (l) => {
    l.location = [4, 2.5, 3.9];
    l.length = [2, 0, 0];
    l.width = [0, 1, 0];
    l.direction = [0, 0, -1];
    l.diffuse = { r: 255, g: 244, b: 229, a: 255 };
  });
  light(rhino.LightStyle.WorldSpot, 'Spot', (l) => {
    l.location = [8, 1, 3.5];
    l.direction = [0, 0, -1];
    l.spotAngleRadians = (40 * Math.PI) / 180;
    l.diffuse = { r: 255, g: 255, b: 255, a: 255 };
  });
  light(rhino.LightStyle.WorldPoint, 'Bulb', (l) => {
    l.location = [1, 5, 2.5];
    l.diffuse = { r: 255, g: 200, b: 150, a: 255 };
  });
  writeFileSync(join(outDir, 'anchor.3dm'), file.toByteArray());
  console.log('anchor.3dm: earth anchor in Rome, north turned 30°, 3 lights');
}

/// Two rooms in metres for the lights: room A (6 × 5 × 3 m) with a window in its south wall, a
/// table, a ceiling panel and a spot; room B (4 × 5 m) behind a solid wall with a bulb, so
/// its light must not reach A. Rhino's sun is on (21 June 2026, 16:30, Milan, daylight saving),
/// north turned 30° from +y; the ground plane is on, visible, 10 cm under the floor.
function litRoom() {
  const file = new rhino.File3dm();
  file.settings().modelUnitSystem = rhino.UnitSystem.Meters;
  const rs = file.settings().renderSettings();
  const sun = rs.sun;
  sun.enableOn = true;
  sun.manualControlOn = false;
  sun.year = 2026;
  sun.month = 6;
  sun.day = 21;
  sun.hours = 16.5;
  sun.timeZone = 1;
  sun.daylightSavingOn = true;
  sun.daylightSavingMinutes = 60;
  sun.latitude = 45.4642;
  sun.longitude = 9.19;
  sun.north = 120;
  const ground = rs.groundPlane;
  ground.enabled = true;
  ground.autoAltitude = false;
  ground.altitude = -0.1;
  ground.shadowOnly = false;
  const add = (lo, hi, layer) => {
    const attrs = new rhino.ObjectAttributes();
    attrs.layerIndex = layer;
    file.objects().addMesh(toRhinoMesh(box(lo, hi)), attrs);
  };
  const layer = (name, rgb) => {
    const l = new rhino.Layer();
    l.name = name;
    l.color = { r: rgb[0], g: rgb[1], b: rgb[2], a: 255 };
    return file.layers().add(l);
  };
  const walls = layer('Walls', [230, 226, 218]);
  const floor = layer('Floor', [168, 132, 96]);
  const furniture = layer('Furniture', [120, 90, 60]);
  const T = 0.2;
  add([-T, -T, -0.3], [10.2 + T, 5 + T, 0], floor); // floor slab
  add([-T, -T, 3], [10.2 + T, 5 + T, 3.3], walls); // roof slab
  add([-T, -T, 0], [0, 5 + T, 3], walls); // west
  add([10.2, -T, 0], [10.2 + T, 5 + T, 3], walls); // east
  add([0, 5, 0], [10.2, 5 + T, 3], walls); // north
  add([6, 0, 0], [6.2, 5, 3], walls); // between A and B, solid
  // South wall with a window in room A (x 1.8–4.2, z 0.9–2.3); room B's part is solid.
  add([0, -T, 0], [1.8, 0, 3], walls);
  add([4.2, -T, 0], [10.2, 0, 3], walls);
  add([1.8, -T, 0], [4.2, 0, 0.9], walls);
  add([1.8, -T, 2.3], [4.2, 0, 3], walls);
  // A table in room A.
  add([2.2, 2.0, 0.72], [3.8, 2.9, 0.76], furniture);
  for (const [x, y] of [[2.25, 2.05], [3.7, 2.05], [2.25, 2.8], [3.7, 2.8]]) add([x, y, 0], [x + 0.05, y + 0.05, 0.72], furniture);
  const light = (style, name, set) => {
    const l = new rhino.Light();
    l.lightStyle = style;
    l.name = name;
    l.isEnabled = true;
    l.intensity = 1;
    set(l);
    file.objects().add(l, null);
  };
  light(rhino.LightStyle.WorldRectangular, 'Ceiling panel', (l) => {
    l.location = [2.4, 2.2, 2.95];
    l.length = [1.2, 0, 0];
    l.width = [0, 0.6, 0];
    l.direction = [0, 0, -1];
    l.diffuse = { r: 255, g: 244, b: 229, a: 255 };
  });
  light(rhino.LightStyle.WorldSpot, 'Spot over the table', (l) => {
    l.location = [3.0, 2.45, 2.9];
    l.direction = [0, 0, -1];
    l.spotAngleRadians = (35 * Math.PI) / 180;
    l.hotSpot = 0.5;
    l.diffuse = { r: 255, g: 255, b: 255, a: 255 };
  });
  light(rhino.LightStyle.WorldPoint, 'Bulb in room B', (l) => {
    l.location = [8.1, 2.5, 2.4];
    l.diffuse = { r: 255, g: 200, b: 150, a: 255 };
  });
  writeFileSync(join(outDir, 'lit_room.3dm'), file.toByteArray());
  console.log('lit_room.3dm: two rooms, three lights, Rhino sun on 21 June 16:30 in Milan, ground plane');
}

if (command === 'house') house();
else if (command === 'anchor') anchor();
else if (command === 'lit_room') litRoom();
else neighbourhood();
