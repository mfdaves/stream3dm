// Tests of stream3dm.js on the synthetic and public samples (samples/fetch.sh), with the module
// built by `npm run build` (or STREAM3DM_WASM). Skips what is missing.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, openAsBlob, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { Stream3dmError, init } from '../stream3dm.js';

const here = new URL('..', import.meta.url);
const fromEnv = process.env.STREAM3DM_WASM && pathToFileURL(resolve(process.env.STREAM3DM_WASM));
const candidates = [fromEnv, new URL('stream3dm_wasm.wasm', here), new URL('../target/wasm32-unknown-unknown/release/stream3dm_wasm.wasm', here)];
const wasm = candidates.find((c) => c && existsSync(c));
const sample = (path) => new URL(`../samples/${path}`, here);
const skip = (path) => (!wasm ? 'module not built (npm run build)' : path && !existsSync(sample(path)) ? `${path} not present (samples/fetch.sh)` : false);

const reader = wasm ? await init(wasm) : null;

test('reads the synthetic house with its meshes and fingerprint', { skip: skip('synthetic/house.3dm') }, async () => {
  const bytes = readFileSync(sample('synthetic/house.3dm'));
  const passes = new Set();
  const { report, context, meshes, fingerprint, stats } = await reader.read(bytes, { meshes: true, onProgress: ({ pass }) => passes.add(pass) });
  assert.equal(typeof report.schema_version, 'number');
  assert.ok(context);
  // Each chunk's ranges account for its vertices, indices and segments.
  let triangles = 0;
  for (const c of meshes.chunks) {
    const sum = (k) => c.ranges.reduce((n, v, i) => (i % 8 === k ? n + v : n), 0);
    assert.equal(sum(1) * 3, c.positions.length);
    assert.equal(c.colors.length, c.positions.length);
    assert.equal(sum(6), c.indices.length);
    assert.equal(sum(7) * 6, c.lines.length);
    triangles += c.indices.length / 3;
  }
  // Drawn: everything but what lies on hidden layers.
  assert.ok(triangles > 0 && triangles <= report.inventory.mesh_triangles);
  assert.ok(meshes.picks.length > 0);
  assert.ok(fingerprint && Object.keys(fingerprint).length > 0);
  assert.equal(stats.failed, 0);
  assert.deepEqual([...passes], [1, 2]);
});

test('a Blob reads the same as its bytes, files with blocks in two passes', { skip: skip('rhino3dm/tests/models/blocks.3dm') }, async () => {
  const path = sample('rhino3dm/tests/models/blocks.3dm');
  const passes = new Set();
  const fromBytes = await reader.read(readFileSync(path));
  const fromBlob = await reader.read(await openAsBlob(path), { onProgress: ({ pass }) => passes.add(pass) });
  assert.deepEqual(fromBlob.report, fromBytes.report);
  assert.deepEqual([...passes], [1, 2]);
  assert.equal(fromBytes.report.blocks[0].placements, 2);
});

test('a function source is read once per pass', { skip: skip('rhino3dm/tests/models/blocks.3dm') }, async () => {
  const bytes = readFileSync(sample('rhino3dm/tests/models/blocks.3dm'));
  let calls = 0;
  const source = async function* () {
    calls++;
    for (let at = 0; at < bytes.length; at += 1000) yield bytes.subarray(at, at + 1000);
  };
  const { report } = await reader.read(source);
  assert.equal(calls, 2);
  assert.deepEqual(report, (await reader.read(bytes)).report);
});

test('the same file twice: a fingerprint diff with no change', { skip: skip('synthetic/house.3dm') }, async () => {
  const bytes = readFileSync(sample('synthetic/house.3dm'));
  const key = new Uint8Array(32).fill(7);
  const a = await reader.read(bytes, { fingerprint: { key } });
  const b = await reader.read(bytes, { fingerprint: { key } });
  const moved = await reader.read(readFileSync(sample('synthetic/house_moved.3dm')), { fingerprint: { key } });
  assert.deepEqual(a.fingerprint, b.fingerprint);
  const same = reader.diff(a.fingerprint, JSON.stringify(b.fingerprint));
  const changed = reader.diff(a.fingerprint, moved.fingerprint);
  assert.equal(same.same, true);
  assert.equal(changed.same, false);
  assert.throws(() => reader.diff({}, a.fingerprint), Stream3dmError);
});

test('the small module says it is not the full build', { skip: skip() }, () => {
  assert.equal(reader.full, false);
});

test('bytes that are not a .3dm file are refused', { skip: skip() }, async () => {
  await assert.rejects(reader.read(new TextEncoder().encode('not a model')), Stream3dmError);
});
