use super::*;
use crate::{Pick, PlacedMesh};
use std::cell::RefCell;
use std::rc::Rc;

/// A small IFC4 model in millimetres: two storeys, a wall with a layered material, a type and
/// property sets, a door on the upper storey, a stair whose flight is part of it, a product the
/// file leaves on no storey.
const MODEL: &str = r#"ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [ReferenceView]'),'2;1');
FILE_NAME('model.ifc','2026-10-16T12:00:00',(''),(''),'test exporter','Example CAD 2026','');
FILE_SCHEMA(('IFC4'));
ENDSEC;
DATA;
#1=IFCPROJECT('0YvctVUKr0kugbFTf53O9L',$,'Project',$,$,$,$,(#2),#3);
#3=IFCUNITASSIGNMENT((#4,#5));
#4=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);
#5=IFCSIUNIT(*,.AREAUNIT.,$,.SQUARE_METRE.);
#10=IFCSITE('1YvctVUKr0kugbFTf53O9L',$,'Site',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#11=IFCBUILDING('2YvctVUKr0kugbFTf53O9L',$,'House',$,$,$,$,$,.ELEMENT.,$,$,$);
#12=IFCBUILDINGSTOREY('3YvctVUKr0kugbFTf53O9L',$,'Level 1',$,$,$,$,$,.ELEMENT.,3000.);
#13=IFCBUILDINGSTOREY('4YvctVUKr0kugbFTf53O9L',$,'Level 0',$,$,$,$,$,.ELEMENT.,0.);
#14=IFCRELAGGREGATES('5YvctVUKr0kugbFTf53O9L',$,$,$,#1,(#10));
#15=IFCRELAGGREGATES('6YvctVUKr0kugbFTf53O9L',$,$,$,#10,(#11));
#16=IFCRELAGGREGATES('7YvctVUKr0kugbFTf53O9L',$,$,$,#11,(#12,#13));
#20=IFCWALL('8YvctVUKr0kugbFTf53O9L',$,'Wall south',$,'Exterior',#90,#91,'W-01',.STANDARD.);
#21=IFCDOOR('9YvctVUKr0kugbFTf53O9L',$,'Door \X2\00E9\X0\t\X2\00E9\X0\',$,$,#90,#92,$,2100.,900.,.DOOR.,$,$);
#22=IFCSTAIR('AYvctVUKr0kugbFTf53O9L',$,'Stair',$,$,#90,$,$,.STRAIGHT_RUN_STAIR.);
#23=IFCSTAIRFLIGHT('BYvctVUKr0kugbFTf53O9L',$,'Flight',$,$,#90,#93,$,10,11,0.17,0.28,.STRAIGHT.);
#24=IFCBUILDINGELEMENTPROXY('CYvctVUKr0kugbFTf53O9L',$,'Loose',$,$,#90,#94,$,$);
#30=IFCRELCONTAINEDINSPATIALSTRUCTURE('DYvctVUKr0kugbFTf53O9L',$,$,$,(#20,#22),#13);
#31=IFCRELCONTAINEDINSPATIALSTRUCTURE('EYvctVUKr0kugbFTf53O9L',$,$,$,(#21),#12);
#32=IFCRELAGGREGATES('FYvctVUKr0kugbFTf53O9L',$,$,$,#22,(#23));
#40=IFCWALLTYPE('GYvctVUKr0kugbFTf53O9L',$,'Basic Wall 215',$,$,(#41),$,$,$,.STANDARD.);
#41=IFCPROPERTYSET('HYvctVUKr0kugbFTf53O9L',$,'Pset_WallCommon',$,(#42,#43));
#42=IFCPROPERTYSINGLEVALUE('FireRating',$,IFCLABEL('REI30'),$);
#43=IFCPROPERTYSINGLEVALUE('IsExternal',$,IFCBOOLEAN(.F.),$);
#44=IFCRELDEFINESBYTYPE('IYvctVUKr0kugbFTf53O9L',$,$,$,(#20),#40);
#50=IFCPROPERTYSET('JYvctVUKr0kugbFTf53O9L',$,'Pset_WallCommon',$,(#51,#52));
#51=IFCPROPERTYSINGLEVALUE('IsExternal',$,IFCBOOLEAN(.T.),$);
#52=IFCPROPERTYSINGLEVALUE('FireRating',$,IFCLABEL('REI60'),$);
#53=IFCRELDEFINESBYPROPERTIES('KYvctVUKr0kugbFTf53O9L',$,$,$,(#20),#50);
#54=IFCELEMENTQUANTITY('LYvctVUKr0kugbFTf53O9L',$,'Qto_WallBaseQuantities',$,$,(#55));
#55=IFCQUANTITYLENGTH('Width',$,$,215.,$);
#56=IFCRELDEFINESBYPROPERTIES('MYvctVUKr0kugbFTf53O9L',$,$,$,(#20),#54);
#60=IFCMATERIAL('Concrete',$,$);
#61=IFCMATERIAL('Plaster',$,$);
#62=IFCMATERIALLAYER(#60,200.,$,$,$,$,$);
#63=IFCMATERIALLAYER(#61,15.,$,$,$,$,$);
#64=IFCMATERIALLAYERSET((#62,#63),'Wall 215',$);
#65=IFCMATERIALLAYERSETUSAGE(#64,.AXIS2.,.POSITIVE.,0.,$);
#66=IFCRELASSOCIATESMATERIAL('NYvctVUKr0kugbFTf53O9L',$,$,$,(#20),#65);
#67=IFCMATERIAL('Oak',$,$);
#68=IFCRELASSOCIATESMATERIAL('OYvctVUKr0kugbFTf53O9L',$,$,$,(#21),#67);
/* geometry is the engine's: placements and representations are only referenced */
#90=IFCLOCALPLACEMENT($,#95);
#91=IFCPRODUCTDEFINITIONSHAPE($,$,());
#92=IFCPRODUCTDEFINITIONSHAPE($,$,());
#93=IFCPRODUCTDEFINITIONSHAPE($,$,());
#94=IFCPRODUCTDEFINITIONSHAPE($,$,());
ENDSEC;
END-ISO-10303-21;
"#;

