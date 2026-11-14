use crate::testkit::*;
use crate::threemf::{self, Caps};
use crate::*;
use zip::CompressionMethod;

fn p(bytes: &[u8]) -> Result<Mesh, MeshError> {
    parse(bytes, NameHint::Model)
}

fn tri() -> [f32; 9] {
    [0.0, 0.0, 0.0, 10.0, 0.0, 0.0, 0.0, 10.0, 0.0]
}

fn bounds(m: &Mesh) -> ([f32; 3], [f32; 3]) {
    (m.min, m.max)
}

// ---------- names and sniffing ----------

#[test]
fn names_and_mime_only_say_whether_to_try() {
    assert!(looks_like_model(Some("Benchy.STL"), None));
    assert!(looks_like_model(
        Some("a.3mf"),
        Some("application/octet-stream")
    ));
    assert!(looks_like_model(None, Some("model/stl")));
    assert!(looks_like_model(None, Some("Application/SLA; charset=x")));
    assert!(looks_like_model(None, Some("application/vnd.ms-pki.stl")));
    assert!(looks_like_model(
        None,
        Some("application/vnd.ms-package.3dmanufacturing-3dmodel+xml")
    ));
    assert!(looks_like_model(None, Some("model/3mf")));
    assert!(!looks_like_model(Some("a.png"), Some("image/png")));
    assert!(!looks_like_model(None, None));
}

#[test]
fn the_content_picks_the_parser_not_the_name() {
    let bin = stl_binary(&cube(0.0, 10.0));
    let m = p(&bin).unwrap();
    assert_eq!((m.format, m.triangle_count()), (Format::StlBinary, 12));
    let asc = stl_ascii(&cube(0.0, 10.0));
    assert_eq!(p(asc.as_bytes()).unwrap().format, Format::StlAscii);
    assert_eq!(
        p(&threemf(&simple_model("millimeter"))).unwrap().format,
        Format::ThreeMf
    );
    // A binary STL whose header starts with "solid": ASCII fails, the size rule holds, binary wins.
    let mut tricky = stl_binary(&cube(0.0, 10.0));
    tricky[..5].copy_from_slice(b"solid");
    assert_eq!(p(&tricky).unwrap().format, Format::StlBinary);
    // No hint: a headerless binary is not tried, self-identifying content is.
    assert!(matches!(
        parse(&bin, NameHint::None),
        Err(MeshError::Unsupported(_))
    ));
    assert!(parse(asc.as_bytes(), NameHint::None).is_ok());
    // A ZIP that is no 3MF, and junk.
    let other = zip_of(&[("a.txt", b"x")], CompressionMethod::Stored);
    assert!(matches!(p(&other), Err(MeshError::Unsupported(_))));
    assert!(matches!(p(b"hello"), Err(MeshError::Malformed(_))));
    assert!(matches!(p(b""), Err(MeshError::Malformed(_))));
}

#[test]
fn binary_and_ascii_stl_agree() {
    let t = cube(-5.0, 15.0);
    let a = p(&stl_binary(&t)).unwrap();
    let b = p(stl_ascii(&t).as_bytes()).unwrap();
    assert_eq!(a.positions, b.positions);
    assert_eq!(bounds(&a), ([-5.0; 3], [15.0; 3]));
    assert!(a.colors.is_none());
}

// ---------- the bounds, one test each ----------

#[test]
fn bound_file_size() {
    let big = vec![0u8; MAX_FILE_BYTES + 1];
    assert_eq!(p(&big), Err(MeshError::TooLarge));
}

#[test]
fn bound_triangles_binary_and_a_size_lie() {
    let n = MAX_TRIANGLES as u32 + 1;
    let mut v = vec![0u8; 84 + 50 * n as usize];
    v[80..84].copy_from_slice(&n.to_le_bytes());
    assert_eq!(p(&v), Err(MeshError::Refused(Bound::Triangles)));
    // The header claims more than the file holds: refused as malformed, nothing allocated for it.
    let mut lie = stl_binary(&[tri()]);
    lie[80..84].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(p(&lie), Err(MeshError::Malformed(_))));
    let mut lie = stl_binary(&[tri()]);
    lie[80..84].copy_from_slice(&5u32.to_le_bytes());
    assert!(matches!(p(&lie), Err(MeshError::Malformed(_))));
}

#[test]
fn bound_triangles_ascii() {
    let one = "vertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\n";
    let mut s = String::from("solid x\n");
    s.push_str(&one.repeat(MAX_TRIANGLES + 1));
    assert!(s.len() <= MAX_FILE_BYTES);
    assert_eq!(p(s.as_bytes()), Err(MeshError::Refused(Bound::Triangles)));
}

#[test]
fn bound_ascii_line() {
    let mut s = String::from("solid x\n");
    s.push_str(&" ".repeat(MAX_ASCII_LINE + 1));
    s.push('\n');
    assert_eq!(p(s.as_bytes()), Err(MeshError::Refused(Bound::AsciiLine)));
    // No newline at all, a long single line.
    let long = format!("solid {}", "x".repeat(10_000));
    assert_eq!(
        p(long.as_bytes()),
        Err(MeshError::Refused(Bound::AsciiLine))
    );
    // A line of exactly the cap is fine.
    let mut ok = stl_ascii(&[tri()]);
    ok.insert_str(8, &format!("{}\n", " ".repeat(MAX_ASCII_LINE)));
    assert!(p(ok.as_bytes()).is_ok());
}

#[test]
fn bound_non_finite_coordinate_drops_the_triangle() {
    let mut bad = tri();
    bad[4] = f32::NAN;
    let mut inf = tri();
    inf[0] = f32::INFINITY;
    let m = p(&stl_binary(&[tri(), bad, inf])).unwrap();
    assert_eq!(m.triangle_count(), 1);
    let ascii = stl_ascii(&[tri(), bad]).replace("NaN", "nan");
    assert_eq!(p(ascii.as_bytes()).unwrap().triangle_count(), 1);
    // Only non-finite triangles: nothing to show.
    assert!(matches!(
        p(&stl_binary(&[bad])),
        Err(MeshError::Malformed(_))
    ));
    // 3MF
    let res = tetra_object(1, "", "").replacen(
        r#"<vertex x="10" y="0" z="0"/>"#,
        r#"<vertex x="nan" y="0" z="0"/>"#,
        1,
    );
    let m = p(&threemf(&model_wrap(
        "millimeter",
        &res,
        r#"<item objectid="1"/>"#,
    )))
    .unwrap();
    assert_eq!(m.triangle_count(), 1);
    assert!(m.positions.iter().all(|c| c.is_finite()));
}

