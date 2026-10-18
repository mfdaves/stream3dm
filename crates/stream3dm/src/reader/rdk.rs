//! Rhino's render settings that light a model: the Sun panel and the ground plane. Rhino keeps
//! them in the document's RDK data, an XML text in a user table after the object table, under
//! the RDK plug-in's id (openNURBS `ONX_ModelPrivate::GetEntireRDKDocument`, `ON_Sun`,
//! `ON_GroundPlane`):
//!
//! ```text
//! user table chunk 0x10000017
//!   0x20008080  uuid (16 bytes), a record header chunk, CRC
//!   0x20000081  i32 version; 1: a UTF-16 string; 3 or 4: i32 length, that many UTF-8 bytes
//!               (4 then lists embedded files, not read)
//! ```
//!
//! In the XML, `render-content-manager-document/settings/sun` and `…/ground-plane` hold one
//! element per value (`<year type="int">2026</year>`). Place and north are absent from the sun
//! when the document has an earth anchor, which then holds them.

use super::cursor::Cursor;
use crate::stream::frame::{TCODE_SHORT, Width};

/// The RDK plug-in's id (16592D58-4A2F-401D-BF5E-3B87741C1B1B) as the file stores it.
const RDK_PLUG_IN: [u8; 16] = [
    0x58, 0x2d, 0x59, 0x16, 0x2f, 0x4a, 0x1d, 0x40, 0xbf, 0x5e, 0x3b, 0x87, 0x74, 0x1c, 0x1b, 0x1b,
];
pub(crate) const TCODE_USER_TABLE: u32 = 0x1000_0017;
const TCODE_USER_TABLE_UUID: u32 = 0x2000_8080;
const TCODE_USER_RECORD: u32 = 0x2000_0081;
/// The XML is not read beyond this size (it holds every render content of the document).
const MAX_XML: usize = 64 << 20;

/// The Sun panel as saved.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SunRec {
    pub enabled: bool,
    pub manual: bool,
    pub azimuth: f64,
    pub altitude: f64,
    pub year: i32,
    pub month: i32,
    pub day: i32,
    pub hours: f64,
    pub timezone: f64,
    pub daylight_saving: Option<i32>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub north: Option<f64>,
    pub intensity: f64,
    pub skylight: bool,
}

/// The ground plane as saved (altitude in model units).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GroundRec {
    pub enabled: bool,
    pub altitude: f64,
    pub auto_altitude: bool,
    pub shadow_only: bool,
    pub show_underside: bool,
}

/// The sun and ground plane of a user table chunk (header included), when it is the RDK's.
pub(crate) fn render_settings(
    chunk: &[u8],
    width: Width,
) -> Option<(Option<SunRec>, Option<GroundRec>)> {
    let mut c = Cursor::new(chunk);
    let (typecode, value) = c.chunk_header(width).ok()?;
    if typecode & !crate::stream::frame::TCODE_CRC != TCODE_USER_TABLE
        || typecode & TCODE_SHORT != 0
    {
        return None;
    }
    let mut table = c.long_body(0, typecode, value).ok()?;
    let mut ours = false;
    while !table.is_empty() {
        let at = table.position();
        let (typecode, value) = table.chunk_header(width).ok()?;
        if typecode & TCODE_SHORT != 0 {
            continue;
        }
        let mut body = table.long_body(at, typecode, value).ok()?;
        match typecode {
            TCODE_USER_TABLE_UUID => ours = body.uuid().ok()? == RDK_PLUG_IN,
            TCODE_USER_RECORD if ours => {
                let xml = document_xml(&mut body)?;
                return Some((sun(&xml), ground(&xml)));
            }
            _ => {}
        }
    }
    None
}

/// The record's XML text.
fn document_xml(c: &mut Cursor) -> Option<String> {
    match c.i32().ok()? {
        1 => c.wstring().ok(),
        3 | 4 => {
            let len = usize::try_from(c.i32().ok()?).ok()?;
            if len == 0 || len > MAX_XML {
                return None;
            }
            let bytes = c.bytes(len).ok()?;
            // A trailing NUL is part of the count in some writers.
            let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
        _ => None,
    }
}

/// The text inside the first `<tag …>…</tag>` of `xml` (self-closing elements have none and are
/// passed over).
fn element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(i) = xml[from..].find('<').map(|i| from + i) {
        let rest = &xml[i + 1..];
        from = i + 1;
        let Some(name) = rest.strip_prefix(tag) else {
            continue;
        };
        let end = name.find('>')?;
        // The tag name must end here (not a longer name sharing the prefix).
        if !name.starts_with(['>', ' ', '/', '\t', '\r', '\n']) {
            continue;
        }
        if name[..end].ends_with('/') {
            // Self-closing: no text; look further.
            from = i + 1 + tag.len() + end;
            continue;
        }
        let inner = &name[end + 1..];
        let close = format!("</{tag}>");
        return inner.find(&close).map(|j| &inner[..j]);
    }
    None
}

/// The section `path` (element names) of the document.
fn section<'a>(xml: &'a str, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(xml, |inner, tag| element(inner, tag))
}

fn value<T: std::str::FromStr>(section: &str, tag: &str) -> Option<T> {
    element(section, tag)?.trim().parse().ok()
}

fn flag(section: &str, tag: &str) -> Option<bool> {
    match element(section, tag)?.trim() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|v| v.is_finite())
}