/// A pick as the sink saw it: id, class, type, name, user strings.
type SeenPick = (
    u64,
    String,
    Option<String>,
    Option<String>,
    Vec<(String, String)>,
);

#[derive(Default)]
struct Seen {
    picks: Vec<SeenPick>,
    meshes: Vec<(u64, Option<usize>, Option<[u8; 3]>)>,
    layers: Vec<String>,
}

impl MeshSink for Seen {
    fn document(&mut self, d: &crate::DocumentInfo) {
        self.layers = d.layers.iter().map(|l| l.path.clone()).collect();
    }
    fn mesh(&mut self, m: &PlacedMesh) {
        self.meshes.push((m.pick, m.layer, m.color));
    }
    fn pick(&mut self, p: &Pick) {
        self.picks.push((
            p.pick,
            p.class.unwrap_or("").to_string(),
            p.block.map(str::to_string),
            p.name.map(str::to_string),
            p.user_strings.to_vec(),
        ));
    }
}

/// A unit square at height `z` (metres).
fn square(z: f64) -> (Vec<f64>, Vec<u32>) {
    (
        vec![0.0, 0.0, z, 1.0, 0.0, z, 1.0, 1.0, z, 0.0, 1.0, z],
        vec![0, 1, 2, 0, 2, 3],
    )
}

fn read(slice: usize) -> (Report, Rc<RefCell<Seen>>) {
    let seen = Rc::new(RefCell::new(Seen::default()));
    let mut ifc = IfcInspector::with_sink(Box::new(seen.clone()));
    for part in MODEL.as_bytes().chunks(slice) {
        ifc.push(part).unwrap();
    }
    assert_eq!(
        ifc.product_count(),
        4,
        "wall, door, flight, proxy (the stair has no representation)"
    );
    ifc.prepare().unwrap();
    for (id, z, color) in [
        (20, 0.0, Some([200, 200, 200])),
        (21, 3.0, None),
        (23, 0.5, None),
        (24, 0.0, None),
        (999, 0.0, None),
    ] {
        let (p, i) = square(z);
        ifc.add_product(
            id,
            &[ProductPart {
                positions: &p,
                indices: &i,
                color,
            }],
        )
        .unwrap();
    }
    (ifc.finish().unwrap(), seen)
}

