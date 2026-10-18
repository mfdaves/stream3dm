//! Analysis of displayable triangles: bounds, areas, orientation and floor levels.

use std::collections::{BTreeMap, HashMap};

/// Normals within 10° of an axis count as aligned to it.
const ALIGN_COS: f64 = 0.984_807_753_012_208; // cos 10°
const ALIGN_SIN: f64 = 0.173_648_177_666_930_3; // sin 10°
/// Plan azimuths are folded into 0..90° and binned at 0.25°.
const AZ_BINS: usize = 360;
const AZ_BIN_DEG: f64 = 90.0 / AZ_BINS as f64;
/// Upward horizontal area is binned by elevation in 0.1 m steps.
const LEVEL_BIN_M: f64 = 0.1;
/// Elevations beyond ±100 km are left out of the level bins (which then stay bounded).
const MAX_LEVEL_Z_M: f64 = 100_000.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Bounds {
    pub(crate) fn point(p: [f64; 3]) -> Self {
        Self { min: p, max: p }
    }

    pub(crate) fn extend(&mut self, p: [f64; 3]) {
        self.min = [0, 1, 2].map(|i| self.min[i].min(p[i]));
        self.max = [0, 1, 2].map(|i| self.max[i].max(p[i]));
    }

    pub fn size(&self) -> [f64; 3] {
        [0, 1, 2].map(|i| self.max[i] - self.min[i])
    }
}

fn extend(bounds: &mut Option<Bounds>, p: [f64; 3]) {
    match bounds {
        Some(b) => b.extend(p),
        None => *bounds = Some(Bounds::point(p)),
    }
}

/// One displayable mesh, positions in metres.
pub struct MeshInput<'a> {
    pub positions: &'a [[f64; 3]],
    pub triangles: &'a [[u32; 3]],
    pub layer: Option<usize>,
    pub material: Option<usize>,
    pub glazing: bool,
    /// On a site layer: counted in areas, left out of floor levels.
    pub site: bool,
}

/// Areas are summed per mesh in `f64`, then committed in fixed point (units of 1e-12 m²), so
/// totals do not depend on the order meshes arrive in.
const FIXED_PER_M2: f64 = 1e12;

/// Area figures of one mesh, in fixed point.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MeshStats {
    pub area: i128,
    pub horizontal: i128,
    pub vertical: i128,
}

impl MeshStats {
    pub fn add(&mut self, other: MeshStats) {
        add_fixed(&mut self.area, other.area);
        add_fixed(&mut self.horizontal, other.horizontal);
        add_fixed(&mut self.vertical, other.vertical);
    }
}

/// Non-finite areas count as zero; huge ones saturate (as does the `as` cast).
pub fn fixed(area: f64) -> i128 {
    if area.is_finite() {
        (area * FIXED_PER_M2).round() as i128
    } else {
        0
    }
}

pub fn add_fixed(total: &mut i128, area: i128) {
    *total = total.saturating_add(area);
}

pub fn float(area: i128) -> f64 {
    area as f64 / FIXED_PER_M2
}

#[derive(Default)]
pub struct Accumulator {
    pub bounds: Option<Bounds>,
    pub layer_bounds: HashMap<usize, Bounds>,
    total: i128,
    horizontal: i128,
    vertical: i128,
    glazing: i128,
    layer_area: HashMap<usize, i128>,
    material_area: HashMap<usize, i128>,
    /// Vertical area by plan azimuth bin.
    azimuth: BTreeMap<usize, i128>,
    /// Upward horizontal area by elevation bin.
    levels: BTreeMap<i64, i128>,
    /// Per-mesh bin sums, reused between meshes.
    mesh_azimuth: Vec<(usize, f64)>,
    mesh_levels: Vec<(i64, f64)>,
}

