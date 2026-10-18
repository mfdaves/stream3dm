//! Class UUIDs of the objects the reader handles, including the legacy aliases Rhino writes
//! itself (`TL_Brep` for most Breps, `TL_NurbsCurve`, …).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClassKind {
    Mesh,
    Brep,
    Extrusion,
    InstanceRef,
    PlaneSurface,
    /// Other surfaces (NURBS, revolution, sum).
    Surface,
    Curve,
    Point,
    PointCloud,
    SubD,
    TextDot,
    Hatch,
    /// Text, leaders and dimensions of every generation.
    Annotation,
    /// Layout details and clipping planes: not model geometry.
    Other,
}

/// Canonical UUID text to the stored byte order: `u32`, `u16`, `u16` little-endian, then
/// eight bytes as written.
const fn wire(text: &str) -> [u8; 16] {
    const fn hex(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("not a hex digit"),
        }
    }
    let t = text.as_bytes();
    let mut digits = [0u8; 32];
    let (mut i, mut n) = (0, 0);
    while i < t.len() {
        if t[i] != b'-' {
            digits[n] = hex(t[i]);
            n += 1;
        }
        i += 1;
    }
    assert!(n == 32, "a UUID has 32 hex digits");
    let mut canonical = [0u8; 16];
    let mut k = 0;
    while k < 16 {
        canonical[k] = digits[2 * k] << 4 | digits[2 * k + 1];
        k += 1;
    }
    let c = canonical;
    [
        c[3], c[2], c[1], c[0], c[5], c[4], c[7], c[6], c[8], c[9], c[10], c[11], c[12], c[13],
        c[14], c[15],
    ]
}

