use cadmpeg_codec_rhino::RhinoCodec;
use cadmpeg_ir::{Codec, DecodeOptions};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use stream3dm::{Projection, Report, Severity, ViewKind};

/// Counts heap bytes in use and their peak, for `--stats`.
struct Counting;

static IN_USE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = IN_USE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                let now = IN_USE.fetch_add(new_size - layout.size(), Ordering::Relaxed)
                    + (new_size - layout.size());
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                IN_USE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const USAGE: &str = "usage:
  stream3dm inspect <file.3dm> [--json] [--stats] [--cadmpeg]
                                    print the extraction report (--stats: time, MB/s, peak heap;
                                    --cadmpeg: decode objects with cadmpeg only, for comparison)
  stream3dm context <file.3dm>      print the context, a compact summary of the report (JSON)
  stream3dm render <file.3dm> [--out DIR] [--size WxH] [--view NAME]
                                    render clay, depth, normals, materials and lines per view
  stream3dm native <file.3dm> [arena] [--records N]
                                    list the decoder's native Rhino arenas, or dump one as JSON
                                    (--records: decode the tables and first N objects only)
  stream3dm fingerprint <file.3dm> [--names-clear] [--pretty]
                                    print the stream3dm fingerprint (docs/fingerprint.md), keyed
                                    with STREAM3DM_FINGERPRINT_KEY (64 hex digits) or the public key
  stream3dm diff <before.json> <after.json> [--json]
                                    compare two fingerprints made with the same key
  stream3dm ifc <file.ifc> [--products]
                                    read an IFC file's metadata (no geometry): summary and
                                    speed, or every product as JSON lines (--products)
  stream3dm --help | --version";