/// Adds `area` to the sum for `bin` in a short list of per-mesh sums. Each bin's sum is built in
/// triangle order, exactly as a map would.
fn add_to_bin<K: PartialEq + Copy>(bins: &mut Vec<(K, f64)>, bin: K, area: f64) {
    if let Some(last) = bins.last_mut()
        && last.0 == bin
    {
        last.1 += area;
        return;
    }
    match bins.iter_mut().find(|(k, _)| *k == bin) {
        Some(entry) => entry.1 += area,
        None => bins.push((bin, area)),
    }
}

impl Accumulator {
    /// Adds a mesh and returns its own figures.
    pub fn add(&mut self, mesh: &MeshInput) -> MeshStats {
        let mut mesh_bounds: Option<Bounds> = None;
        for &p in mesh.positions {
            if p.iter().all(|c| c.is_finite()) {
                extend(&mut mesh_bounds, p);
            }
        }
        if let Some(b) = mesh_bounds {
            extend(&mut self.bounds, b.min);
            extend(&mut self.bounds, b.max);
            if let Some(layer) = mesh.layer {
                let lb = self
                    .layer_bounds
                    .entry(layer)
                    .or_insert_with(|| Bounds::point(b.min));
                lb.extend(b.min);
                lb.extend(b.max);
            }
        }
        let (mut total, mut horizontal, mut vertical) = (0.0, 0.0, 0.0);
        let mut azimuth = std::mem::take(&mut self.mesh_azimuth);
        let mut levels = std::mem::take(&mut self.mesh_levels);
        azimuth.clear();
        levels.clear();
        for tri in mesh.triangles {
            let [Some(a), Some(b), Some(c)] = tri.map(|i| mesh.positions.get(i as usize).copied())
            else {
                continue;
            };
            let n = cross(sub(b, a), sub(c, a));
            let len = norm(n);
            if len <= f64::EPSILON {
                continue;
            }
            let area = 0.5 * len;
            if !area.is_finite() {
                continue;
            }
            let n = [n[0] / len, n[1] / len, n[2] / len];
            total += area;
            if n[2].abs() >= ALIGN_COS {
                horizontal += area;
                let z = (a[2] + b[2] + c[2]) / 3.0;
                if n[2] > 0.0 && !mesh.site && z.abs() <= MAX_LEVEL_Z_M {
                    add_to_bin(&mut levels, (z / LEVEL_BIN_M).round() as i64, area);
                }
            } else if n[2].abs() <= ALIGN_SIN {
                vertical += area;
                let az = n[1].atan2(n[0]).to_degrees().rem_euclid(90.0);
                add_to_bin(
                    &mut azimuth,
                    ((az / AZ_BIN_DEG) as usize).min(AZ_BINS - 1),
                    area,
                );
            }
        }
        let stats = MeshStats {
            area: fixed(total),
            horizontal: fixed(horizontal),
            vertical: fixed(vertical),
        };
        let total = stats.area;
        add_fixed(&mut self.total, total);
        add_fixed(&mut self.horizontal, stats.horizontal);
        add_fixed(&mut self.vertical, stats.vertical);
        if mesh.glazing {
            add_fixed(&mut self.glazing, total);
        }
        if let Some(layer) = mesh.layer {
            add_fixed(self.layer_area.entry(layer).or_default(), total);
        }
        if let Some(material) = mesh.material {
            add_fixed(self.material_area.entry(material).or_default(), total);
        }
        for &(bin, area) in &azimuth {
            add_fixed(self.azimuth.entry(bin).or_default(), fixed(area));
        }
        for &(bin, area) in &levels {
            add_fixed(self.levels.entry(bin).or_default(), fixed(area));
        }
        self.mesh_azimuth = azimuth;
        self.mesh_levels = levels;
        stats
    }

    pub fn total_area(&self) -> f64 {
        float(self.total)
    }

    pub fn horizontal_area(&self) -> f64 {
        float(self.horizontal)
    }

    pub fn vertical_area(&self) -> f64 {
        float(self.vertical)
    }