fn sun(xml: &str) -> Option<SunRec> {
    let s = section(xml, &["render-content-manager-document", "settings", "sun"])?;
    Some(SunRec {
        enabled: flag(s, "enable-on").unwrap_or(false),
        manual: flag(s, "manual-control-on").unwrap_or(false),
        azimuth: finite(value(s, "sun-azimuth")).unwrap_or(0.0),
        altitude: finite(value(s, "sun-altitude")).unwrap_or(0.0),
        year: value(s, "year").unwrap_or(2000),
        month: value::<i32>(s, "month").unwrap_or(1).clamp(1, 12),
        day: value::<i32>(s, "day").unwrap_or(1).clamp(1, 31),
        hours: finite(value(s, "time")).unwrap_or(12.0).clamp(0.0, 24.0),
        timezone: finite(value(s, "observer-timezone")).unwrap_or(0.0),
        daylight_saving: flag(s, "daylight-saving-on")
            .unwrap_or(false)
            .then(|| value(s, "daylight-saving-minutes").unwrap_or(60)),
        latitude: finite(value(s, "observer-latitude")),
        longitude: finite(value(s, "observer-longitude")),
        north: finite(value(s, "north")),
        intensity: finite(value(s, "intensity")).unwrap_or(1.0),
        skylight: flag(s, "skylight-on").unwrap_or(true),
    })
}

fn ground(xml: &str) -> Option<GroundRec> {
    let g = section(
        xml,
        &[
            "render-content-manager-document",
            "settings",
            "ground-plane",
        ],
    )?;
    Some(GroundRec {
        enabled: flag(g, "on").unwrap_or(false),
        altitude: finite(value(g, "altitude")).unwrap_or(0.0),
        auto_altitude: flag(g, "auto-altitude").unwrap_or(false),
        shadow_only: flag(g, "shadow-only").unwrap_or(false),
        show_underside: flag(g, "show-underside").unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<xml>
	<render-content-manager-document>
		<settings>
			<sun>
				<enable-on type="bool">true</enable-on>
				<manual-control-on type="bool">false</manual-control-on>
				<daylight-saving-on type="bool">true</daylight-saving-on>
				<daylight-saving-minutes type="int">60</daylight-saving-minutes>
				<year type="int">2026</year>
				<month type="int">6</month>
				<day type="int">21</day>
				<time type="double">16.5</time>
				<sun-azimuth type="double">250.5</sun-azimuth>
				<sun-altitude type="double">40.25</sun-altitude>
				<observer-latitude type="double">45.4642</observer-latitude>
				<observer-longitude type="double">9.19</observer-longitude>
				<observer-timezone type="double">1</observer-timezone>
				<north type="double">110</north>
				<skylight-custom-environment type="uuid">00000000-0000-0000-0000-000000000000</skylight-custom-environment>
			</sun>
			<ground-plane>
				<on type="bool">true</on>
				<altitude type="double">-1.5</altitude>
				<auto-altitude type="bool">false</auto-altitude>
				<shadow-only type="bool">true</shadow-only>
				<material type="string"/>
			</ground-plane>
		</settings>
	</render-content-manager-document>
</xml>"#;

    /// A user table chunk (four-byte lengths) holding `xml` as an RDK record of `version`.
    fn table(xml: &str, plug_in: [u8; 16]) -> Vec<u8> {
        let mut uuid = plug_in.to_vec();
        uuid.extend([0u8; 4]); // CRC
        let mut record = 4i32.to_le_bytes().to_vec();
        record.extend((xml.len() as i32).to_le_bytes());
        record.extend(xml.as_bytes());
        let mut body = Vec::new();
        for (tc, b) in [(TCODE_USER_TABLE_UUID, uuid), (TCODE_USER_RECORD, record)] {
            body.extend(tc.to_le_bytes());
            body.extend((b.len() as u32).to_le_bytes());
            body.extend(b);
        }
        let mut chunk = TCODE_USER_TABLE.to_le_bytes().to_vec();
        chunk.extend((body.len() as u32).to_le_bytes());
        chunk.extend(body);
        chunk
    }

    #[test]
    fn reads_the_sun_and_the_ground_plane() {
        let (sun, ground) = render_settings(&table(XML, RDK_PLUG_IN), Width::Four).unwrap();
        let sun = sun.unwrap();
        assert!(sun.enabled && !sun.manual);
        assert_eq!(
            (sun.year, sun.month, sun.day, sun.hours),
            (2026, 6, 21, 16.5)
        );
        assert_eq!((sun.azimuth, sun.altitude), (250.5, 40.25));
        assert_eq!(
            (sun.latitude, sun.longitude, sun.timezone),
            (Some(45.4642), Some(9.19), 1.0)
        );
        assert_eq!(
            (sun.daylight_saving, sun.north, sun.intensity, sun.skylight),
            (Some(60), Some(110.0), 1.0, true)
        );
        let ground = ground.unwrap();
        assert!(
            ground.enabled && ground.shadow_only && !ground.auto_altitude && !ground.show_underside
        );
        assert_eq!(ground.altitude, -1.5);
    }

    #[test]
    fn other_plug_ins_and_damaged_tables_give_nothing() {
        assert_eq!(render_settings(&table(XML, [7; 16]), Width::Four), None);
        let whole = table(XML, RDK_PLUG_IN);
        for cut in [0, 5, 12, 40, whole.len() / 2, whole.len() - 1] {
            // No panic on any truncation; a cut inside the XML still reads what is there.
            let _ = render_settings(&whole[..cut], Width::Four);
        }
        let broken = XML.replace("</sun>", "").replace("16.5", "late");
        let (sun, _) = render_settings(&table(&broken, RDK_PLUG_IN), Width::Four).unwrap();
        assert_eq!(sun, None);
    }

    #[test]
    fn element_names_must_match_whole() {
        let s = "<sun-azimuth>1</sun-azimuth><sun>2</sun>";
        assert_eq!(element(s, "sun"), Some("2"));
        assert_eq!(element("<a/><a>3</a>", "a"), Some("3"));
        assert_eq!(element("<a type=\"string\"/>", "a"), None);
    }
}
