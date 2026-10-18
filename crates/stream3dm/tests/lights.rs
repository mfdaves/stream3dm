//! Lights from the light table, checked against rhino3dm.js (McNeel's reader) on every public
//! sample that has them (tests/fixtures/lights-rhino3dm.json): kind, colour, position,
//! direction and spot angle. Rhino 1 files keep their lights in the object table, which cadmpeg
//! (our fallback for them) does not give as light records: they are left out until the reader
//! reads Rhino 1 itself.

use std::path::PathBuf;
use stream3dm::inspect;

fn samples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples")
}

#[test]
fn lights_match_rhino3dm() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/lights-rhino3dm.json")).unwrap();
    let mut checked = 0;
    for (file, expected) in fixture["files"].as_object().unwrap() {
        let Ok(bytes) = std::fs::read(samples().join(file)) else {
            eprintln!("skipped: {file} not fetched (samples/fetch.sh)");
            continue;
        };
        if file.contains("/V1/") {
            continue;
        }
        let report = inspect(&bytes).unwrap();
        let unit = report.file.metres_per_unit.unwrap_or(0.001);
        let expected = expected.as_array().unwrap();
        assert_eq!(report.lights.len(), expected.len(), "{file}: light count");
        assert_eq!(report.inventory.lights, expected.len(), "{file}: inventory");
        for (got, want) in report.lights.iter().zip(expected) {
            let kind = match want["style"].as_str().unwrap() {
                "WorldSpot" => "spot",
                "WorldPoint" => "point",
                "WorldDirectional" => "directional",
                other => panic!("unexpected style {other}"),
            };
            assert_eq!(got.kind, kind, "{file}");
            assert!(!got.camera_relative, "{file}");
            assert_eq!(got.name, want["name"].as_str().unwrap(), "{file}");
            let color: Vec<u8> = want["color"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect();
            assert_eq!(got.color.to_vec(), color, "{file}: colour");
            for k in 0..3 {
                let w = want["location"][k].as_f64().unwrap() * unit;
                assert!(
                    (got.location[k] - w).abs() <= 1e-6 * w.abs().max(1.0),
                    "{file}: location {:?} vs {w}",
                    got.location
                );
            }
            if kind != "point" {
                let d: Vec<f64> = (0..3)
                    .map(|k| want["direction"][k].as_f64().unwrap())
                    .collect();
                let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
                for (g, w) in got.direction.iter().zip(&d) {
                    assert!((g - w / len).abs() < 1e-5, "{file}: direction");
                }
            }
            if kind == "spot" {
                let angle = got.spot_angle_deg.unwrap();
                assert!(
                    (angle - want["spot_deg"].as_f64().unwrap()).abs() < 1e-3,
                    "{file}: spot {angle}"
                );
            }
            checked += 1;
        }
    }
    if checked > 0 {
        assert!(checked >= 30, "{checked} lights checked");
        eprintln!("{checked} lights checked against rhino3dm.js");
    }
}

#[test]
fn no_earth_anchor_in_files_that_set_none() {
    let Ok(bytes) = std::fs::read(samples().join("synthetic/house.3dm")) else {
        return;
    };
    assert_eq!(inspect(&bytes).unwrap().file.earth_anchor, None);
}

#[test]
fn earth_anchor_and_lights_of_the_synthetic_fixture() {
    // tools/fixtures/generate.mjs anchor: Rome, north turned 30° counterclockwise from +Y.
    let Ok(bytes) = std::fs::read(samples().join("synthetic/anchor.3dm")) else {
        return;
    };
    let report = inspect(&bytes).unwrap();
    let a = report.file.earth_anchor.expect("earth anchor");
    assert_eq!(
        (a.latitude, a.longitude, a.elevation_m),
        (41.9028, 12.4964, 21.0)
    );
    let t = 30f64.to_radians();
    assert!((a.north[0] + t.sin()).abs() < 1e-6 && (a.north[1] - t.cos()).abs() < 1e-6);
    assert!((a.east[0] - t.cos()).abs() < 1e-6 && (a.east[1] - t.sin()).abs() < 1e-6);
    let kinds: Vec<&str> = report.lights.iter().map(|l| l.kind.as_str()).collect();
    assert_eq!(kinds, ["rectangular", "spot", "point"]);
    let panel = &report.lights[0];
    assert_eq!(panel.name, "Ceiling panel");
    assert_eq!(panel.color, [255, 244, 229]);
    assert_eq!(panel.location, [4.0, 2.5, 3.9]);
    assert_eq!(panel.length, Some([2.0, 0.0, 0.0]));
    assert_eq!(panel.width, Some([0.0, 1.0, 0.0]));
    assert_eq!(report.lights[1].spot_angle_deg, Some(40.0));
    assert!(report.lights.iter().all(|l| l.on));
}

#[test]
fn earth_anchor_of_a_public_sample() {
    // v5_ring.3dm sets Taipei, with the default north.
    let Ok(bytes) = std::fs::read(samples().join("opennurbs/example_files/V5/v5_ring.3dm")) else {
        return;
    };
    let a = inspect(&bytes)
        .unwrap()
        .file
        .earth_anchor
        .expect("earth anchor");
    assert_eq!((a.latitude, a.longitude), (25.0351, 121.5067));
    assert_eq!((a.north, a.east), ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0]));
}

/// Rhino's Sun panel and ground plane from the RDK document data, on the synthetic lit room
/// (tools/fixtures: lit_room), where the earth anchor holds the place and north.
#[test]
fn rhino_sun_and_ground_plane() {
    let path = samples().join("synthetic/lit_room.3dm");
    let bytes = std::fs::read(&path).expect("samples/synthetic/lit_room.3dm is committed");
    let report = inspect(&bytes).unwrap();
    let render = report.render.expect("render settings");
    let sun = render.sun.expect("sun");
    assert!(sun.enabled && !sun.manual && sun.skylight);
    assert_eq!((sun.date.as_str(), sun.hours), ("2026-06-21", 16.5));
    assert_eq!(
        (sun.utc_offset, sun.daylight_saving_minutes),
        (1.0, Some(60))
    );
    // The earth anchor holds the place and north, so the sun leaves them out.
    assert_eq!((sun.latitude, sun.longitude, sun.north), (None, None, None));
    let anchor = report.file.earth_anchor.expect("earth anchor");
    assert_eq!((anchor.latitude, anchor.longitude), (45.4642, 9.19));
    assert_eq!(anchor.north, [-0.5, 0.866025, 0.0]);
    let ground = render.ground_plane.expect("ground plane");
    assert!(ground.enabled && !ground.auto_altitude && !ground.shadow_only);
    assert_eq!(ground.altitude_m, -0.1);
    let kinds: Vec<_> = report
        .lights
        .iter()
        .map(|l| (l.name.as_str(), l.kind.as_str(), l.on))
        .collect();
    assert_eq!(
        kinds,
        [
            ("Ceiling panel", "rectangular", true),
            ("Spot over the table", "spot", true),
            ("Bulb in room B", "point", true)
        ]
    );
}

/// A file without RDK document data has no render settings.
#[test]
fn no_render_settings_without_rdk_data() {
    let Ok(bytes) =
        std::fs::read(samples().join("opennurbs/example_files/V8/v8_caps_and_joins.3dm"))
    else {
        eprintln!("skipped: samples not fetched");
        return;
    };
    assert!(inspect(&bytes).unwrap().render.is_none());
}