    pub fn glazing_area(&self) -> f64 {
        float(self.glazing)
    }

    pub fn layer_area(&self, layer: usize) -> f64 {
        self.layer_area.get(&layer).copied().map_or(0.0, float)
    }

    pub fn material_area(&self, material: usize) -> f64 {
        self.material_area
            .get(&material)
            .copied()
            .map_or(0.0, float)
    }

    /// Share of area aligned to the vertical axis (horizontal faces) or to the dominant plan grid
    /// (vertical faces within 10° of it or of its perpendicular).
    pub fn orthogonality(&self) -> Option<f64> {
        if self.total <= 0 {
            return None;
        }
        let bins: Vec<i128> = (0..AZ_BINS)
            .map(|b| self.azimuth.get(&b).copied().unwrap_or(0))
            .collect();
        let window = |center: usize, half: usize| -> i128 {
            (0..=2 * half)
                .map(|k| bins[(center + AZ_BINS + k - half) % AZ_BINS])
                .fold(0i128, i128::saturating_add)
        };
        let smooth_half = (2.0 / AZ_BIN_DEG) as usize;
        let dominant = (0..AZ_BINS)
            .max_by(|&a, &b| {
                window(a, smooth_half)
                    .cmp(&window(b, smooth_half))
                    .then(b.cmp(&a))
            })
            .unwrap_or(0);
        let aligned_vertical = window(dominant, (10.0 / AZ_BIN_DEG) as usize);
        Some(float(self.horizontal.saturating_add(aligned_vertical)) / float(self.total))
    }

    /// Clusters of upward horizontal area, as (area-weighted elevation, area), lowest first.
    /// Bins are taken largest first and join the cluster whose peak lies within 0.35 m, so a
    /// run of small bins (stair treads) cannot chain two floors together.
    fn clusters(&self) -> Vec<(f64, f64)> {
        let mut bins: Vec<(i64, i128)> = self.levels.iter().map(|(&k, &a)| (k, a)).collect();
        bins.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        // (peak elevation, area-weighted elevation sum, area)
        let mut clusters: Vec<(f64, f64, f64)> = Vec::new();
        for (key, area) in bins {
            if area <= 0 {
                continue;
            }
            let (z, area) = (key as f64 * LEVEL_BIN_M, float(area));
            let nearest = clusters
                .iter_mut()
                .filter(|c| (c.0 - z).abs() <= 0.35)
                .min_by(|a, b| (a.0 - z).abs().total_cmp(&(b.0 - z).abs()));
            match nearest {
                Some(c) => {
                    c.1 += z * area;
                    c.2 += area;
                }
                None => clusters.push((z, z * area, area)),
            }
        }
        let mut out: Vec<(f64, f64)> = clusters.into_iter().map(|(_, zs, a)| (zs / a, a)).collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out
    }