#[test]
fn bound_zip_entries() {
    let names: Vec<String> = (0..=MAX_ZIP_ENTRIES).map(|i| format!("e{i}")).collect();
    let parts: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
    let z = zip_of(&parts, CompressionMethod::Stored);
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::ZipEntries)));
}

#[test]
fn bound_compression_is_stored_or_deflate_only() {
    let mut z = threemf(&simple_model("millimeter"));
    // Patch every local (offset 8) and central (offset 10) header's method to 12 (bzip2).
    let mut i = 0;
    while i + 46 < z.len() {
        if z[i..].starts_with(b"PK\x03\x04") {
            z[i + 8] = 12;
            z[i + 9] = 0;
        } else if z[i..].starts_with(b"PK\x01\x02") {
            z[i + 10] = 12;
            z[i + 11] = 0;
        }
        i += 1;
    }
    assert!(
        matches!(p(&z), Err(MeshError::Unsupported(_))),
        "{:?}",
        p(&z)
    );
}

#[test]
fn bound_model_xml_total_never_trusts_the_header() {
    // 3 MB of small tokens (one big token is the token cap's business, tested apart).
    let pad = "<a/>".repeat(750_000);
    let model = simple_model("millimeter").replace("<build>", &format!("{pad}<build>"));
    // Deflated: a tiny entry that inflates past the cap.
    let z = zip_of(
        &[
            ("_rels/.rels", RELS.as_bytes()),
            ("3D/3dmodel.model", model.as_bytes()),
        ],
        CompressionMethod::Deflated,
    );
    assert!(z.len() < 100_000);
    let caps = Caps {
        total_xml: 100_000,
        ..Caps::default()
    };
    assert_eq!(
        threemf::parse(&z, &caps),
        Err(MeshError::Refused(Bound::XmlTotal))
    );
    // The same file under the real cap parses.
    assert!(p(&z).is_ok());
    // Across parts: each part alone fits, both together do not.
    let part = simple_model("millimeter");
    let extra = vec![("3D/o.model".to_string(), part.clone())];
    let root = model_wrap(
        "millimeter",
        &tetra_object(1, "", ""),
        r#"<item objectid="1"/><item objectid="1" p:path="/3D/o.model"/>"#,
    );
    let z = threemf_parts(&root, &extra);
    let one = root.len().max(part.len()) as u64;
    let caps = Caps {
        total_xml: one + 50,
        ..Caps::default()
    };
    assert_eq!(
        threemf::parse(&z, &caps),
        Err(MeshError::Refused(Bound::XmlTotal))
    );
    assert!(p(&z).is_ok());
}

#[test]
fn bound_small_parts() {
    let rels = RELS.replace(
        "</Relationships>",
        &format!("<!-- {} --></Relationships>", "x".repeat(70_000)),
    );
    let z = zip_of(
        &[
            ("_rels/.rels", rels.as_bytes()),
            ("3D/3dmodel.model", simple_model("millimeter").as_bytes()),
        ],
        CompressionMethod::Stored,
    );
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::SmallPart)));
    let ct = format!("<!-- {} -->{CONTENT_TYPES}", "x".repeat(70_000));
    let z = zip_of(
        &[
            ("[Content_Types].xml", ct.as_bytes()),
            ("_rels/.rels", RELS.as_bytes()),
            ("3D/3dmodel.model", simple_model("millimeter").as_bytes()),
        ],
        CompressionMethod::Stored,
    );
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::SmallPart)));
}

#[test]
fn bound_doctype_and_entities() {
    let evil = r#"<!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;&lol;">]>"#;
    let model = simple_model("millimeter").replacen("<model", &format!("{evil}<model"), 1);
    assert_eq!(p(&threemf(&model)), Err(MeshError::Refused(Bound::DocType)));
    let rels = format!("{evil}{RELS}");
    let z = zip_of(
        &[
            ("_rels/.rels", rels.as_bytes()),
            ("3D/3dmodel.model", simple_model("millimeter").as_bytes()),
        ],
        CompressionMethod::Stored,
    );
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::DocType)));
    // An entity reference alone is never expanded and does no harm.
    let model = simple_model("millimeter").replace("<build>", "<metadata>&lol;</metadata><build>");
    assert!(
        p(&threemf(&model)).is_ok() || matches!(p(&threemf(&model)), Err(MeshError::Malformed(_)))
    );
}

#[test]
fn bound_xml_depth() {
    let deep = format!("{}{}", "<a>".repeat(40), "</a>".repeat(40));
    let model = simple_model("millimeter").replace("<build>", &format!("{deep}<build>"));
    assert_eq!(
        p(&threemf(&model)),
        Err(MeshError::Refused(Bound::XmlDepth))
    );
    let shallow = format!("{}{}", "<a>".repeat(20), "</a>".repeat(20));
    let model = simple_model("millimeter").replace("<build>", &format!("{shallow}<build>"));
    assert!(p(&threemf(&model)).is_ok());
}