fn main() -> ExitCode {
    #[cfg(unix)]
    default_sigpipe();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some("--version" | "-V") => {
            println!("stream3dm {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    if args.first().map(String::as_str) == Some("render") {
        return render(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("ifc") {
        return match &args[1..] {
            [path] => ifc(path, false),
            [path, flag] if flag == "--products" => ifc(path, true),
            _ => usage(),
        };
    }
    if args.first().map(String::as_str) == Some("native") {
        let mut records = None;
        let mut rest = Vec::new();
        let mut it = args[1..].iter();
        while let Some(a) = it.next() {
            if a == "--records" {
                match it.next().and_then(|v| v.parse().ok()) {
                    Some(n) => records = Some(n),
                    None => return usage(),
                }
            } else {
                rest.push(a.as_str());
            }
        }
        return match rest.as_slice() {
            [path] => native(path, None, records),
            [path, arena] => native(path, Some(arena), records),
            _ => usage(),
        };
    }
    let flags: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| a.starts_with("--"))
        .collect();
    let positional: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with("--"))
        .collect();
    match positional.as_slice() {
        ["inspect", path]
            if flags
                .iter()
                .all(|f| matches!(*f, "--json" | "--stats" | "--cadmpeg")) =>
        {
            inspect(
                path,
                flags.contains(&"--json"),
                flags.contains(&"--stats"),
                flags.contains(&"--cadmpeg"),
            )
        }
        ["context", path] if flags.is_empty() => context(path),
        ["fingerprint", path]
            if flags
                .iter()
                .all(|f| matches!(*f, "--names-clear" | "--pretty")) =>
        {
            fingerprint(
                path,
                flags.contains(&"--names-clear"),
                flags.contains(&"--pretty"),
            )
        }
        ["diff", before, after] if flags.iter().all(|f| *f == "--json") => {
            diff(before, after, flags.contains(&"--json"))
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Ends quietly when the reader of the output goes away (`stream3dm ifc … --products | head`), as
/// Unix tools do, instead of panicking on the broken pipe: Rust starts programs with SIGPIPE
/// ignored.
#[cfg(unix)]
fn default_sigpipe() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    // The same numbers on Linux, macOS and the BSDs.
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

/// `n` and the noun, singular for one.
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The key from STREAM3DM_FINGERPRINT_KEY (64 hex digits), so it stays out of shell history.
fn fingerprint_key() -> Result<Option<[u8; 32]>, String> {
    let Ok(text) = std::env::var("STREAM3DM_FINGERPRINT_KEY") else {
        return Ok(None);
    };
    let text = text.trim();
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("STREAM3DM_FINGERPRINT_KEY must be 64 hexadecimal digits".into());
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).expect("checked hex");
    }
    Ok(Some(key))
}

fn fingerprint(path: &str, clear_names: bool, pretty: bool) -> ExitCode {
    let key = match fingerprint_key() {
        Ok(key) => key,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if key.is_none() {
        eprintln!(
            "note: no STREAM3DM_FINGERPRINT_KEY, using the public key (anyone can test guesses against it)"
        );
    }
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let options = stream3dm::FingerprintOptions { key, clear_names };
    match stream3dm::fingerprint_reader(std::io::BufReader::new(file), Default::default(), &options)
    {
        Ok((_, fingerprint)) => {
            let json = if pretty {
                serde_json::to_string_pretty(&fingerprint)
            } else {
                serde_json::to_string(&fingerprint)
            };
            println!("{}", json.expect("fingerprint serializes"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{path}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn diff(before: &str, after: &str, json: bool) -> ExitCode {
    let load = |path: &str| -> Option<stream3dm::Fingerprint> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| eprintln!("{path}: {e}"))
            .ok()?;
        serde_json::from_str(&text)
            .map_err(|e| eprintln!("{path}: {e}"))
            .ok()
    };
    let (Some(a), Some(b)) = (load(before), load(after)) else {
        return ExitCode::FAILURE;
    };
    let d = match stream3dm::fingerprint::diff(&a, &b) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&d).expect("diff serializes")
        );
        return ExitCode::SUCCESS;
    }
    if d.same {
        println!("same version");
        return ExitCode::SUCCESS;
    }
    // Names when the fingerprints carry them, keys otherwise.
    let mut names = std::collections::HashMap::new();
    for f in [&a, &b] {
        for l in &f.layers {
            names.insert(l.key.clone(), l.name.clone());
        }
        for m in &f.materials {
            names.insert(m.key.clone(), m.name.clone());
        }
        for x in &f.blocks {
            names.insert(x.key.clone(), x.name.clone());
        }
        for v in &f.views {
            names.insert(v.key.clone(), v.name.clone());
        }
    }
    let label = |k: &String| names.get(k).cloned().flatten().unwrap_or_else(|| k.clone());
    let o = &d.objects;
    println!(
        "geometry {}",
        if d.same_geometry {
            "unchanged"
        } else {
            "changed"
        }
    );
    if !d.summary.is_empty() {
        println!("summary: {} changed", d.summary.join(", "));
    }
    let count = |moved: bool, distance: stream3dm::fingerprint::Distance| {
        o.changed
            .iter()
            .filter(|c| c.moved == moved && c.distance == distance)
            .count()
    };
    use stream3dm::fingerprint::Distance::{Beyond1m, Within0_1m, Within1m};
    println!(
        "objects: {} unchanged, {} added, {} removed, {} re-created ({} in place)",
        o.unchanged,
        o.added.len(),
        o.removed.len(),
        o.recreated.len(),
        o.recreated.iter().filter(|r| r.in_place).count()
    );
    println!(
        "  moved (same shape): {} within 0.1 m, {} within 1 m, {} beyond",
        count(true, Within0_1m),
        count(true, Within1m),
        count(true, Beyond1m)
    );
    println!(
        "  reshaped: {} within 0.1 m, {} within 1 m, {} beyond ({} with area changed 2x or more)",
        count(false, Within0_1m),
        count(false, Within1m),
        count(false, Beyond1m),
        o.changed.iter().filter(|c| c.area_changed).count()
    );
    println!(
        "  on another layer: {} (same geometry) + {} (changed)",
        o.moved_to_layer.len(),
        o.changed.iter().filter(|c| c.layer_changed).count()
    );
    for (title, t) in [
        ("layers", &d.layers),
        ("materials", &d.materials),
        ("blocks", &d.blocks),
        ("views", &d.views),
    ] {
        if t.added.is_empty() && t.removed.is_empty() && t.changed.is_empty() {
            continue;
        }
        let list = |v: &Vec<String>| v.iter().map(&label).collect::<Vec<_>>().join(", ");
        println!(
            "{title}: added [{}], removed [{}], changed [{}]",
            list(&t.added),
            list(&t.removed),
            list(&t.changed)
        );
    }
    ExitCode::SUCCESS
}

fn context(path: &str) -> ExitCode {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match stream3dm::inspect_reader(std::io::BufReader::new(file), Default::default()) {
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&stream3dm::context(&report))
                    .expect("context serializes")
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{path}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn render(args: &[String]) -> ExitCode {
    let mut options = stream3dm_render::RenderOptions::default();
    let (mut path, mut out) = (None, std::path::PathBuf::from("render"));
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" => match it.next() {
                Some(v) => out = v.into(),
                None => return usage(),
            },
            "--view" => match it.next() {
                Some(v) => options.view = Some(v.clone()),
                None => return usage(),
            },
            "--size" => match it.next().and_then(|v| v.split_once('x')) {
                Some((w, h)) => match (w.parse(), h.parse()) {
                    (Ok(w), Ok(h)) if w > 0 && h > 0 => {
                        options.width = w;
                        options.height = h;
                    }
                    _ => return usage(),
                },
                None => return usage(),
            },
            p if path.is_none() && !p.starts_with("--") => path = Some(p.to_string()),
            _ => return usage(),
        }
    }
    let Some(path) = path else { return usage() };
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let started = std::time::Instant::now();
    let renderer = std::rc::Rc::new(std::cell::RefCell::new(stream3dm_render::Renderer::new(
        options,
    )));
    let mut inspector =
        stream3dm::Inspector::with_sink(Default::default(), Box::new(renderer.clone()));
    if let Err(e) = stream3dm::inspect_reader_with(std::io::BufReader::new(file), &mut inspector) {
        eprintln!("{path}: {e}");
        return ExitCode::FAILURE;
    }
    let renderer = std::rc::Rc::try_unwrap(renderer)
        .ok()
        .expect("the inspector released the renderer")
        .into_inner();
    let triangles = renderer.triangles();
    let passes = renderer.finish();
    if passes.is_empty() {
        eprintln!("{path}: no view to render (the file has no usable named views or viewports)");
        return ExitCode::FAILURE;
    }
    let stem = std::path::Path::new(&path)
        .file_stem()
        .map_or("model".into(), |s| s.to_string_lossy().to_string());
    for p in &passes {
        let safe: String = p
            .view
            .name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let dir = out
            .join(&stem)
            .join(if safe.is_empty() { "view".into() } else { safe });
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("{}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
        for (name, image) in p.images() {
            if let Err(e) = image.write_png(&dir.join(format!("{name}.png"))) {
                eprintln!("{}: {e}", dir.display());
                return ExitCode::FAILURE;
            }
        }
        let covered = 100.0 * p.coverage as f64 / f64::from(p.clay.width * p.clay.height);
        println!(
            "{} → {} ({covered:.0}% covered)",
            p.view.name,
            dir.display()
        );
    }
    println!(
        "{}, {}, {:.2?}",
        count(triangles as usize, "triangle", "triangles"),
        count(passes.len(), "view", "views"),
        started.elapsed()
    );
    ExitCode::SUCCESS
}

/// Reads an IFC file's metadata in 1 MiB slices: a summary with the speed and peak heap, or
/// every product as a JSON line.
fn ifc(path: &str, products: bool) -> ExitCode {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let started = std::time::Instant::now();
    let baseline = IN_USE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let mut reader = stream3dm::IfcInspector::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut bytes = 0u64;
    loop {
        let n = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                eprintln!("{path}: {e}");
                return ExitCode::FAILURE;
            }
        };
        bytes += n as u64;
        if let Err(e) = reader.push(&buffer[..n]) {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    }
    if let Err(e) = reader.prepare() {
        eprintln!("{path}: {e}");
        return ExitCode::FAILURE;
    }
    let seconds = started.elapsed().as_secs_f64();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    let list = reader.products();
    if products {
        for p in &list {
            println!("{}", serde_json::to_string(p).expect("product serializes"));
        }
    } else {
        println!(
            "{path}: {}, {:.1} MB in {seconds:.3} s ({:.0} MB/s), peak heap {:.1} MB",
            count(list.len(), "product", "products"),
            bytes as f64 / 1e6,
            bytes as f64 / 1e6 / seconds,
            peak as f64 / 1e6
        );
    }
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn read(path: &str) -> Option<Vec<u8>> {
    std::fs::read(path)
        .map_err(|e| eprintln!("{path}: {e}"))
        .ok()
}

fn inspect(path: &str, json: bool, stats: bool, cadmpeg_only: bool) -> ExitCode {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let started = std::time::Instant::now();
    let baseline = IN_USE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let mut inspector = stream3dm::Inspector::new(stream3dm::InspectOptions {
        cadmpeg_only,
        ..Default::default()
    });
    let result = stream3dm::inspect_reader_with(std::io::BufReader::new(file), &mut inspector);
    if stats {
        let elapsed = started.elapsed().as_secs_f64();
        let size = std::fs::metadata(path).map_or(0, |m| m.len()) as f64 / 1e6;
        let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline) as f64 / 1e6;
        eprintln!(
            "stats: {size:.1} MB in {elapsed:.3} s ({:.1} MB/s), peak heap {peak:.1} MB ({:.2}x file)",
            size / elapsed.max(1e-9),
            peak / size.max(1e-9)
        );
        let r = inspector.reader_stats();
        eprintln!(
            "records: {} by our reader, {} delegated to cadmpeg, {} failed in ours (per pass)",
            r.own, r.delegated, r.failed
        );
    }
    match result {
        Ok(report) if json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("report serializes")
            );
            ExitCode::SUCCESS
        }
        Ok(report) => {
            print_summary(path, &report);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{path}: {e}");
            if e == stream3dm::InspectError::NotRhinoFile && path.to_lowercase().ends_with(".ifc") {
                eprintln!("an IFC file is read with `stream3dm ifc {path}`");
            }
            ExitCode::FAILURE
        }
    }
}