    /// Raw floor-level candidates: the `max` largest clusters of upward horizontal area with at
    /// least 1% of the largest cluster, as (elevation, area), lowest first.
    pub fn levels(&self, max: usize) -> Vec<(f64, f64)> {
        let mut clusters = self.clusters();
        let largest = clusters.iter().map(|c| c.1).fold(0.0, f64::max);
        clusters.retain(|c| c.1 >= 0.01 * largest);
        clusters.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.total_cmp(&b.0)));
        clusters.truncate(max);
        clusters.sort_by(|a, b| a.0.total_cmp(&b.0));
        clusters
    }

    /// Floor levels: clusters with at least 20% of the largest one's area, read as storeys
    /// when there are at least three with regular spacing between 2 and 8 m.
    pub fn storeys(&self) -> Option<(usize, f64, Vec<f64>)> {
        let clusters = self.clusters();
        let largest = clusters.iter().map(|c| c.1).fold(0.0, f64::max);
        let levels: Vec<f64> = clusters
            .into_iter()
            .filter(|c| c.1 >= 0.2 * largest)
            .map(|c| c.0)
            .collect();
        if levels.len() < 3 {
            return None;
        }
        let mut gaps: Vec<f64> = levels.windows(2).map(|w| w[1] - w[0]).collect();
        gaps.sort_by(f64::total_cmp);
        let median = gaps[gaps.len() / 2];
        let regular = gaps
            .iter()
            .all(|g| (0.6 * median..=1.6 * median).contains(g));
        ((2.0..=8.0).contains(&median) && regular).then(|| (levels.len() - 1, median, levels))
    }
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub fn norm(v: [f64; 3]) -> f64 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Axis-aligned box from two corners, triangulated with outward normals.
    fn boxed(min: [f64; 3], max: [f64; 3]) -> (Vec<[f64; 3]>, Vec<[u32; 3]>) {
        let p = |x: usize, y: usize, z: usize| {
            [
                [min[0], max[0]][x],
                [min[1], max[1]][y],
                [min[2], max[2]][z],
            ]
        };
        let positions = vec![
            p(0, 0, 0),
            p(1, 0, 0),
            p(1, 1, 0),
            p(0, 1, 0),
            p(0, 0, 1),
            p(1, 0, 1),
            p(1, 1, 1),
            p(0, 1, 1),
        ];
        let quads = [
            [0, 3, 2, 1],
            [4, 5, 6, 7],
            [0, 1, 5, 4],
            [1, 2, 6, 5],
            [2, 3, 7, 6],
            [3, 0, 4, 7],
        ];
        let triangles = quads
            .iter()
            .flat_map(|q| [[q[0], q[1], q[2]], [q[0], q[2], q[3]]])
            .collect();
        (positions, triangles)
    }

    fn add_box(acc: &mut Accumulator, min: [f64; 3], max: [f64; 3]) {
        let (positions, triangles) = boxed(min, max);
        acc.add(&MeshInput {
            positions: &positions,
            triangles: &triangles,
            layer: Some(0),
            material: None,
            glazing: false,
            site: false,
        });
    }

    #[test]
    fn unit_box_is_fully_orthogonal() {
        let mut acc = Accumulator::default();
        add_box(&mut acc, [0.0; 3], [1.0; 3]);
        assert!((acc.total_area() - 6.0).abs() < 1e-12);
        assert!((acc.horizontal_area() - 2.0).abs() < 1e-12);
        assert!((acc.orthogonality().unwrap() - 1.0).abs() < 1e-12);
        assert_eq!(acc.bounds.unwrap().size(), [1.0; 3]);
    }

    #[test]
    fn stair_treads_do_not_merge_floors() {
        let mut acc = Accumulator::default();
        for floor in 0..=2 {
            let z = floor as f64 * 3.0;
            add_box(&mut acc, [0.0, 0.0, z - 0.3], [12.0, 9.0, z]);
        }
        for step in 0..17 {
            let top = (step + 1) as f64 * 3.0 / 17.0;
            add_box(
                &mut acc,
                [step as f64 * 0.27, 0.0, 0.0],
                [(step + 1) as f64 * 0.27, 1.0, top],
            );
        }
        let (count, height, levels) = acc.storeys().unwrap();
        assert_eq!(count, 2);
        assert!((height - 3.0).abs() < 0.01, "{height}");
        assert!(
            levels
                .iter()
                .zip([0.0, 3.0, 6.0])
                .all(|(l, e)| (l - e).abs() < 0.01),
            "{levels:?}"
        );
    }

    #[test]
    fn stacked_slabs_give_storeys() {
        let mut acc = Accumulator::default();
        for floor in 0..=8 {
            let z = floor as f64 * 3.2;
            add_box(&mut acc, [0.0, 0.0, z - 0.3], [20.0, 12.0, z]);
        }
        let (count, height, levels) = acc.storeys().unwrap();
        assert_eq!(count, 8);
        assert!((height - 3.2).abs() < 1e-9);
        assert_eq!(levels.len(), 9);
    }
}
