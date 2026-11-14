//! Slicer filament colours and unprinted parts (V5). Fixtures are hand-made equivalents of what
//! Bambu Studio, OrcaSlicer and PrusaSlicer write (`notes/kourier/viewer-3d-slicer-keys.md`).

use crate::testkit::*;
use crate::*;

const OBJ_PATH: &str = "3D/Objects/object_1.model";

fn p(bytes: &[u8]) -> Result<Mesh, MeshError> {
    parse(bytes, NameHint::Model)
}

fn parts_of(m: &Mesh) -> Vec<u32> {
    m.colors
        .clone()
        .unwrap_or_else(|| vec![0; m.triangle_count()])
}

fn rep(c: u32, n: usize) -> Vec<u32> {
    vec![c; n]
}

/// Root: build objects 10 (components 1, 2) and 20 (components 3, 4) of `object_1.model`.
fn bambu_files(extra: &[(&str, &str)]) -> Vec<u8> {
    let sub = model_wrap(
        "millimeter",
        &(1..=4).map(|i| tetra_object(i, "", "")).collect::<String>(),
        "",
    );
    let comp = |id: u32| format!(r#"<component objectid="{id}" p:path="/{OBJ_PATH}"/>"#);
    let res = format!(
        r#"<object id="10"><components>{}{}</components></object><object id="20"><components>{}{}</components></object>"#,
        comp(1),
        comp(2),
        comp(3),
        comp(4)
    );
    let root = model_wrap(
        "millimeter",
        &res,
        r#"<item objectid="10"/><item objectid="20"/>"#,
    );
    let mut files = vec![(OBJ_PATH.to_string(), sub)];
    for (n, d) in extra {
        files.push((n.to_string(), d.to_string()));
    }
    threemf_parts(&root, &files)
}

fn bambu(model_settings: &str, project: &str) -> Vec<u8> {
    bambu_files(&[
        ("Metadata/model_settings.config", model_settings),
        ("Metadata/project_settings.config", project),
    ])
}

const JSON3: &str = r##"{"other":{"deep":[1,2,{"a":null}]},"filament_colour":["#FF0000","#00FF00","#0000FF"],"x":1}"##;

fn settings(body: &str) -> String {
    format!(r#"<?xml version="1.0" encoding="UTF-8"?><config>{body}</config>"#)
}

fn meta(key: &str, value: &str) -> String {
    format!(r#"<metadata key="{key}" value="{value}"/>"#)
}

/// Object 10 extruder 1, object 20 extruder 2 with part 4 on 3; a plate and a slice result around.
fn bambu_settings() -> String {
    settings(&format!(
        r##"<object id="10">{n}{e1}<part id="1" subtype="normal_part">{n}</part><part id="2" subtype="normal_part"/></object><object id="20">{e2}<part id="3" subtype="normal_part"/><part id="4" subtype="normal_part">{e3}</part></object><plate><metadata key="plater_id" value="1"/><model_instance><metadata key="object_id" value="10"/></model_instance><filament id="1" color="#123456"/></plate><assemble><assemble_item object_id="10"/></assemble>"##,
        n = meta("name", "x"),
        e1 = meta("extruder", "1"),
        e2 = meta("extruder", "2"),
        e3 = meta("extruder", "3"),
    ))
}

fn expect_bambu() -> Vec<u32> {
    let mut v = rep(0xFF0000FF, 8);
    v.extend(rep(0x00FF00FF, 4));
    v.extend(rep(0x0000FFFF, 4));
    v
}

fn plain_bambu() -> Mesh {
    p(&bambu_files(&[])).unwrap()
}

#[test]
fn bambu_colours_per_object_and_part() {
    let m = p(&bambu(&bambu_settings(), JSON3)).unwrap();
    assert_eq!(m.triangle_count(), 16);
    assert_eq!(parts_of(&m), expect_bambu());
    assert!(plain_bambu().colors.is_none());
}

#[test]
fn orca_style_default_has_no_part_extruders() {
    let ms = settings(&format!(
        r#"<object id="10">{}<part id="1"/><part id="2"/></object><object id="20">{}<part id="3"/><part id="4"/></object>"#,
        meta("extruder", "1"),
        meta("extruder", "2"),
    ));
    let m = p(&bambu(&ms, JSON3)).unwrap();
    let mut v = rep(0xFF0000FF, 8);
    v.extend(rep(0x00FF00FF, 8));
    assert_eq!(parts_of(&m), v);
}

#[test]
fn the_old_ini_project_settings_form() {
    let ini = "; comment\nnozzle = 0.4\nfilament_colour = #FF0000;#00FF00;#0000FF\n";
    let m = p(&bambu(&bambu_settings(), ini)).unwrap();
    assert_eq!(parts_of(&m), expect_bambu());
}

#[test]
fn bambu_an_old_file_uses_a_triangle_range_of_the_objects_own_triangles() {
    // One object with its own 4 triangles, two parts as ranges, the second on filament 2.
    let root = model_wrap(
        "millimeter",
        &tetra_object(1, "", ""),
        r#"<item objectid="1"/>"#,
    );
    let ms = settings(&format!(
        r#"<object id="1">{}<part id="1" firstid="0" lastid="1"/><part id="2" firstid="2" lastid="3">{}</part></object>"#,
        meta("extruder", "1"),
        meta("extruder", "2"),
    ));
    let z = threemf_parts(
        &root,
        &[
            ("Metadata/model_settings.config".into(), ms),
            ("Metadata/project_settings.config".into(), JSON3.into()),
        ],
    );
    let mut v = rep(0xFF0000FF, 2);
    v.extend(rep(0x00FF00FF, 2));
    assert_eq!(parts_of(&p(&z).unwrap()), v);
}

#[test]
fn bambu_wins_when_both_flavours_are_there() {
    let prusa_model = r#"<config><object id="10"><metadata type="object" key="extruder" value="3"/></object></config>"#;
    let bs = bambu_settings();
    let z = bambu_files(&[
        ("Metadata/model_settings.config", &bs),
        ("Metadata/project_settings.config", JSON3),
        ("Metadata/Slic3r_PE_model.config", prusa_model),
        ("Metadata/Slic3r_PE.config", "; filament_colour = #ABCDEF\n"),
    ]);
    assert_eq!(parts_of(&p(&z).unwrap()), expect_bambu());
}

#[test]
fn an_alpha_colour_is_kept_and_a_short_one_is_none() {
    let ms = settings(&format!(
        r#"<object id="10">{}</object><object id="20">{}</object>"#,
        meta("extruder", "1"),
        meta("extruder", "2"),
    ));
    let m = p(&bambu(&ms, r##"{"filament_colour":["#11223344","#FFF"]}"##)).unwrap();
    let mut v = rep(0x11223344, 8);
    v.extend(rep(0, 8));
    assert_eq!(parts_of(&m), v);
}

fn tri_attrs_model() -> String {
    // One object, the triangles 0 and 1 carry a standard colour of their own.
    let res = format!(
        r##"<basematerials id="1"><base name="a" displaycolor="#A0B0C0"/></basematerials>{}"##,
        tetra_object(1, "", "").replacen(
            r#"<triangle v1="0" v2="2" v3="1" />"#,
            r#"<triangle v1="0" v2="2" v3="1" pid="1" p1="0"/>"#,
            1
        )
    );
    model_wrap("millimeter", &res, r#"<item objectid="1"/>"#)
}

#[test]
fn a_standard_colour_wins_over_a_filament_colour() {
    let ms = settings(&format!(
        r#"<object id="1">{}</object>"#,
        meta("extruder", "2")
    ));
    let z = threemf_parts(
        &tri_attrs_model(),
        &[
            ("Metadata/model_settings.config".into(), ms),
            ("Metadata/project_settings.config".into(), JSON3.into()),
        ],
    );
    let mut v = vec![0xA0B0C0FF];
    v.extend(rep(0x00FF00FF, 3));
    assert_eq!(parts_of(&p(&z).unwrap()), v);
}

#[test]
fn instances_share_their_objects_colours() {
    let root = model_wrap(
        "millimeter",
        &tetra_object(1, "", ""),
        r#"<item objectid="1"/><item objectid="1" transform="1 0 0 0 1 0 0 0 1 20 0 0"/>"#,
    );
    let ms = settings(&format!(
        r#"<object id="1">{}</object>"#,
        meta("extruder", "3")
    ));
    let z = threemf_parts(
        &root,
        &[
            ("Metadata/model_settings.config".into(), ms),
            ("Metadata/project_settings.config".into(), JSON3.into()),
        ],
    );
    assert_eq!(parts_of(&p(&z).unwrap()), rep(0x0000FFFF, 8));
}

// ---------- ignored configs ----------

/// The config is ignored: the mesh is the one without the config parts, colours included.
fn assert_ignored(model_settings: &str, project: &str) {
    let m = p(&bambu(model_settings, project)).unwrap();
    assert_eq!(m, plain_bambu());
    assert!(m.colors.is_none());
}

#[test]
fn bambu_a_bad_config_is_ignored() {
    let good = bambu_settings();
    // The good fixture does colour.
    assert!(p(&bambu(&good, JSON3)).unwrap().colors.is_some());
    assert_ignored(&good, r##"{"filament_colour": ["#FF0000","#00"##);
    assert_ignored(&good, r##"{"filament_colour": "#FF0000"}"##);
    assert_ignored(&good, r##"{"filament_colour": [1, 2]}"##);
    assert_ignored(&good, "");
    assert_ignored(
        &format!(r#"<!DOCTYPE config [<!ENTITY a "b">]>{good}"#),
        JSON3,
    );
}

#[test]
fn bambu_malformed_depth_size_and_utf8_are_ignored() {
    let good = bambu_settings();
    assert_ignored(&good.replace("</config>", ""), JSON3);
    let deep = format!(
        "{}{}{}",
        good.replace("</config>", ""),
        "<x>".repeat(40),
        "</x>".repeat(40)
    ) + "</config>";
    assert_ignored(&deep, JSON3);
    // A config part of MAX + 1 bytes is ignored, MAX bytes still read.
    let pad = |total: usize| {
        let mut s = good.clone();
        let fill = total - s.len();
        s.push_str(&" ".repeat(fill));
        s
    };
    let max = MAX_CONFIG_PART_BYTES as usize;
    assert!(p(&bambu(&pad(max), JSON3)).unwrap().colors.is_some());
    assert_ignored(&pad(max + 1), JSON3);
    // Not UTF-8.
    let bad = threemf_parts_bytes(&good, b"\xFF\xFE{\"filament_colour\":[\"#FF0000\"]}");
    assert_eq!(p(&bad).unwrap(), plain_bambu());
}

fn threemf_parts_bytes(model_settings: &str, project: &[u8]) -> Vec<u8> {
    let base = bambu_files(&[]);
    // Rebuild the same ZIP with the two config parts added (raw bytes for the project part).
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(base)).unwrap();
    let mut items: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut b).unwrap();
        items.push((f.name().unwrap().to_string(), b));
    }
    items.push((
        "Metadata/model_settings.config".into(),
        model_settings.into(),
    ));
    items.push(("Metadata/project_settings.config".into(), project.to_vec()));
    let refs: Vec<(&str, &[u8])> = items
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    zip_of(&refs, zip::CompressionMethod::Stored)
}

#[test]
fn bambu_257_colours_ignore_the_config_and_256_are_read() {
    let list = |n: usize| {
        let c: Vec<String> = (0..n).map(|_| r##""#FF0000""##.to_string()).collect();
        format!(r#"{{"filament_colour":[{}]}}"#, c.join(","))
    };
    assert!(
        p(&bambu(&bambu_settings(), &list(MAX_COLOURS)))
            .unwrap()
            .colors
            .is_some()
    );
    assert_ignored(&bambu_settings(), &list(MAX_COLOURS + 1));
}

#[test]
fn bambu_a_non_hex_colour_is_no_colour_for_that_entry_only() {
    let ms = settings(&format!(
        r#"<object id="10">{}</object><object id="20">{}</object>"#,
        meta("extruder", "1"),
        meta("extruder", "2"),
    ));
    let m = p(&bambu(
        &ms,
        r##"{"filament_colour":["#FF0000","red","#0000FF"]}"##,
    ))
    .unwrap();
    let mut v = rep(0xFF0000FF, 8);
    v.extend(rep(0, 8));
    assert_eq!(parts_of(&m), v);
    // Only the bad entry: the mesh equals the plain one.
    let one = settings(&format!(
        r#"<object id="10">{}</object>"#,
        meta("extruder", "1")
    ));
    assert_eq!(
        p(&bambu(&one, r##"{"filament_colour":["#FFF"]}"##)).unwrap(),
        plain_bambu()
    );
}

#[test]
fn bambu_extruder_zero_or_past_the_list_is_filament_one() {
    for e in ["0", "9"] {
        let ms = settings(&format!(
            r#"<object id="10">{}</object><object id="20">{}</object>"#,
            meta("extruder", e),
            meta("extruder", "2"),
        ));
        let m = p(&bambu(&ms, JSON3)).unwrap();
        let mut v = rep(0xFF0000FF, 8);
        v.extend(rep(0x00FF00FF, 8));
        assert_eq!(parts_of(&m), v, "extruder {e}");
    }
    // A part: 0 inherits the object's, past the list is 1.
    let ms = settings(&format!(
        r#"<object id="20">{}<part id="3">{}</part><part id="4">{}</part></object>"#,
        meta("extruder", "2"),
        meta("extruder", "0"),
        meta("extruder", "7"),
    ));
    let v = parts_of(&p(&bambu(&ms, JSON3)).unwrap());
    // Object 10 has no entry: it takes filament 1.
    assert_eq!(&v[8..12], &rep(0x00FF00FF, 4)[..]);
    assert_eq!(&v[12..16], &rep(0xFF0000FF, 4)[..]);
    // A wrong-shaped extruder ignores the config.
    let bad = settings(&format!(
        r#"<object id="10">{}</object>"#,
        meta("extruder", "two")
    ));
    assert_ignored(&bad, JSON3);
}

// ---------- PrusaSlicer ----------

const PRUSA_PROJECT_CFG: &str = "; generated by PrusaSlicer\n; filament_colour = #FF0000;#00FF00;\"#AB;CD\";#112233\n; extruder_colour = ;#0000FF;;\n; other = 1\n";

fn prusa_files(extra: &[(&str, &str)]) -> Vec<u8> {
    let res = format!("{}{}", tetra_object(1, "", ""), tetra_object(2, "", ""));
    let root = model_wrap(
        "millimeter",
        &res,
        r#"<item objectid="1"/><item objectid="2"/>"#,
    );
    let files: Vec<(String, String)> = extra
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    threemf_parts(&root, &files)
}

fn prusa(model: &str, project: &str) -> Vec<u8> {
    prusa_files(&[
        ("Metadata/Slic3r_PE_model.config", model),
        ("Metadata/Slic3r_PE.config", project),
    ])
}

fn pmeta(ty: &str, key: &str, value: &str) -> String {
    format!(r#"<metadata type="{ty}" key="{key}" value="{value}"/>"#)
}

fn prusa_model() -> String {
    settings(&format!(
        r#"<object id="1" instances_count="1">{n}{e4}<volume firstid="0" lastid="1">{vt}</volume><volume firstid="2" lastid="3">{e2}{vt}<mesh edges_fixed="0"/></volume></object><object id="2">{e1}</object>"#,
        n = pmeta("object", "name", "x"),
        e4 = pmeta("object", "extruder", "4"),
        e2 = pmeta("volume", "extruder", "2"),
        e1 = pmeta("object", "extruder", "1"),
        vt = pmeta("volume", "volume_type", "ModelPart"),
    ))
}

fn plain_prusa() -> Mesh {
    p(&prusa_files(&[])).unwrap()
}

#[test]
fn prusa_volumes_ranges_fallbacks_and_quoted_elements() {
    let m = p(&prusa(&prusa_model(), PRUSA_PROJECT_CFG)).unwrap();
    // Object 1: volume 0..=1 on the object's extruder 4 (extruder entry empty, filament 4); volume 2..=3 on 2
    // (extruder_colour 2). Object 2: extruder 1 (extruder entry empty, filament 1).
    let mut v = rep(0x112233FF, 2);
    v.extend(rep(0x0000FFFF, 2));
    v.extend(rep(0xFF0000FF, 4));
    assert_eq!(parts_of(&m), v);
    // Extruder 3: the quoted element with `;` is no colour.
    let model = settings(&format!(
        r#"<object id="1">{}</object>"#,
        pmeta("object", "extruder", "3")
    ));
    let m = p(&prusa(&model, PRUSA_PROJECT_CFG)).unwrap();
    assert!(m.colors.is_none());
    // The element after it is still the fourth.
    let model = settings(&format!(
        r#"<object id="2">{}</object>"#,
        pmeta("object", "extruder", "4")
    ));
    let mut v = rep(0, 4);
    v.extend(rep(0x112233FF, 4));
    assert_eq!(parts_of(&p(&prusa(&model, PRUSA_PROJECT_CFG)).unwrap()), v);
}

#[test]
fn prusa_escapes_and_a_lone_empty_element() {
    let cfg = "; extruder_colour = \"\"\n; filament_colour = \"#FF\\\"00\";#00FF00\n";
    let model = settings(&format!(
        r#"<object id="1">{}</object>"#,
        pmeta("object", "extruder", "2")
    ));
    let m = p(&prusa(&model, cfg)).unwrap();
    assert_eq!(&parts_of(&m)[..4], &rep(0x00FF00FF, 4)[..]);
}

#[test]
fn prusa_extruder_zero_is_one_and_past_the_lists_is_none() {
    let zero = settings(&format!(
        r#"<object id="1">{}<volume firstid="0" lastid="3">{}</volume></object>"#,
        pmeta("object", "extruder", "0"),
        pmeta("volume", "extruder", "0"),
    ));
    let m = p(&prusa(&zero, PRUSA_PROJECT_CFG)).unwrap();
    assert_eq!(&parts_of(&m)[..4], &rep(0xFF0000FF, 4)[..]);
    let past = settings(&format!(
        r#"<object id="1">{}</object><object id="2">{}</object>"#,
        pmeta("object", "extruder", "5"),
        pmeta("object", "extruder", "6"),
    ));
    assert_eq!(p(&prusa(&past, PRUSA_PROJECT_CFG)).unwrap(), plain_prusa());
}

#[test]
fn prusa_a_bad_range_is_ignored() {
    // The object's extruder is past both lists, so only a range could colour.
    let model = |a: u32, b: u32| {
        settings(&format!(
            r#"<object id="1">{}<volume firstid="{a}" lastid="{b}">{}</volume></object>"#,
            pmeta("object", "extruder", "5"),
            pmeta("volume", "extruder", "1"),
        ))
    };
    assert!(
        p(&prusa(&model(0, 3), PRUSA_PROJECT_CFG))
            .unwrap()
            .colors
            .is_some()
    );
    for (a, b) in [(0, 4), (2, 9), (3, 1)] {
        assert_eq!(
            p(&prusa(&model(a, b), PRUSA_PROJECT_CFG)).unwrap(),
            plain_prusa(),
            "{a}..={b}"
        );
    }
}

#[test]
fn prusa_a_bad_config_is_ignored() {
    let good = prusa_model();
    assert!(
        p(&prusa(&good, PRUSA_PROJECT_CFG))
            .unwrap()
            .colors
            .is_some()
    );
    for bad in [
        format!(r#"<!DOCTYPE config [<!ENTITY a "b">]>{good}"#),
        good.replace("</config>", ""),
        good.replace(r#"firstid="0""#, r#"firstid="zero""#),
    ] {
        assert_eq!(p(&prusa(&bad, PRUSA_PROJECT_CFG)).unwrap(), plain_prusa());
    }
    let long: Vec<String> = (0..MAX_COLOURS + 1)
        .map(|_| "#FF0000".to_string())
        .collect();
    let cfg = format!("; filament_colour = {}\n", long.join(";"));
    assert_eq!(p(&prusa(&good, &cfg)).unwrap(), plain_prusa());
    // Only one of the pair: no flavour.
    let one = prusa_files(&[("Metadata/Slic3r_PE_model.config", &good)]);
    assert_eq!(p(&one).unwrap(), plain_prusa());
}

// ---------- parts the slicer does not print ----------

#[test]
fn bambu_modifier_parts_are_dropped_unless_they_are_all() {
    let ms = |s2: &str, s1: &str| {
        settings(&format!(
            r#"<object id="10">{}<part id="1" subtype="{s1}"/><part id="2" subtype="{s2}"/></object><object id="20"><part id="3" subtype="normal_part"/><part id="4"/></object>"#,
            meta("extruder", "1"),
        ))
    };
    let plain_all = plain_bambu();
    let m = p(&bambu(&ms("negative_part", "normal_part"), JSON3)).unwrap();
    assert_eq!(m.triangle_count(), 12);
    // Part 2 (triangles 4..8) is gone: the mesh is the plain one without them.
    assert_eq!(&m.positions[..36], &plain_all.positions[..36]);
    assert_eq!(&m.positions[36..], &plain_all.positions[72..]);
    for s in ["modifier_part", "support_blocker", "support_enforcer"] {
        assert_eq!(
            p(&bambu(&ms(s, "normal_part"), JSON3))
                .unwrap()
                .triangle_count(),
            12,
            "{s}"
        );
    }
    // An unknown subtype keeps the triangles.
    assert_eq!(
        p(&bambu(&ms("brand_new", "normal_part"), JSON3))
            .unwrap()
            .triangle_count(),
        16
    );
    // Every part of every object a modifier: the mesh is as without the config.
    let all = settings(
        r#"<object id="10"><part id="1" subtype="modifier_part"/><part id="2" subtype="modifier_part"/></object><object id="20"><part id="3" subtype="negative_part"/><part id="4" subtype="modifier_part"/></object>"#,
    );
    assert_eq!(p(&bambu(&all, JSON3)).unwrap(), plain_bambu());
}

#[test]
fn prusa_modifier_volumes_are_dropped_unless_they_are_all() {
    let model = |t0: &str, t1: &str| {
        settings(&format!(
            r#"<object id="1"><volume firstid="0" lastid="1">{}</volume><volume firstid="2" lastid="3">{}</volume></object>"#,
            pmeta("volume", "volume_type", t0),
            pmeta("volume", "volume_type", t1),
        ))
    };
    let plain = plain_prusa();
    for t in [
        "NegativeVolume",
        "ParameterModifier",
        "SupportBlocker",
        "SupportEnforcer",
    ] {
        let m = p(&prusa(&model("ModelPart", t), PRUSA_PROJECT_CFG)).unwrap();
        assert_eq!(m.triangle_count(), 6, "{t}");
        assert_eq!(&m.positions[..18], &plain.positions[..18]);
        assert_eq!(&m.positions[18..], &plain.positions[36..]);
    }
    let unknown = p(&prusa(&model("ModelPart", "Future"), PRUSA_PROJECT_CFG)).unwrap();
    assert_eq!(unknown.triangle_count(), 8);
    // The old form: `modifier` = 1; `volume_type` overrides it.
    let old = settings(&format!(
        r#"<object id="1"><volume firstid="0" lastid="1">{}</volume><volume firstid="2" lastid="3">{}{}</volume></object>"#,
        pmeta("volume", "modifier", "0"),
        pmeta("volume", "modifier", "1"),
        pmeta("volume", "volume_type", "ModelPart"),
    ));
    assert_eq!(
        p(&prusa(&old, PRUSA_PROJECT_CFG)).unwrap().triangle_count(),
        8
    );
    let old = old.replace(&pmeta("volume", "volume_type", "ModelPart"), "");
    assert_eq!(
        p(&prusa(&old, PRUSA_PROJECT_CFG)).unwrap().triangle_count(),
        6
    );
    // Both volumes modifiers: as without the config.
    let all = model("NegativeVolume", "ParameterModifier").replace(
        "</config>",
        &format!(
            r#"<object id="2"><volume firstid="0" lastid="3">{}</volume></object></config>"#,
            pmeta("volume", "volume_type", "SupportBlocker")
        ),
    );
    assert_eq!(p(&prusa(&all, PRUSA_PROJECT_CFG)).unwrap(), plain);
}

#[test]
fn the_config_parts_read_stay_within_the_count() {
    // 4 of 8 at most, whatever the file holds.
    const { assert!(MAX_CONFIG_PARTS >= 4) };
}