fn print_summary(path: &str, r: &Report) {
    let f = &r.file;
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let version = f.archive_version.map_or("?".to_string(), |v| v.to_string());
    let app = f.application.as_deref().unwrap_or("unknown application");
    println!(
        "{name}: archive {version}, {app}, {}, {} KB",
        f.unit_system,
        f.size_bytes.div_ceil(1024)
    );

    if let Some(p) = &r.proportions {
        let [x, y, z] = p.size;
        let mut line = format!("size      {x} × {y} × {z} m");
        if let Some(a) = p.plan_aspect {
            line += &format!(", plan aspect {a}");
        }
        if let Some(h) = p.height_to_plan {
            line += &format!(", height/plan {h}");
        }
        println!("{line}");
        if let Some(s) = &p.storeys {
            println!(
                "storeys   {} × {} m (levels {:?})",
                s.count, s.typical_height, s.levels
            );
        }
    }

    let i = &r.inventory;
    let parts = [
        ("breps", i.breps),
        ("meshes", i.meshes),
        ("subds", i.subds),
        ("curves", i.curves),
        ("points", i.points),
        ("annotations", i.annotations),
        ("hatches", i.hatches),
        ("block instances", i.block_instances),
        ("lights", i.lights),
        ("other", i.other),
        ("undecoded", i.undecoded),
    ];
    let listed: Vec<String> = parts
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(k, n)| format!("{k} {n}"))
        .collect();
    println!("objects   {} ({})", i.objects, listed.join(", "));
    if i.breps > 0 {
        println!(
            "          {} of {} breps have a render mesh, {} tessellated here",
            i.breps_with_render_mesh, i.breps, i.breps_tessellated
        );
    }
    println!(
        "mesh      {} vertices, {} triangles",
        i.mesh_vertices, i.mesh_triangles
    );

    println!("layers    {}", r.structure.layers.len());
    for l in &r.structure.layers {
        let mut line = format!("  {} — {}", l.path, count(l.objects, "object", "objects"));
        if let Some(m) = &l.material {
            line += &format!(", material {m}");
        }
        if !l.visible {
            line += ", hidden";
        }
        println!("{line}");
    }
    if !r.materials.is_empty() {
        let names: Vec<&str> = r
            .materials
            .iter()
            .map(|m| m.name.as_str())
            .take(12)
            .collect();
        let more = r.materials.len().saturating_sub(names.len());
        println!(
            "materials {}: {}{}",
            r.materials.len(),
            names.join(", "),
            if more > 0 {
                format!(", … +{more}")
            } else {
                String::new()
            }
        );
    }
    if !r.blocks.is_empty() {
        for b in &r.blocks {
            println!(
                "block     {} — {}, {}",
                b.name,
                count(b.placements, "placement", "placements"),
                count(b.members, "member", "members")
            );
        }
    }
    for v in &r.views {
        let kind = match v.kind {
            ViewKind::Viewport => "viewport",
            ViewKind::Named => "named",
        };
        let projection = match v.projection {
            Projection::Parallel => "parallel".to_string(),
            Projection::Perspective => v
                .lens_35mm
                .map_or("perspective".to_string(), |l| format!("perspective {l} mm")),
            Projection::Other => "other".to_string(),
        };
        println!("view      {} ({kind}, {projection})", v.name);
    }
    if !r.structure.user_strings.is_empty() {
        let keys: Vec<&str> = r
            .structure
            .user_strings
            .iter()
            .map(|u| u.key.as_str())
            .collect();
        println!("user text {}", keys.join(", "));
    }

    let s = &r.style;
    let fmt = |v: Option<f64>| v.map_or("—".to_string(), |v| v.to_string());
    println!(
        "style     orthogonality {}, planar faces {}, glazing {}, horizontal {}, vertical {}",
        fmt(s.orthogonality),
        fmt(s.planar_face_share),
        fmt(s.glazing_area_share),
        fmt(s.horizontal_area_share),
        fmt(s.vertical_area_share)
    );
    for d in &r.diagnostics {
        let severity = match d.severity {
            Severity::Info => "info",
            Severity::Warning => "warning",
            _ => "note",
        };
        let count = d.count.map_or(String::new(), |n| format!(" ({n})"));
        println!("{severity:<9} {}{count}", d.message);
    }
}

/// Debugging aid: lists the decoder's native Rhino arenas, or prints one arena's records as JSON.
fn native(path: &str, arena: Option<&str>, records: Option<usize>) -> ExitCode {
    let Some(mut bytes) = read(path) else {
        return ExitCode::FAILURE;
    };
    if let Some(n) = records {
        bytes = match stream3dm::sample_file(&bytes, n) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{path}: {e}");
                return ExitCode::FAILURE;
            }
        };
    }
    let decoded = match RhinoCodec.decode(&mut Cursor::new(bytes), &DecodeOptions::default()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(ns) = decoded.ir().native.namespace("rhino") else {
        eprintln!("{path}: no rhino namespace");
        return ExitCode::FAILURE;
    };
    match arena {
        None => {
            for (name, records) in ns.arenas() {
                println!("{name}\t{}", records.len());
            }
        }
        Some(name) => {
            let records: Vec<_> = ns
                .arenas()
                .get(name)
                .into_iter()
                .flatten()
                .map(|r| r.fields())
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&records).expect("records serialize")
            );
        }
    }
    ExitCode::SUCCESS
}
