//! The IFC reader on the IFC samples (`samples/fetch.sh ifc.tsv`): every file is read in 1 MiB
//! slices without error, lists its products and storeys, and gives the same report for any
//! slice size. Skipped when the samples are not fetched. `--nocapture` prints the read speed.

use std::path::PathBuf;
use std::time::Instant;
use stream3dm::IfcInspector;

fn samples() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples");
    let Ok(manifest) = std::fs::read_to_string(root.join("ifc.tsv")) else {
        return Vec::new();
    };
    manifest
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| root.join(l.split('\t').next().unwrap()))
        .filter(|p| p.exists())
        .collect()
}

fn read(bytes: &[u8], slice: usize) -> (stream3dm::Report, usize) {
    let mut ifc = IfcInspector::new();
    for part in bytes.chunks(slice) {
        ifc.push(part).unwrap();
    }
    ifc.prepare().unwrap();
    let products = ifc.product_count();
    (ifc.finish().unwrap(), products)
}

#[test]
fn every_sample_reads() {
    let files = samples();
    if files.is_empty() {
        eprintln!("skipped: run samples/fetch.sh ifc.tsv");
        return;
    }
    let (mut megabytes, mut seconds) = (0.0, 0.0);
    for path in &files {
        let bytes = std::fs::read(path).unwrap();
        let t = Instant::now();
        let (report, products) = read(&bytes, 1 << 20);
        let s = t.elapsed().as_secs_f64();
        megabytes += bytes.len() as f64 / 1e6;
        seconds += s;
        let name = path.file_name().unwrap().to_string_lossy();
        let storeys = report
            .structure
            .layers
            .iter()
            .filter(|l| !l.path.contains("::"))
            .count()
            - 1;
        println!(
            "{name}: {products} products, {storeys} storeys, {} materials, {} types, {:.0} MB/s",
            report.materials.len(),
            report.blocks.len(),
            bytes.len() as f64 / 1e6 / s
        );
        assert!(
            report
                .file
                .application_details
                .as_deref()
                .is_some_and(|s| s.starts_with("IFC")),
            "{name}: schema read"
        );
        assert!(products > 0, "{name}: products found");
    }
    println!(
        "{} files, {megabytes:.0} MB at {:.0} MB/s",
        files.len(),
        megabytes / seconds
    );
}

#[test]
fn slice_size_does_not_change_the_report() {
    for path in samples()
        .iter()
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() < 3_000_000))
    {
        let bytes = std::fs::read(path).unwrap();
        let reference = serde_json::to_string(&read(&bytes, bytes.len()).0).unwrap();
        for slice in [4096, 65_537] {
            assert_eq!(
                serde_json::to_string(&read(&bytes, slice).0).unwrap(),
                reference,
                "{} in slices of {slice}",
                path.display()
            );
        }
    }
}