#[test]
fn bound_vertex_index() {
    let res = tetra_object(1, "", "").replace(r#"v3="1""#, r#"v3="9""#);
    let model = model_wrap("millimeter", &res, r#"<item objectid="1"/>"#);
    assert_eq!(
        p(&threemf(&model)),
        Err(MeshError::Refused(Bound::VertexIndex))
    );
}

#[test]
fn bound_model_parts() {
    let build = |n: usize| {
        let extra: Vec<(String, String)> = (0..n)
            .map(|i| (format!("3D/Objects/o{i}.model"), simple_model("millimeter")))
            .collect();
        let items: String = (0..n)
            .map(|i| format!(r#"<item objectid="1" p:path="/3D/Objects/o{i}.model"/>"#))
            .collect();
        threemf_parts(&model_wrap("millimeter", "", &items), &extra)
    };
    // The root plus 63 = 64 parts.
    let m = p(&build(MAX_MODEL_PARTS - 1)).unwrap();
    assert_eq!(m.triangle_count(), 4 * (MAX_MODEL_PARTS - 1));
    assert_eq!(
        p(&build(MAX_MODEL_PARTS)),
        Err(MeshError::Refused(Bound::ModelParts))
    );
    // A part that is not there.
    let z = threemf(&model_wrap(
        "millimeter",
        "",
        r#"<item objectid="1" p:path="/3D/none.model"/>"#,
    ));
    assert!(matches!(p(&z), Err(MeshError::Malformed(_))));
}

fn chain(n: u32) -> Vec<u8> {
    let mut res = String::new();
    for i in 1..n {
        res.push_str(&format!(
            r#"<object id="{i}"><components><component objectid="{}"/></components></object>"#,
            i + 1
        ));
    }
    res.push_str(&tetra_object(n, "", ""));
    threemf(&model_wrap("millimeter", &res, r#"<item objectid="1"/>"#))
}

#[test]
fn bound_component_depth() {
    // 17 objects = an item's object plus 16 levels of components.
    assert!(p(&chain(MAX_COMPONENT_DEPTH as u32 + 1)).is_ok());
    assert_eq!(
        p(&chain(MAX_COMPONENT_DEPTH as u32 + 2)),
        Err(MeshError::Refused(Bound::ComponentDepth))
    );
}

#[test]
fn bound_component_cycle() {
    let res = r#"<object id="1"><components><component objectid="2"/></components></object><object id="2"><components><component objectid="1"/></components></object>"#;
    let z = threemf(&model_wrap("millimeter", res, r#"<item objectid="1"/>"#));
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::ComponentCycle)));
    let res = r#"<object id="1"><components><component objectid="1"/></components></object>"#;
    let z = threemf(&model_wrap("millimeter", res, r#"<item objectid="1"/>"#));
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::ComponentCycle)));
}

fn many_tri_object(id: u32, n: usize) -> String {
    let tris: String = (0..n)
        .map(|_| r#"<triangle v1="0" v2="1" v3="2"/>"#)
        .collect();
    format!(
        r#"<object id="{id}"><mesh><vertices><vertex x="0" y="0" z="0"/><vertex x="1" y="0" z="0"/><vertex x="0" y="1" z="0"/></vertices><triangles>{tris}</triangles></mesh></object>"#
    )
}

#[test]
fn bound_instanced_triangles_count_toward_the_cap() {
    let mut res = many_tri_object(1, 1000);
    let comps: String = (0..2001).map(|_| r#"<component objectid="1"/>"#).collect();
    res.push_str(&format!(
        r#"<object id="2"><components>{comps}</components></object>"#
    ));
    let z = threemf(&model_wrap("millimeter", &res, r#"<item objectid="2"/>"#));
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::Triangles)));
    // 2000 instances of 1000 triangles is exactly the cap.
    let mut res = many_tri_object(1, 1000);
    let comps: String = (0..2000).map(|_| r#"<component objectid="1"/>"#).collect();
    res.push_str(&format!(
        r#"<object id="2"><components>{comps}</components></object>"#
    ));
    let z = threemf(&model_wrap("millimeter", &res, r#"<item objectid="2"/>"#));
    assert_eq!(p(&z).unwrap().triangle_count(), 2_000_000);
}

#[test]
fn bound_unique_triangles_in_the_file() {
    let res = many_tri_object(1, MAX_TRIANGLES / 2) + &many_tri_object(2, MAX_TRIANGLES / 2 + 1);
    let z = threemf(&model_wrap("millimeter", &res, r#"<item objectid="1"/>"#));
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::Triangles)));
}

#[test]
fn bound_instances_objects_and_vertices_by_lowered_caps() {
    let model = |res: &str| threemf(&model_wrap("millimeter", res, r#"<item objectid="1"/>"#));
    // Stored components over the cap.
    let comps: String = (0..51).map(|_| r#"<component objectid="2"/>"#).collect();
    let res = format!(
        r#"<object id="1"><components>{comps}</components></object>{}"#,
        tetra_object(2, "", "")
    );
    let caps = Caps {
        instances: 50,
        ..Caps::default()
    };
    assert_eq!(
        threemf::parse(&model(&res), &caps),
        Err(MeshError::Refused(Bound::Instances))
    );
    // A fan-out of empty objects: 1 + 10 + 100 visits against a cap of 50.
    let res = format!(
        r#"<object id="1"><components>{a}</components></object><object id="2"><components>{b}</components></object><object id="3"/>"#,
        a = (0..10)
            .map(|_| r#"<component objectid="2"/>"#)
            .collect::<String>(),
        b = (0..10)
            .map(|_| r#"<component objectid="3"/>"#)
            .collect::<String>(),
    );
    let caps = Caps {
        instances: 50,
        ..Caps::default()
    };
    // 1 + 10 + 100 visits (and no triangles, so the empty result would be malformed): the cap fires first.
    assert_eq!(
        threemf::parse(&model(&res), &caps),
        Err(MeshError::Refused(Bound::Instances))
    );
    // Objects.
    let res = (1..=4)
        .map(|i| format!(r#"<object id="{i}"/>"#))
        .collect::<String>();
    let caps = Caps {
        objects: 3,
        ..Caps::default()
    };
    assert_eq!(
        threemf::parse(&model(&res), &caps),
        Err(MeshError::Refused(Bound::Objects))
    );
    // Vertices.
    let caps = Caps {
        vertices: 3,
        ..Caps::default()
    };
    assert_eq!(
        threemf::parse(&model(&tetra_object(1, "", "")), &caps),
        Err(MeshError::Refused(Bound::Vertices))
    );
}

#[test]
fn bound_transform_is_twelve_finite_numbers() {
    let go = |t: &str| {
        let build = format!(r#"<item objectid="1" transform="{t}"/>"#);
        p(&threemf(&model_wrap(
            "millimeter",
            &tetra_object(1, "", ""),
            &build,
        )))
    };
    let m = go("1 0 0 0 1 0 0 0 1 5 6 7").unwrap();
    assert_eq!(m.min, [5.0, 6.0, 7.0]);
    assert_eq!(m.max, [15.0, 16.0, 17.0]);
    for bad in [
        "1 0 0 0 1 0 0 0 1 5 6",
        "1 0 0 0 1 0 0 0 1 5 6 7 8",
        "1 0 0 0 1 0 0 0 1 5 6 nan",
        "1 0 0 0 1 0 0 0 1 5 6 inf",
        "a b c d e f g h i j k l",
    ] {
        assert_eq!(go(bad), Err(MeshError::Refused(Bound::Transform)), "{bad}");
    }
}

#[test]
fn components_compose_transforms_child_first() {
    // The child moves by +10 in x, the parent then rotates 90 degrees about z (x -> y).
    let res = format!(
        r#"{}<object id="2"><components><component objectid="1" transform="1 0 0 0 1 0 0 0 1 10 0 0"/></components></object>"#,
        tetra_object(1, "", "")
    );
    let build = r#"<item objectid="2" transform="0 1 0 -1 0 0 0 0 1 0 0 0"/>"#;
    let m = p(&threemf(&model_wrap("millimeter", &res, build))).unwrap();
    // (x,y,z) -> (x+10,y,z) -> (-y, x+10, z): x in [-10,0], y in [10,20].
    assert_eq!((m.min, m.max), ([-10.0, 10.0, 0.0], [0.0, 20.0, 10.0]));
}

#[test]
fn bound_units() {
    for (unit, k) in [
        ("micron", 0.001f32),
        ("millimeter", 1.0),
        ("centimeter", 10.0),
        ("inch", 25.4),
        ("foot", 304.8),
        ("meter", 1000.0),
    ] {
        let m = p(&threemf(&simple_model(unit))).unwrap();
        assert!(
            (m.max[0] - 10.0 * k).abs() < 1e-3 * k.max(1.0),
            "{unit}: {:?}",
            m.max
        );
    }
    assert_eq!(
        p(&threemf(&simple_model("furlong"))),
        Err(MeshError::Refused(Bound::Unit))
    );
    // No unit attribute means millimetres.
    let m = p(&threemf(
        &simple_model("millimeter").replace(r#"unit="millimeter""#, ""),
    ))
    .unwrap();
    assert_eq!(m.max[0], 10.0);
}

#[test]
fn bound_colour_count_and_format() {
    let bases = |n: usize| -> String {
        let b: String = (0..n)
            .map(|_| r##"<base name="a" displaycolor="#FF0000"/>"##)
            .collect();
        format!(r#"<basematerials id="1">{b}</basematerials>"#)
    };
    let model = |n: usize| {
        let res = format!(
            "{}{}",
            bases(n),
            tetra_object(2, r#"pid="1" pindex="0""#, "")
        );
        threemf(&model_wrap("millimeter", &res, r#"<item objectid="2"/>"#))
    };
    assert!(p(&model(MAX_COLOURS)).is_ok());
    assert_eq!(
        p(&model(MAX_COLOURS + 1)),
        Err(MeshError::Refused(Bound::Colours))
    );
}

#[test]
fn standard_colours_per_object_per_triangle_and_colorgroup() {
    let res = format!(
        r##"<basematerials id="1"><base name="o" displaycolor="#E05A1E"/><base name="b" displaycolor="#2A6FDB80"/><base name="bad" displaycolor="red"/></basematerials><m:colorgroup id="5"><m:color color="#10203040"/></m:colorgroup>{}"##,
        tetra_object(2, r#"pid="1" pindex="0""#, "")
            .replacen(
                r#"<triangle v1="0" v2="1" v3="3" />"#,
                r#"<triangle v1="0" v2="1" v3="3" pid="1" p1="1"/>"#,
                1
            )
            .replacen(
                r#"<triangle v1="1" v2="2" v3="3" />"#,
                r#"<triangle v1="1" v2="2" v3="3" pid="5" p1="0" p2="0" p3="0"/>"#,
                1
            )
            .replacen(
                r#"<triangle v1="2" v2="0" v3="3" />"#,
                r#"<triangle v1="2" v2="0" v3="3" pid="1" p1="2"/>"#,
                1
            )
    );
    let m = p(&threemf(&model_wrap(
        "millimeter",
        &res,
        r#"<item objectid="2"/>"#,
    )))
    .unwrap();
    let c = m.colors.unwrap();
    // Triangle 0: the object's colour; 1: p1 = 1; 2: the colorgroup; 3: an unparsable colour falls back to the object's.
    assert_eq!(c, vec![0xE05A1EFF, 0x2A6FDB80, 0x10203040, 0xE05A1EFF]);
    // No colour anywhere: no table.
    assert!(
        p(&threemf(&simple_model("millimeter")))
            .unwrap()
            .colors
            .is_none()
    );
}

#[test]
fn production_extension_parts_and_component_paths() {
    let part = model_wrap("millimeter", &tetra_object(7, "", ""), "");
    let res = r#"<object id="1"><components><component objectid="7" p:path="/3D/Objects/a.model"/></components></object>"#;
    let root = model_wrap("millimeter", res, r#"<item objectid="1"/>"#);
    let z = threemf_parts(&root, &[("3D/Objects/a.model".into(), part)]);
    let m = p(&z).unwrap();
    assert_eq!(m.triangle_count(), 4);
}

#[test]
fn model_parts_are_not_rendered_when_support_objects() {
    let res = format!(
        "{}{}",
        tetra_object(1, "", ""),
        tetra_object(2, "", "").replace(r#"type="model""#, r#"type="support""#)
    );
    let z = threemf(&model_wrap(
        "millimeter",
        &res,
        r#"<item objectid="1"/><item objectid="2"/>"#,
    ));
    assert_eq!(p(&z).unwrap().triangle_count(), 4);
}

#[test]
fn bounds_match_the_design_table() {
    assert_eq!(MAX_FILE_BYTES, 256 * 1024 * 1024);
    assert_eq!(MAX_TRIANGLES, 2_000_000);
    assert_eq!(MAX_ASCII_LINE, 4096);
    assert_eq!(MAX_ZIP_ENTRIES, 4096);
    assert_eq!(MAX_MODEL_XML_TOTAL, 512 * 1024 * 1024);
    assert_eq!(MAX_SMALL_PART_BYTES, 64 * 1024);
    assert_eq!(MAX_CONFIG_PART_BYTES, 4 * 1024 * 1024);
    assert_eq!(MAX_CONFIG_PARTS, 8);
    assert_eq!(MAX_XML_DEPTH, 32);
    assert_eq!(MAX_MODEL_PARTS, 64);
    assert_eq!(MAX_COMPONENT_DEPTH, 16);
    assert_eq!(MAX_COLOURS, 256);
    assert_eq!(MAX_XML_TOKEN, 1024 * 1024);
    assert_eq!(MAX_PATH_BYTES, 1024);
    assert_eq!(MAX_GROUPS, 256);
}

// ---------- the rasterizer ----------

fn coverage(s: &Still) -> usize {
    s.rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] > 0)
        .count()
}

#[test]
fn rasterizer_is_deterministic_and_fits_the_frame() {
    let m = p(&stl_binary(&cube(0.0, 10.0))).unwrap();
    let mode = StillMode::Colour {
        fallback: 0x8FA3BFFF,
    };
    let a = render_still(&m, 64, mode);
    let b = render_still(&m, 64, mode);
    assert_eq!(a, b);
    assert_eq!((a.width, a.height, a.rgba.len()), (64, 64, 64 * 64 * 4));
    // Transparent corners, opaque middle, centred, filling about 92 % of the frame.
    assert_eq!(a.rgba[3], 0);
    assert_eq!(a.rgba[(32 * 64 + 32) * 4 + 3], 255);
    let (mut x0, mut x1, mut y0, mut y1) = (64, 0, 64, 0);
    for y in 0..64 {
        for x in 0..64 {
            if a.rgba[(y * 64 + x) * 4 + 3] > 0 {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    let span = (x1 - x0 + 1).max(y1 - y0 + 1);
    assert!((56..=61).contains(&span), "span {span}");
    assert!((x0 as i32 - (63 - x1 as i32)).abs() <= 2 || (y0 as i32 - (63 - y1 as i32)).abs() <= 2);
    let cov = coverage(&a);
    assert!((2630..=2760).contains(&cov), "coverage {cov}");
    // A bigger or smaller request is clamped.
    assert_eq!(render_still(&m, 1, mode).width, MIN_STILL_EDGE);
    assert_eq!(
        render_still(&m, 100_000, StillMode::Shade).width,
        MAX_STILL_EDGE
    );
}

#[test]
fn rasterizer_modes() {
    let m = p(&stl_binary(&cube(0.0, 10.0))).unwrap();
    let shade = render_still(&m, 64, StillMode::Shade);
    for px in shade.rgba.as_chunks::<4>().0.iter().filter(|p| p[3] > 0) {
        assert!(px[0] == px[1] && px[1] == px[2], "grey");
    }
    let greys: std::collections::BTreeSet<u8> = shade
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] == 255)
        .map(|p| p[0])
        .collect();
    assert!(greys.len() >= 2, "faces differ in light: {greys:?}");
    // The floor of the light: a face turned away from it stays readable (0.44 at the darkest).
    assert!(
        greys.iter().all(|g| *g >= 108),
        "no face darker than ~0.43: {greys:?}"
    );
    let col = render_still(
        &m,
        64,
        StillMode::Colour {
            fallback: 0xFF0000FF,
        },
    );
    assert_eq!(coverage(&col), coverage(&shade));
    for px in col.rgba.as_chunks::<4>().0.iter().filter(|p| p[3] == 255) {
        assert!(px[0] > 0 && px[1] == 0 && px[2] == 0);
    }
    // A triangle's own colour beats the fallback.
    let res = tetra_object(1, "", "");
    let own = format!(
        r##"<basematerials id="9"><base name="g" displaycolor="#00FF00"/></basematerials>{}"##,
        res.replace(
            "<object id=\"1\" type=\"model\" ",
            "<object id=\"1\" type=\"model\" pid=\"9\" pindex=\"0\" "
        )
    );
    let m = p(&threemf(&model_wrap(
        "millimeter",
        &own,
        r#"<item objectid="1"/>"#,
    )))
    .unwrap();
    let s = render_still(
        &m,
        64,
        StillMode::Colour {
            fallback: 0xFF0000FF,
        },
    );
    assert!(
        s.rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] == 255)
            .all(|p| p[0] == 0 && p[1] > 0)
    );
}

#[test]
fn rasterizer_work_is_bounded_for_stacked_huge_triangles() {
    // 200 000 triangles that each cover the frame: without the work cap this is ~10^11 pixel visits.
    let big = [-100.0, -100.0, 0.0, 100.0, -100.0, 0.0, 0.0, 100.0, 0.0];
    let m = p(&stl_binary(&vec![big; 200_000])).unwrap();
    let s = render_still(&m, 256, StillMode::Shade);
    assert!(coverage(&s) > 0);
}

#[test]
fn png_encodes_the_still() {
    let m = p(&stl_binary(&cube(0.0, 10.0))).unwrap();
    let s = render_still(&m, 32, StillMode::Shade);
    let bytes = encode_png(&s);
    assert!(bytes.starts_with(b"\x89PNG"));
    let dec = png::Decoder::new(std::io::Cursor::new(&bytes));
    let mut r = dec.read_info().unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    assert_eq!((info.width, info.height), (32, 32));
    assert_eq!(&buf[..info.buffer_size()], &s.rgba[..]);
}

/// The 2 M-triangle timing the lead runs by hand in a release build:
/// `cargo nextest run --release -p kourier-mesh --run-ignored only two_million_triangles`.
#[test]
#[ignore = "timing, run by hand in release mode"]
fn two_million_triangles_parse_and_render_under_1_5_seconds() {
    use std::time::Instant;
    // A bumpy sphere, 2 M triangles, as in the spike.
    // 2 · s · m = 2 M triangles.
    let s = 1000usize;
    let m = s;
    let mut v = Vec::with_capacity((s + 1) * m);
    for i in 0..=s {
        let th = std::f64::consts::PI * i as f64 / s as f64;
        for j in 0..m {
            let ph = 2.0 * std::f64::consts::PI * j as f64 / m as f64;
            let r = 40.0 * (1.0 + 0.12 * (7.0 * th).sin() * (5.0 * ph).cos());
            v.push([
                (r * th.sin() * ph.cos()) as f32,
                (r * th.sin() * ph.sin()) as f32,
                (40.0 + r * th.cos()) as f32,
            ]);
        }
    }
    let mut tris = Vec::with_capacity(2 * s * m);
    for i in 0..s {
        for j in 0..m {
            let a = i * m + j;
            let b = i * m + (j + 1) % m;
            let c = (i + 1) * m + j;
            let d = (i + 1) * m + (j + 1) % m;
            tris.push([a, c, b]);
            tris.push([b, c, d]);
        }
    }
    assert_eq!(tris.len(), MAX_TRIANGLES);
    let mut bytes = vec![0u8; 80];
    bytes.extend_from_slice(&(tris.len() as u32).to_le_bytes());
    for t in &tris {
        bytes.extend_from_slice(&[0u8; 12]);
        for &k in t {
            for c in v[k] {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&[0u8; 2]);
    }
    let t0 = Instant::now();
    let mesh = parse(&bytes, NameHint::Model).unwrap();
    let t1 = Instant::now();
    let still = render_still(
        &mesh,
        400,
        StillMode::Colour {
            fallback: 0x8FA3BFFF,
        },
    );
    let png = encode_png(&still);
    let t2 = Instant::now();
    eprintln!(
        "parse {:?} render+png {:?} png {} B",
        t1 - t0,
        t2 - t1,
        png.len()
    );
    assert!((t2 - t0).as_secs_f64() < 1.5, "{:?}", t2 - t0);
}

// ---------- the review round ----------

/// A central header with an empty name pointing at offset `local` (46 bytes).
fn central(local: u32) -> Vec<u8> {
    let mut h = vec![0u8; 46];
    h[..4].copy_from_slice(b"PK\x01\x02");
    h[42..46].copy_from_slice(&local.to_le_bytes());
    h
}

fn eocd(entries: u16, cd_size: u32, cd_offset: u32) -> Vec<u8> {
    let mut e = vec![0u8; 22];
    e[..4].copy_from_slice(b"PK\x05\x06");
    e[8..10].copy_from_slice(&entries.to_le_bytes());
    e[10..12].copy_from_slice(&entries.to_le_bytes());
    e[12..16].copy_from_slice(&cd_size.to_le_bytes());
    e[16..20].copy_from_slice(&cd_offset.to_le_bytes());
    e
}

fn local_header() -> Vec<u8> {
    let mut h = vec![0u8; 30];
    h[..4].copy_from_slice(b"PK\x03\x04");
    h
}

#[test]
fn zip_a_chain_of_end_records_is_refused_at_once() {
    let mut z = local_header();
    for _ in 0..65_533 {
        z.extend_from_slice(&central(0));
    }
    for _ in 0..2000 {
        z.extend_from_slice(&eocd(65_534, 0, 30));
    }
    let t = std::time::Instant::now();
    let r = p(&z);
    assert!(r.is_err());
    assert!(t.elapsed().as_secs_f64() < 2.0, "{:?}", t.elapsed());
    // Only end records, no central headers: the structure bound names it.
    let mut z = local_header();
    for _ in 0..2000 {
        z.extend_from_slice(&eocd(0, 0, 30));
    }
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::ZipStructure)));
}

#[test]
fn zip_many_central_headers_with_duplicate_names_are_refused() {
    let n = MAX_ZIP_ENTRIES + 100;
    let mut z = local_header();
    for _ in 0..n {
        z.extend_from_slice(&central(0));
    }
    z.extend_from_slice(&eocd(n as u16, (46 * n) as u32, 30));
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::ZipEntries)));
}

#[test]
fn zip_two_entries_may_not_share_one_local_header() {
    // Both names are 11 bytes, so a local-name check cannot tell them apart.
    let rels = RELS.replace("/3D/3dmodel.model", "/3D/m1.model");
    let mut z = zip_of(
        &[
            ("_rels/.rels", rels.as_bytes()),
            ("3D/m1.model", simple_model("millimeter").as_bytes()),
        ],
        CompressionMethod::Stored,
    );
    // Point the second central record's local offset at the first local header (0).
    let mut seen = 0;
    let mut i = 0;
    while i + 46 < z.len() {
        if z[i..].starts_with(b"PK\x01\x02") {
            seen += 1;
            if seen == 2 {
                z[i + 42..i + 46].copy_from_slice(&0u32.to_le_bytes());
            }
        }
        i += 1;
    }
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::ZipStructure)));
    // Unpatched, the same file is fine.
    let ok = zip_of(
        &[
            ("_rels/.rels", rels.as_bytes()),
            ("3D/m1.model", simple_model("millimeter").as_bytes()),
        ],
        CompressionMethod::Stored,
    );
    assert!(p(&ok).is_ok());
}

#[test]
fn bound_one_xml_token() {
    let deflated = |model: &str| {
        zip_of(
            &[
                ("_rels/.rels", RELS.as_bytes()),
                ("3D/3dmodel.model", model.as_bytes()),
            ],
            CompressionMethod::Deflated,
        )
    };
    let big = "x".repeat(3_000_000);
    let comment = simple_model("millimeter").replace("<build>", &format!("<!-- {big} --><build>"));
    assert_eq!(
        p(&deflated(&comment)),
        Err(MeshError::Refused(Bound::XmlToken))
    );
    let text = simple_model("millimeter")
        .replace("<build>", &format!("<metadata>{big}</metadata><build>"));
    assert_eq!(
        p(&deflated(&text)),
        Err(MeshError::Refused(Bound::XmlToken))
    );
    let name = simple_model("millimeter").replace("<build>", &format!("<{big}/><build>"));
    assert_eq!(
        p(&deflated(&name)),
        Err(MeshError::Refused(Bound::XmlToken))
    );
    // 500 KB in one token is fine.
    let ok = simple_model("millimeter").replace(
        "<build>",
        &format!("<!-- {} --><build>", "x".repeat(500_000)),
    );
    assert!(p(&deflated(&ok)).is_ok());
}

#[test]
fn bound_path_length_and_distinct_names() {
    let long = "a".repeat(MAX_PATH_BYTES + 1);
    let z = threemf(&model_wrap(
        "millimeter",
        "",
        &format!(r#"<item objectid="1" p:path="/{long}"/>"#),
    ));
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::PathLength)));
    // 65 distinct names (the root and 64 more) are refused even though no part exists.
    let items: String = (0..MAX_MODEL_PARTS)
        .map(|i| format!(r#"<item objectid="1" p:path="/3D/{i}.model"/>"#))
        .collect();
    let z = threemf(&model_wrap("millimeter", "", &items));
    assert_eq!(p(&z), Err(MeshError::Refused(Bound::ModelParts)));
}

#[test]
fn many_components_sharing_one_path_stay_cheap() {
    let part = model_wrap("millimeter", &tetra_object(7, "", ""), "");
    let path = format!("/3D/{}.model", "o".repeat(900));
    let comps: String = (0..20_000)
        .map(|_| format!(r#"<component objectid="7" p:path="{path}"/>"#))
        .collect();
    let res = format!(r#"<object id="1"><components>{comps}</components></object>"#);
    let root = model_wrap("millimeter", &res, r#"<item objectid="1"/>"#);
    let z = threemf_parts(&root, &[(path.trim_start_matches('/').to_string(), part)]);
    let t = std::time::Instant::now();
    assert_eq!(p(&z).unwrap().triangle_count(), 80_000);
    assert!(t.elapsed().as_secs_f64() < 5.0, "{:?}", t.elapsed());
}

#[test]
fn bound_material_tables() {
    let groups = |n: usize| -> Vec<u8> {
        let g: String = (0..n)
            .map(|i| format!(r#"<m:colorgroup id="{}"/>"#, i + 10))
            .collect();
        threemf(&model_wrap(
            "millimeter",
            &format!("{g}{}", tetra_object(1, "", "")),
            r#"<item objectid="1"/>"#,
        ))
    };
    assert!(p(&groups(MAX_GROUPS)).is_ok());
    assert_eq!(
        p(&groups(MAX_GROUPS + 1)),
        Err(MeshError::Refused(Bound::Groups))
    );
    let dup = format!(
        r#"<m:colorgroup id="5"/><basematerials id="5"/>{}"#,
        tetra_object(1, "", "")
    );
    assert!(matches!(
        p(&threemf(&model_wrap(
            "millimeter",
            &dup,
            r#"<item objectid="1"/>"#
        ))),
        Err(MeshError::Malformed(_))
    ));
}

#[test]
fn debug_of_a_mesh_prints_no_coordinate() {
    let m = p(&stl_binary(&[[
        0.0, 0.0, 0.0, 10.0, 20.0, 30.0, 7.77, 6.5432, 3.0,
    ]]))
    .unwrap();
    let d = format!("{m:?}");
    assert!(d.contains("triangles") && d.contains("StlBinary"), "{d}");
    assert!(!d.contains("7.77") && !d.contains("6.5432"), "{d}");
}

#[test]
fn ascii_stl_with_cr_only_or_crlf_line_ends() {
    let t = cube(0.0, 10.0);
    let lf = stl_ascii(&t);
    for sep in ["\r", "\r\n"] {
        let s = lf.replace('\n', sep);
        let m = p(s.as_bytes()).unwrap();
        assert_eq!(m.triangle_count(), 12, "{sep:?}");
        assert_eq!(m.positions, p(lf.as_bytes()).unwrap().positions);
    }
    // The line cap still holds with CR-only ends.
    let long = format!("solid x\r{}\rendsolid\r", " ".repeat(MAX_ASCII_LINE + 1));
    assert_eq!(
        p(long.as_bytes()),
        Err(MeshError::Refused(Bound::AsciiLine))
    );
}

#[test]
fn each_part_is_scaled_by_its_own_unit() {
    let part = model_wrap("inch", &tetra_object(1, "", ""), "");
    let root = model_wrap(
        "millimeter",
        "",
        r#"<item objectid="1" p:path="/3D/o.model"/>"#,
    );
    let z = threemf_parts(&root, &[("3D/o.model".to_string(), part)]);
    let m = p(&z).unwrap();
    assert!((m.max[0] - 254.0).abs() < 1e-3, "{:?}", m.max);
    // A centimetre root with a millimetre part: the part's vertices stay millimetres, the item's
    // translation (root unit) is centimetres.
    let part = model_wrap("millimeter", &tetra_object(1, "", ""), "");
    let root = model_wrap(
        "centimeter",
        "",
        r#"<item objectid="1" p:path="/3D/o.model" transform="1 0 0 0 1 0 0 0 1 1 0 0"/>"#,
    );
    let z = threemf_parts(&root, &[("3D/o.model".to_string(), part)]);
    let m = p(&z).unwrap();
    assert_eq!((m.min[0], m.max[0]), (10.0, 20.0));
}

#[test]
fn a_nested_object_is_malformed() {
    let inner = tetra_object(2, "", "");
    let res = format!(r#"<object id="1">{inner}</object>"#);
    let z = threemf(&model_wrap("millimeter", &res, r#"<item objectid="1"/>"#));
    assert!(matches!(p(&z), Err(MeshError::Malformed(_))));
}

#[test]
fn core_attributes_are_unprefixed_and_not_duplicated() {
    // `xmlns:id` is not the object's id.
    let res =
        tetra_object(1, "", "").replace(r#"<object id="1""#, r#"<object xmlns:id="9" id="1""#);
    let z = threemf(&model_wrap("millimeter", &res, r#"<item objectid="1"/>"#));
    assert_eq!(p(&z).unwrap().triangle_count(), 4);
    // A prefixed `q:id` alone is no id.
    let res =
        tetra_object(1, "", "").replace(r#"<object id="1""#, r#"<object xmlns:q="urn:q" q:id="1""#);
    let z = threemf(&model_wrap("millimeter", &res, r#"<item objectid="1"/>"#));
    assert!(matches!(p(&z), Err(MeshError::Malformed(_))));
    // A duplicate attribute is refused, not first- or last-wins.
    let res = tetra_object(1, "", "").replace(
        r#"<vertex x="10" y="0" z="0"/>"#,
        r#"<vertex x="10" x="99" y="0" z="0"/>"#,
    );
    let z = threemf(&model_wrap("millimeter", &res, r#"<item objectid="1"/>"#));
    assert!(matches!(p(&z), Err(MeshError::Malformed(_))));
    // `xmlns:path` is a declaration, not a part name.
    let z = threemf(&model_wrap(
        "millimeter",
        &tetra_object(1, "", ""),
        r#"<item objectid="1" xmlns:path="urn:x"/>"#,
    ));
    assert_eq!(p(&z).unwrap().triangle_count(), 4);
}

// ---- zip 8.6 advisories (RUSTSEC-2026-0367/-0368/-0369/-0429 are all in `extract`) ----
// The reader never extracts and never writes an entry to disk: it reads entries by name from
// the bytes in memory. These hold that a symlink entry and a lying declared size do no harm.

/// A 3MF whose parts are `parts`, plus symlink entries `links` (name, target).
fn zip_with_links(parts: &[(&str, &[u8])], links: &[(&str, &str)]) -> Vec<u8> {
    use std::io::Write;
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let o = zip::write::SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, data) in parts {
        z.start_file(*name, o).unwrap();
        z.write_all(data).unwrap();
    }
    for (name, target) in links {
        z.add_symlink(*name, *target, o).unwrap();
    }
    z.finish().unwrap().into_inner()
}

/// Sets the declared sizes of entry `name` in its local header and its central record.
fn patch_declared(z: &mut [u8], name: &str, compressed: Option<u32>, plain: Option<u32>) {
    let n = name.as_bytes();
    let mut i = 0;
    while i + 4 <= z.len() {
        if z[i..].starts_with(b"PK\x03\x04") && z.get(i + 30..).is_some_and(|s| s.starts_with(n)) {
            if let Some(c) = compressed {
                z[i + 18..i + 22].copy_from_slice(&c.to_le_bytes());
            }
            if let Some(u) = plain {
                z[i + 22..i + 26].copy_from_slice(&u.to_le_bytes());
            }
        }
        if z[i..].starts_with(b"PK\x01\x02") && z.get(i + 46..).is_some_and(|s| s.starts_with(n)) {
            if let Some(c) = compressed {
                z[i + 20..i + 24].copy_from_slice(&c.to_le_bytes());
            }
            if let Some(u) = plain {
                z[i + 24..i + 28].copy_from_slice(&u.to_le_bytes());
            }
        }
        i += 1;
    }
}

#[test]
fn zip_a_symlink_entry_is_never_followed() {
    let model = simple_model("millimeter");
    // A symlink beside the parts is ignored: the model reads as without it.
    let beside = zip_with_links(
        &[
            ("_rels/.rels", RELS.as_bytes()),
            ("3D/3dmodel.model", model.as_bytes()),
        ],
        &[("evil", "../../../../etc/passwd"), ("abs", "/etc/shadow")],
    );
    assert_eq!(p(&beside).unwrap().triangle_count(), 4);
    // A symlink AS the model part or the relationships part is only its target's text, which is
    // no model: refused, nothing outside the archive is read.
    for target in ["/etc/passwd", "../../outside.model", "3D/3dmodel.model"] {
        let as_model = zip_with_links(
            &[("_rels/.rels", RELS.as_bytes())],
            &[("3D/3dmodel.model", target)],
        );
        assert!(p(&as_model).is_err(), "model part linked to {target}");
        let as_rels = zip_with_links(
            &[("3D/3dmodel.model", model.as_bytes())],
            &[("_rels/.rels", target)],
        );
        assert!(p(&as_rels).is_err(), "rels linked to {target}");
    }
}

#[test]
fn zip_a_huge_declared_size_is_bounded() {
    use std::time::Instant;
    let model = simple_model("millimeter");
    let base = || {
        zip_with_links(
            &[
                ("_rels/.rels", RELS.as_bytes()),
                ("3D/3dmodel.model", model.as_bytes()),
            ],
            &[("link", "/etc/passwd")],
        )
    };
    let huge = 0xFFFF_FFF0u32;
    let cases: [(&str, Option<u32>, Option<u32>); 5] = [
        // The model part claims 4 GiB of plain text (it holds a few hundred bytes).
        ("3D/3dmodel.model", None, Some(huge)),
        // ... or 4 GiB of stored data, more than the file holds.
        ("3D/3dmodel.model", Some(huge), Some(huge)),
        // The small parts too.
        ("_rels/.rels", None, Some(huge)),
        ("_rels/.rels", Some(huge), Some(huge)),
        // RUSTSEC-2026-0429's shape: a symlink entry with a huge declared size (never read).
        ("link", Some(huge), Some(huge)),
    ];
    for (name, compressed, plain) in cases {
        let mut z = base();
        patch_declared(&mut z, name, compressed, plain);
        let t = Instant::now();
        let r = p(&z);
        assert!(
            t.elapsed().as_secs_f32() < 2.0,
            "{name} {compressed:?} {plain:?} took {:?}",
            t.elapsed()
        );
        // A lying compressed size is refused; a lying plain size reads no further than the data.
        if compressed.is_some() && name != "link" {
            assert!(r.is_err(), "{name} with a huge compressed size: {r:?}");
        }
        if let Ok(m) = r {
            assert_eq!(m.triangle_count(), 4, "{name}");
        }
    }
}
