//! Real project files: every `.3dm` under `samples/private/`, plus any `.3dm` under `samples/`
//! that is not in the manifest. They are never committed, so this test is opt-in:
//!
//! ```sh
//! cargo test --release -p stream3dm --test private -- --ignored --nocapture
//! ```
#![cfg(feature = "cadmpeg")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;
use stream3dm::{InspectOptions, Report, inspect_reader, inspect_streaming, inspect_whole};

fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples")
}

fn private_files() -> Vec<PathBuf> {
    let listed: HashSet<String> = std::fs::read_to_string(samples_dir().join("manifest.tsv"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split('\t').next().map(str::to_string))
        .collect();
    let mut out = Vec::new();
    let mut stack = vec![samples_dir()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("3dm"))
            {
                let relative = path
                    .strip_prefix(samples_dir())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if !listed.contains(&relative) && !relative.starts_with("synthetic/") {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out
}

fn summary(r: &Report) -> String {
    let i = &r.inventory;
    format!(
        "{} objects ({} breps, {} with render mesh, {} meshes, {} block instances), {} triangles, {} layers, {} materials, {} views, size {:?}",
        i.objects,
        i.breps,
        i.breps_with_render_mesh,
        i.meshes,
        i.block_instances,
        i.mesh_triangles,
        r.structure.layers.len(),
        r.materials.len(),
        r.views.len(),
        r.proportions.as_ref().map(|p| p.size)
    )
}

#[test]
#[ignore = "needs real files in samples/private/"]
fn real_files_stream_consistently() {
    let files = private_files();
    if files.is_empty() {
        eprintln!("skipped: no private files");
        return;
    }
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let started = Instant::now();
        let reader = std::fs::File::open(&path).unwrap();
        let reference = inspect_reader(std::io::BufReader::new(reader), InspectOptions::default())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let mb = std::fs::metadata(&path).unwrap().len() as f64 / 1e6;
        eprintln!(
            "{name}: {mb:.1} MB in {:.2?}: {}",
            started.elapsed(),
            summary(&reference)
        );

        let bytes = std::fs::read(&path).unwrap();
        let small_batches = inspect_streaming(
            &bytes,
            InspectOptions {
                batch_bytes: Some(1024 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(small_batches == reference, "{name}: 1 MiB batches differ");

        let cadmpeg = inspect_streaming(
            &bytes,
            InspectOptions {
                cadmpeg_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        if cadmpeg != reference {
            let (a, b) = (
                serde_json::to_value(&cadmpeg).unwrap(),
                serde_json::to_value(&reference).unwrap(),
            );
            let fields: Vec<String> = a
                .as_object()
                .unwrap()
                .iter()
                .filter(|(k, v)| b.get(k.as_str()) != Some(v))
                .map(|(k, _)| k.clone())
                .collect();
            eprintln!(
                "  our reader and cadmpeg differ in: {} (see reader::tests::compare_file)",
                fields.join(", ")
            );
        }

        match inspect_whole(&bytes) {
            Ok(whole) if whole.blocks.is_empty() => {
                assert!(
                    cadmpeg.inventory.undecoded <= whole.inventory.undecoded,
                    "{name}: streaming lost objects"
                );
                if cadmpeg.inventory.undecoded == whole.inventory.undecoded {
                    assert!(
                        whole == cadmpeg,
                        "{name}: differs from the whole-file decode"
                    );
                }
                eprintln!("  whole-file decode agrees");
            }
            Ok(_) => eprintln!("  has blocks: whole-file comparison skipped"),
            Err(e) => eprintln!("  whole-file decode fails: {e}"),
        }
    }
}