const CLASSES: &[(&str, ClassKind)] = &[
    ("4ed7d4e4-e947-11d3-bfe5-0010830122f0", ClassKind::Mesh),
    ("60b5dbc5-e660-11d3-bfe4-0010830122f0", ClassKind::Brep),
    ("f06fc243-a32a-4608-9dd8-a7d2c4ce2a36", ClassKind::Brep), // TL_Brep
    ("0705fdef-3e2a-11d4-800e-0010830122f0", ClassKind::Brep), // old trimmed surface
    ("2d4cfedb-3e2a-11d4-800e-0010830122f0", ClassKind::Brep), // old b-rep
    ("36f53175-72b8-4d47-bf1f-b4e6fc24f4b9", ClassKind::Extrusion),
    (
        "f9cfb638-b9d4-4340-87e3-c56e7865d96a",
        ClassKind::InstanceRef,
    ),
    (
        "4ed7d4df-e947-11d3-bfe5-0010830122f0",
        ClassKind::PlaneSurface,
    ),
    ("4ed7d4de-e947-11d3-bfe5-0010830122f0", ClassKind::Surface), // NURBS
    ("4760c817-0be3-11d4-bffe-0010830122f0", ClassKind::Surface), // TL_NurbsSurface
    ("fa4fd4b5-1613-11d4-8000-0010830122f0", ClassKind::Surface), // old NURBS surface
    ("a16220d3-163b-11d4-8000-0010830122f0", ClassKind::Surface), // revolution
    ("0a8401b6-4d34-4b99-8615-1b4e723dc4e5", ClassKind::Surface), // TL_RevSurface
    ("c4cd5359-446d-4690-9ff5-29059732472b", ClassKind::Surface), // sum
    ("665f6331-2a66-4cce-81d0-b5eebd9b5417", ClassKind::Surface), // TL_SumSurface
    ("4ed7d4dd-e947-11d3-bfe5-0010830122f0", ClassKind::Curve),   // NURBS
    ("5eaf1119-0b51-11d4-bffe-0010830122f0", ClassKind::Curve),   // TL_NurbsCurve
    ("76a709d5-1550-11d4-8000-0010830122f0", ClassKind::Curve),   // old NURBS curve
    ("4ed7d4e6-e947-11d3-bfe5-0010830122f0", ClassKind::Curve),   // polyline
    ("4ed7d4db-e947-11d3-bfe5-0010830122f0", ClassKind::Curve),   // line
    ("cf33be2a-09b4-11d4-bffb-0010830122f0", ClassKind::Curve),   // arc
    ("4ed7d4e0-e947-11d3-bfe5-0010830122f0", ClassKind::Curve),   // polycurve
    ("ef638317-154b-11d4-8000-0010830122f0", ClassKind::Curve),   // old polycurve
    ("c3101a1d-f157-11d3-bfe7-0010830122f0", ClassKind::Point),
    (
        "2488f347-f8fa-11d3-bfec-0010830122f0",
        ClassKind::PointCloud,
    ),
    ("f09ba4d9-455b-42c3-ba3b-e6ccacef853b", ClassKind::SubD),
    ("74198302-cdf4-4f95-9609-6d684f22ab37", ClassKind::TextDot),
    ("0559733b-5332-49d1-a936-0532ac76ade5", ClassKind::Hatch),
    (
        "57376349-62a9-4a16-b411-a46bcd544790",
        ClassKind::Annotation,
    ), // text
    (
        "945bf594-6ff9-4f5c-bfc0-b3af528f29d2",
        ClassKind::Annotation,
    ), // leader
    (
        "e550882b-f44d-4154-a1ef-6e50cbbbf543",
        ClassKind::Annotation,
    ), // linear dimension
    (
        "d417786b-f6cd-4f12-9e1f-063f414dbeb6",
        ClassKind::Annotation,
    ), // angular dimension
    (
        "fc749c2f-4c00-41fd-9840-26d94f047ad3",
        ClassKind::Annotation,
    ), // radial dimension
    (
        "03124828-4c9b-4d28-9a82-664ddde7a14f",
        ClassKind::Annotation,
    ), // ordinate dimension
    (
        "d46767ba-7e8f-4d9d-9a92-66050219a5b9",
        ClassKind::Annotation,
    ), // centre mark
    (
        "46f75541-f46b-48be-aa7e-b353bbe068a7",
        ClassKind::Annotation,
    ), // V5 text
    (
        "14922b7a-5b65-4f11-8345-d415a9637129",
        ClassKind::Annotation,
    ), // V5 leader
    (
        "bd57f33b-a1b2-46e9-9c6e-af09d30ffdde",
        ClassKind::Annotation,
    ), // V5 linear dimension
    (
        "b2b683fc-7964-4e96-b1f9-9b356a76b08b",
        ClassKind::Annotation,
    ), // V5 radial dimension
    (
        "841bc40b-a971-4a8e-94e5-bba26d67348e",
        ClassKind::Annotation,
    ), // V5 angular dimension
    (
        "c8288d69-5bd8-4f50-9baf-525a0086b0c3",
        ClassKind::Annotation,
    ), // V5 ordinate dimension
    (
        "abaf5873-4145-11d4-800f-0010830122f0",
        ClassKind::Annotation,
    ), // V2 annotation
    (
        "5de6b20d-486b-11d4-8014-0010830122f0",
        ClassKind::Annotation,
    ), // V2 linear dimension
    (
        "5de6b20e-486b-11d4-8014-0010830122f0",
        ClassKind::Annotation,
    ), // V2 radial dimension
    (
        "5de6b20f-486b-11d4-8014-0010830122f0",
        ClassKind::Annotation,
    ), // V2 angular dimension
    (
        "5de6b210-486b-11d4-8014-0010830122f0",
        ClassKind::Annotation,
    ), // V2 text
    (
        "5de6b211-486b-11d4-8014-0010830122f0",
        ClassKind::Annotation,
    ), // V2 leader
    (
        "8bd94e19-59e1-11d4-8018-0010830122f0",
        ClassKind::Annotation,
    ), // V2 text dot
    // V2 arrow: openNURBS reads it as a line curve (tail to head), so it is a curve here too.
    ("8bd94e1a-59e1-11d4-8018-0010830122f0", ClassKind::Curve),
    ("c8c66efa-b3cb-4e00-9440-2ad66203379e", ClassKind::Other), // layout detail
    ("dbc5a584-ce3f-4170-98a8-497069ca5c36", ClassKind::Other), // clipping plane
];

const fn table() -> [([u8; 16], ClassKind); CLASSES.len()] {
    let mut out = [([0u8; 16], ClassKind::Mesh); CLASSES.len()];
    let mut i = 0;
    while i < CLASSES.len() {
        out[i] = (wire(CLASSES[i].0), CLASSES[i].1);
        i += 1;
    }
    out
}

const TABLE: [([u8; 16], ClassKind); CLASSES.len()] = table();

pub(crate) fn kind(class: &[u8; 16]) -> Option<ClassKind> {
    TABLE.iter().find(|(id, _)| id == class).map(|&(_, k)| k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_byte_order() {
        // ON_NurbsCurve as it appears in files.
        assert_eq!(
            wire("4ED7D4DD-E947-11D3-BFE5-0010830122F0"),
            [
                0xdd, 0xd4, 0xd7, 0x4e, 0x47, 0xe9, 0xd3, 0x11, 0xbf, 0xe5, 0x00, 0x10, 0x83, 0x01,
                0x22, 0xf0
            ]
        );
        assert_eq!(
            crate::stream::frame::INSTANCE_REF_CLASS,
            wire("f9cfb638-b9d4-4340-87e3-c56e7865d96a")
        );
    }
}