#[test]
fn storeys_classes_types_materials_and_properties() {
    let (report, seen) = read(MODEL.len());
    let seen = seen.borrow();
    assert_eq!(report.file.application.as_deref(), Some("Example CAD 2026"));
    assert_eq!(report.file.application_details.as_deref(), Some("IFC4"));
    // Layers: storeys by elevation, then no storey; classes below.
    let paths: Vec<&str> = report
        .structure
        .layers
        .iter()
        .map(|l| l.path.as_str())
        .collect();
    for p in [
        "Level 0",
        "Level 1",
        "No storey",
        "Level 0::IfcWall",
        "Level 0::IfcStairFlight",
        "Level 1::IfcDoor",
        "No storey::IfcBuildingElementProxy",
        "No storey::IfcProduct",
    ] {
        assert!(paths.contains(&p), "{p} in {paths:?}");
    }
    assert!(
        paths.iter().position(|p| *p == "Level 0") < paths.iter().position(|p| *p == "Level 1")
    );
    assert_eq!(
        seen.layers,
        paths.iter().map(|p| p.to_string()).collect::<Vec<_>>()
    );
    // Materials by name: the wall's thickest layer, the door's own; coloured by the engine.
    let materials: Vec<&str> = report.materials.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(materials, ["Concrete", "Oak"]);
    assert_eq!(report.materials[0].color.as_deref(), Some("#c8c8c8"));
    // Storeys from the file, in metres.
    let storeys = report
        .proportions
        .as_ref()
        .unwrap()
        .storeys
        .as_ref()
        .unwrap();
    assert_eq!(storeys.levels, [0.0, 3.0]);
    assert_eq!(storeys.typical_height, 3.0);
    // Types act as blocks.
    assert_eq!(report.blocks.len(), 1);
    assert_eq!(
        (report.blocks[0].name.as_str(), report.blocks[0].placements),
        ("Basic Wall 215", 1)
    );
    // And count in the repetition figures.
    let r = &report.style.repetition;
    assert_eq!(
        (
            r.block_definitions,
            r.block_placements,
            r.max_placements_per_block
        ),
        (1, 1, 1)
    );
    assert_eq!(report.inventory.objects, 5);

    let wall = seen
        .picks
        .iter()
        .find(|p| p.3.as_deref() == Some("Wall south"))
        .expect("wall picked");
    assert_eq!(wall.1, "IfcWall");
    assert_eq!(wall.2.as_deref(), Some("Basic Wall 215"));
    let get = |k: &str| {
        wall.4
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(get("Storey"), Some("Level 0"));
    assert_eq!(get("Material"), Some("Concrete"));
    assert_eq!(
        get("Material layers"),
        Some("Concrete 200 mm, Plaster 15 mm")
    );
    assert_eq!(get("ObjectType"), Some("Exterior"));
    assert_eq!(get("Tag"), Some("W-01"));
    // The product's own property set replaces the type's values.
    assert_eq!(get("Pset_WallCommon.IsExternal"), Some("true"));
    assert_eq!(get("Pset_WallCommon.FireRating"), Some("REI60"));
    assert_eq!(get("Qto_WallBaseQuantities.Width"), Some("215"));
    let door = seen
        .picks
        .iter()
        .find(|p| p.1 == "IfcDoor")
        .expect("door picked");
    assert_eq!(door.3.as_deref(), Some("Door été"));
    let flight = seen
        .picks
        .iter()
        .find(|p| p.1 == "IfcStairFlight")
        .expect("flight picked");
    assert!(
        flight
            .4
            .contains(&("Storey".to_string(), "Level 0".to_string())),
        "through the stair it is part of"
    );
    // The engine's colour reaches the mesh.
    assert!(seen.meshes.iter().any(|m| m.2 == Some([200, 200, 200])));
}

#[test]
fn slices_do_not_change_the_report() {
    let whole = serde_json::to_string(&read(MODEL.len()).0).unwrap();
    for slice in [1, 7, 64, 333] {
        assert_eq!(
            serde_json::to_string(&read(slice).0).unwrap(),
            whole,
            "slices of {slice}"
        );
    }
}

#[test]
fn feet_and_unknown_files() {
    let feet = MODEL
        .replace("#4=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);", "#4=IFCCONVERSIONBASEDUNIT(#7,.LENGTHUNIT.,'FOOT',#8);\n#8=IFCMEASUREWITHUNIT(IFCLENGTHMEASURE(0.3048),#9);\n#9=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);")
        .replace("3000.);", "10.);");
    let mut ifc = IfcInspector::new();
    ifc.push(feet.as_bytes()).unwrap();
    let report = ifc.finish().unwrap();
    let storeys = report.proportions.map(|p| p.storeys);
    // No product meshes were added, so there are no proportions to attach storeys to.
    assert!(storeys.is_none_or(|s| s.is_none_or(|s| (s.levels[1] - 3.048).abs() < 1e-9)));
    let mut ifc = IfcInspector::new();
    assert!(ifc.push(b"\x00\x01not a step file").is_err());
    let mut ifc = IfcInspector::new();
    ifc.push(&MODEL.as_bytes()[..MODEL.len() / 2]).unwrap();
    assert!(matches!(ifc.finish(), Err(InspectError::Truncated { .. })));
}

#[test]
fn class_names_read_as_the_schema_spells_them() {
    assert_eq!(pretty_class("IFCWALLSTANDARDCASE"), "IfcWallStandardCase");
    assert_eq!(pretty_class("IFCFURNITURE"), "IfcFurniture");
    assert_eq!(pretty_class("IFCSOMETHINGNEW"), "IfcSomethingnew");
}
