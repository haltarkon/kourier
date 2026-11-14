//! Mutation proptests: `parse` never panics and every `Ok` mesh is within the caps.

use proptest::prelude::*;

use crate::testkit::*;
use crate::*;

fn check(r: Result<Mesh, MeshError>) {
    if let Ok(m) = r {
        assert_eq!(m.positions.len() % 9, 0);
        assert!(!m.positions.is_empty());
        assert!(m.triangle_count() <= MAX_TRIANGLES);
        assert!(m.positions.iter().all(|c| c.is_finite()));
        assert!(m.min.iter().chain(&m.max).all(|c| c.is_finite()));
        if let Some(c) = &m.colors {
            assert_eq!(c.len(), m.triangle_count());
        }
    }
}

fn mutate(mut v: Vec<u8>, ops: &[(u8, usize, u8)]) -> Vec<u8> {
    for &(kind, at, byte) in ops {
        if v.is_empty() {
            break;
        }
        let at = at % v.len();
        match kind % 4 {
            0 => v[at] ^= byte | 1,
            1 => v.truncate(at),
            2 => {
                let end = (at + byte as usize * 3).min(v.len());
                let chunk = v[at..end].to_vec();
                let to = (at + 7) % v.len();
                v.splice(to..to, chunk);
            }
            _ => v.insert(at, byte),
        }
    }
    v
}

fn ops() -> impl Strategy<Value = Vec<(u8, usize, u8)>> {
    prop::collection::vec((any::<u8>(), any::<usize>(), any::<u8>()), 1..6)
}

fn rich_model() -> String {
    let res = format!(
        r##"<basematerials id="1"><base name="a" displaycolor="#112233"/></basematerials>{}<object id="9"><components><component objectid="2" transform="1 0 0 0 1 0 0 0 1 1 2 3"/></components></object>"##,
        tetra_object(2, r#"pid="1" pindex="0""#, r#"pid="1" p1="0""#)
    );
    model_wrap("millimeter", &res, r#"<item objectid="9"/>"#)
}

const BAMBU_MS: &str = r#"<config><object id="9"><metadata key="extruder" value="2"/><part id="2" subtype="modifier_part"><metadata key="extruder" value="1"/></part></object></config>"#;
const BAMBU_PS: &str = r##"{"filament_colour":["#FF0000","#00FF00"]}"##;
const PRUSA_MS: &str = r#"<config><object id="9"><metadata type="object" key="extruder" value="2"/><volume firstid="0" lastid="1"><metadata type="volume" key="volume_type" value="NegativeVolume"/></volume></object></config>"#;
const PRUSA_PS: &str = "; filament_colour = #FF0000;\"#00;FF00\"\n; extruder_colour = ;#0000FF\n";

/// A stored 3MF that carries both flavours' config parts.
fn with_configs(bambu_ms: &[u8], prusa_ms: &[u8]) -> Vec<u8> {
    let model = rich_model();
    zip_of(
        &[
            ("_rels/.rels", RELS.as_bytes()),
            ("3D/3dmodel.model", model.as_bytes()),
            ("Metadata/model_settings.config", bambu_ms),
            ("Metadata/project_settings.config", BAMBU_PS.as_bytes()),
            ("Metadata/Slic3r_PE_model.config", prusa_ms),
            ("Metadata/Slic3r_PE.config", PRUSA_PS.as_bytes()),
        ],
        zip::CompressionMethod::Stored,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn mutated_binary_stl_never_panics(o in ops()) {
        check(parse(&mutate(stl_binary(&cube(0.0, 10.0)), &o), NameHint::Model));
    }

    #[test]
    fn mutated_ascii_stl_never_panics(o in ops()) {
        check(parse(&mutate(stl_ascii(&cube(0.0, 10.0)).into_bytes(), &o), NameHint::Model));
    }

    #[test]
    fn mutated_3mf_zip_never_panics(o in ops()) {
        check(parse(&mutate(threemf(&rich_model()), &o), NameHint::Model));
    }

    #[test]
    fn mutated_3mf_xml_in_a_valid_zip_never_panics(o in ops()) {
        let xml = mutate(rich_model().into_bytes(), &o);
        let z = zip_of(
            &[("_rels/.rels", RELS.as_bytes()), ("3D/3dmodel.model", &xml)],
            zip::CompressionMethod::Deflated,
        );
        check(parse(&z, NameHint::Model));
    }

    #[test]
    fn any_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..400)) {
        check(parse(&bytes, NameHint::Model));
        check(parse(&bytes, NameHint::None));
    }

    #[test]
    fn mutated_3mf_with_slicer_configs_never_panics(o in ops()) {
        check(parse(&mutate(with_configs(BAMBU_MS.as_bytes(), PRUSA_MS.as_bytes()), &o), NameHint::Model));
    }

    #[test]
    fn mutated_slicer_config_parts_in_a_valid_zip_never_panic(o in ops(), p in ops()) {
        let b = mutate(BAMBU_MS.as_bytes().to_vec(), &o);
        let r = mutate(PRUSA_MS.as_bytes().to_vec(), &p);
        check(parse(&with_configs(&b, &r), NameHint::Model));
        // A Prusa-only file too.
        let model = rich_model();
        let z = zip_of(
            &[
                ("_rels/.rels", RELS.as_bytes()),
                ("3D/3dmodel.model", model.as_bytes()),
                ("Metadata/Slic3r_PE_model.config", &r),
                ("Metadata/Slic3r_PE.config", PRUSA_PS.as_bytes()),
            ],
            zip::CompressionMethod::Stored,
        );
        check(parse(&z, NameHint::Model));
    }
}
