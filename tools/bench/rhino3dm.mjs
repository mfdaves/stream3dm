// Our reader (stream3dm-wasm) against McNeel's rhino3dm.js, the library browser tools use to open
// .3dm (three.js's 3DMLoader wraps it), on the same files in the same runtime (Node).
//
//   node tools/bench/rhino3dm.mjs <file.3dm>...      prints a Markdown table, JSON to stderr
//   node tools/bench/rhino3dm.mjs --one <engine> <file.3dm>   (internal: one run, one process)
//
// Each engine and file runs in its own process, so the peak memory (max RSS) is that run's.
// Both report what they can display (visibility as Rhino applies it, parent layers included):
// visible meshes, Breps and extrusions through their meshes (rhino3dm.js only has the render
// meshes Rhino cached; ours tessellates the rest), SubD control nets, curves and points (point
// clouds expanded), block placements expanded, as 3DMLoader does. Needs the release wasm build
// (cargo build -j 1 --release -p stream3dm-wasm --target wasm32-unknown-unknown) and
// tools/fixtures/node_modules (npm ci in tools/fixtures).
import { execFileSync } from 'node:child_process';
import { closeSync, openSync, readFileSync, readSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const WASM = join(root, 'target/wasm32-unknown-unknown/release/stream3dm_wasm.wasm');

async function runRhino3dm(path) {
  const { default: rhino3dm } = await import(join(root, 'tools/fixtures/node_modules/rhino3dm/rhino3dm.module.js')).catch(() =>
    import(join(root, 'tools/fixtures/node_modules/rhino3dm/rhino3dm.js')),
  );
  const t0 = performance.now();
  const rhino = await rhino3dm();
  const init = performance.now() - t0;
  const t1 = performance.now();
  const file = rhino.File3dm.fromByteArray(new Uint8Array(readFileSync(path)));
  if (!file) throw new Error('rhino3dm could not read the file');
  const parse = performance.now() - t1;
  const t2 = performance.now();
  const layers = file.layers();
  // Visible as Rhino shows it: the layer and all its parents on (3DMLoader checks only the
  // layer itself, so it shows objects whose parent layer is off).
  const layerById = new Map();
  for (let i = 0; i < layers.count; i++) layerById.set(layers.get(i).id, layers.get(i));
  const layerVisible = (i) => {
    for (let l = i >= 0 && i < layers.count ? layers.get(i) : null; l; l = layerById.get(l.parentLayerId)) {
      if (!l.visible) return false;
    }
    return true;
  };
  const objects = file.objects();
  const byId = new Map();
  for (let i = 0; i < objects.count; i++) {
    const o = objects.get(i);
    byId.set(o.attributes().id, o);
  }
  const idefs = file.instanceDefinitions();
  const stats = { objects: objects.count, triangles: 0, curves: 0, point_sets: 0, points: 0, breps: 0, breps_shown: 0, breps_partly: 0, extrusions: 0, extrusions_shown: 0, meshes: 0, subds: 0, placements: 0 };
  // Curves and sets of points of one geometry: a curve, a point, a point cloud.
  const wires = (g) => {
    if (g.objectType === rhino.ObjectType.Curve) return { curves: 1, point_sets: 0, points: 0 };
    if (g.objectType === rhino.ObjectType.Point) return { curves: 0, point_sets: 1, points: 1 };
    if (g.objectType === rhino.ObjectType.PointSet) return { curves: 0, point_sets: 1, points: g.count };
    return { curves: 0, point_sets: 0, points: 0 };
  };
  const add = (into, more) => {
    for (const k of ['triangles', 'curves', 'point_sets', 'points']) into[k] += more[k];
  };
  const meshTriangles = (m) => (m ? m.faces().triangleCount + 2 * m.faces().quadCount : 0);
  // Triangles of one geometry (counting kinds once, at the top level).
  const triangles = (g, count) => {
    switch (g.constructor.name) {
      case 'Mesh':
        if (count) stats.meshes++;
        return meshTriangles(g);
      case 'Brep': {
        const faces = g.faces();
        let t = 0;
        let meshed = 0;
        for (let k = 0; k < faces.count; k++) {
          const m = faces.get(k).getMesh(rhino.MeshType.Any);
          if (m) {
            meshed++;
            t += meshTriangles(m);
          }
        }
        if (count) {
          stats.breps++;
          if (meshed === faces.count && faces.count > 0) stats.breps_shown++;
          else if (meshed > 0) stats.breps_partly++;
        }
        return t;
      }
      case 'Extrusion': {
        const m = g.getMesh(rhino.MeshType.Any);
        if (count) {
          stats.extrusions++;
          if (m) stats.extrusions_shown++;
        }
        return meshTriangles(m);
      }
      case 'SubD': {
        if (count) stats.subds++;
        return meshTriangles(rhino.Mesh.createFromSubDControlNet(g, false));
      }
      default:
        return 0;
    }
  };
  // A definition's triangles, curves and points, nested placements included (each definition
  // computed once).
  const none = () => ({ triangles: 0, curves: 0, point_sets: 0, points: 0 });
  const definitionTotals = new Map();
  const definition = (id, depth) => {
    if (definitionTotals.has(id)) return definitionTotals.get(id);
    definitionTotals.set(id, none()); // cycles
    const d = idefs.findId(id);
    const t = none();
    for (const member of d ? d.getObjectIds() : []) {
      const o = byId.get(member);
      if (!o) continue;
      const g = o.geometry();
      if (g.constructor.name === 'InstanceReference') {
        if (depth < 8) add(t, definition(g.parentIdefId, depth + 1));
      } else {
        add(t, { triangles: triangles(g, false), ...wires(g) });
      }
    }
    definitionTotals.set(id, t);
    return t;
  };
  for (let i = 0; i < objects.count; i++) {
    const o = objects.get(i);
    const a = o.attributes();
    if (a.isInstanceDefinitionObject || !a.visible || !layerVisible(a.layerIndex)) continue;
    const g = o.geometry();
    if (g.constructor.name === 'InstanceReference') {
      stats.placements++;
      add(stats, definition(g.parentIdefId, 0));
    } else {
      add(stats, { triangles: triangles(g, true), ...wires(g) });
    }
  }
  const walk = performance.now() - t2;
  return { init_s: init / 1000, read_s: (parse + walk) / 1000, parse_s: parse / 1000, ...stats };
}

async function runStream3dm(path) {
  const t0 = performance.now();
  const { instance } = await WebAssembly.instantiate(readFileSync(WASM), {});
  const w = instance.exports;
  const init = performance.now() - t0;
  const t1 = performance.now();
  const SLICE = 1 << 20;
  const session = w.stream3dm_new_with_meshes(0);
  w.stream3dm_edges?.(session, 0); // edges are not part of the comparison
  const slice = w.stream3dm_alloc(SLICE);
  const chunk = (w.stream3dm_alloc(48) + 7) & ~7;
  let triangles = 0;
  const wires = { curves: 0, point_sets: 0, points: 0 };
  const drain = () => {
    w.stream3dm_chunk(session, chunk);
    const c = new Uint32Array(w.memory.buffer, chunk, 10);
    triangles += c[5] / 3;
    // Ranges of eight u32: a curve has segments only, a set of points vertices only.
    const ranges = new Uint32Array(w.memory.buffer, c[6], c[7]);
    for (let r = 0; r < ranges.length; r += 8) {
      const [vertices, indices, segments] = [ranges[r + 1], ranges[r + 6], ranges[r + 7]];
      if (indices === 0 && vertices === 0 && segments > 0) wires.curves++;
      else if (indices === 0 && segments === 0 && vertices > 0) {
        wires.point_sets++;
        wires.points += vertices;
      }
    }
    w.stream3dm_chunk_clear(session);
  };
  const fd = openSync(path, 'r');
  let report = null;
  try {
    for (;;) {
      let position = 0;
      for (;;) {
        const view = new Uint8Array(w.memory.buffer, slice, SLICE);
        const n = readSync(fd, view, 0, SLICE, position);
        if (n === 0) break;
        position += n;
        if (w.stream3dm_push(session, slice, n) !== 0) throw new Error(readText(w, session));
        drain();
      }
      const status = w.stream3dm_finish(session);
      drain();
      if (status === 1) continue;
      if (status !== 0) throw new Error(readText(w, session));
      report = JSON.parse(readText(w, session));
      break;
    }
  } finally {
    closeSync(fd);
  }
  const read = performance.now() - t1;
  const i = report.inventory;
  return {
    init_s: init / 1000,
    read_s: read / 1000,
    objects: i.objects,
    triangles,
    ...wires,
    breps: i.breps,
    breps_shown: i.breps_with_render_mesh + i.breps_tessellated,
    meshes: i.meshes,
    subds: i.subds,
    placements: report.blocks.reduce((n, b) => n + b.placements, 0),
  };
}

function readText(w, session) {
  return new TextDecoder().decode(new Uint8Array(w.memory.buffer, w.stream3dm_result_ptr(session), w.stream3dm_result_len(session)).slice());
}

const args = process.argv.slice(2);
if (args[0] === '--one') {
  const [, engine, path] = args;
  try {
    const result = engine === 'stream3dm' ? await runStream3dm(path) : await runRhino3dm(path);
    console.log(JSON.stringify({ ok: true, ...result, rss_mb: process.resourceUsage().maxRSS / 1024 }));
  } catch (error) {
    console.log(JSON.stringify({ ok: false, error: String(error?.message ?? error).slice(0, 200), rss_mb: process.resourceUsage().maxRSS / 1024 }));
  }
  process.exit(0);
}

const rows = [];
for (const path of args) {
  const mb = statSync(path).size / 1e6;
  const row = { file: path, mb };
  for (const engine of ['stream3dm', 'rhino3dm']) {
    let out;
    try {
      out = execFileSync(process.execPath, ['--max-old-space-size=8192', fileURLToPath(import.meta.url), '--one', engine, path], {
        encoding: 'utf8',
        timeout: 600_000,
        maxBuffer: 1 << 24,
        stdio: ['ignore', 'pipe', 'ignore'],
      });
      row[engine] = JSON.parse(out.trim().split('\n').pop());
    } catch (error) {
      row[engine] = { ok: false, error: error.killed ? 'timed out' : `crashed (${error.status ?? error.signal})` };
    }
  }
  rows.push(row);
  console.error(JSON.stringify(row));
}

const fmt = (r, key, digits = 2) => (r?.ok && r[key] !== undefined ? Number(r[key]).toFixed(digits) : '–');
const shown = (r) => (r?.ok ? `${r.breps_shown}/${r.breps}` : r?.error ?? '–');
const wired = (r) => (r?.ok ? `${r.curves} / ${r.points}` : '–');
console.log('| File | MB | ours: read s | rhino3dm: read s | ours: peak MB | rhino3dm: peak MB | ours: triangles | rhino3dm: triangles | ours: Breps shown | rhino3dm: Breps shown | ours: curves / points | rhino3dm: curves / points |');
console.log('|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|');
for (const r of rows) {
  const name = r.file.split('/').pop();
  console.log(
    `| ${name} | ${r.mb.toFixed(1)} | ${fmt(r.stream3dm, 'read_s')} | ${fmt(r.rhino3dm, 'read_s')} | ${fmt(r.stream3dm, 'rss_mb', 0)} | ${fmt(r.rhino3dm, 'rss_mb', 0)} | ` +
      `${r.stream3dm?.ok ? r.stream3dm.triangles.toLocaleString('en') : r.stream3dm?.error} | ${r.rhino3dm?.ok ? r.rhino3dm.triangles.toLocaleString('en') : r.rhino3dm?.error} | ${shown(r.stream3dm)} | ${shown(r.rhino3dm)} | ${wired(r.stream3dm)} | ${wired(r.rhino3dm)} |`,
  );
}
