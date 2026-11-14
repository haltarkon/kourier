//! Fixtures built inside the tests (never committed as files).

use std::io::Write;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

pub const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Target="/3D/3dmodel.model" Id="rel0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/></Relationships>"#;
pub const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/></Types>"#;

pub fn zip_of(parts: &[(&str, &[u8])], method: CompressionMethod) -> Vec<u8> {
    let mut z = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let o = SimpleFileOptions::default().compression_method(method);
    for (name, data) in parts {
        z.start_file(*name, o).unwrap();
        z.write_all(data).unwrap();
    }
    z.finish().unwrap().into_inner()
}

/// A 3MF whose root model is `model` (stored, so byte mutations reach the XML).
pub fn threemf(model: &str) -> Vec<u8> {
    zip_of(
        &[
            ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
            ("_rels/.rels", RELS.as_bytes()),
            ("3D/3dmodel.model", model.as_bytes()),
        ],
        CompressionMethod::Stored,
    )
}

pub fn threemf_parts(model: &str, extra: &[(String, String)]) -> Vec<u8> {
    let mut parts: Vec<(&str, &[u8])> = vec![
        ("_rels/.rels", RELS.as_bytes()),
        ("3D/3dmodel.model", model.as_bytes()),
    ];
    for (n, d) in extra {
        parts.push((n.as_str(), d.as_bytes()));
    }
    zip_of(&parts, CompressionMethod::Stored)
}

pub fn model_wrap(unit: &str, resources: &str, build: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><model unit="{unit}" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02" xmlns:p="http://schemas.microsoft.com/3dmanufacturing/production/2015/06" xmlns:m="http://schemas.microsoft.com/3dmanufacturing/material/2015/02"><resources>{resources}</resources><build>{build}</build></model>"#
    )
}

/// A unit-ish tetrahedron object with `id`, vertices at 0/10 mm, `tri_attrs` appended to each triangle.
pub fn tetra_object(id: u32, obj_attrs: &str, tri_attrs: &str) -> String {
    format!(
        r#"<object id="{id}" type="model" {obj_attrs}><mesh><vertices><vertex x="0" y="0" z="0"/><vertex x="10" y="0" z="0"/><vertex x="0" y="10" z="0"/><vertex x="0" y="0" z="10"/></vertices><triangles><triangle v1="0" v2="2" v3="1" {tri_attrs}/><triangle v1="0" v2="1" v3="3" {tri_attrs}/><triangle v1="1" v2="2" v3="3" {tri_attrs}/><triangle v1="2" v2="0" v3="3" {tri_attrs}/></triangles></mesh></object>"#
    )
}

pub fn simple_model(unit: &str) -> String {
    model_wrap(unit, &tetra_object(1, "", ""), r#"<item objectid="1"/>"#)
}

/// Binary STL of the given triangles (9 floats each).
pub fn stl_binary(tris: &[[f32; 9]]) -> Vec<u8> {
    let mut v = vec![0u8; 80];
    v.extend_from_slice(&(tris.len() as u32).to_le_bytes());
    for t in tris {
        v.extend_from_slice(&[0u8; 12]);
        for c in t {
            v.extend_from_slice(&c.to_le_bytes());
        }
        v.extend_from_slice(&[0u8; 2]);
    }
    v
}

pub fn stl_ascii(tris: &[[f32; 9]]) -> String {
    let mut s = String::from("solid t\n");
    for t in tris {
        s.push_str("facet normal 0 0 0\nouter loop\n");
        for k in 0..3 {
            s.push_str(&format!(
                "vertex {} {} {}\n",
                t[3 * k],
                t[3 * k + 1],
                t[3 * k + 2]
            ));
        }
        s.push_str("endloop\nendfacet\n");
    }
    s.push_str("endsolid t\n");
    s
}

/// The 12 triangles of an axis-aligned box from `lo` to `hi`.
pub fn cube(lo: f32, hi: f32) -> Vec<[f32; 9]> {
    let p = |i: usize| -> [f32; 3] {
        [
            if i & 1 == 0 { lo } else { hi },
            if i & 2 == 0 { lo } else { hi },
            if i & 4 == 0 { lo } else { hi },
        ]
    };
    let quads = [
        [0, 1, 3, 2],
        [4, 6, 7, 5],
        [0, 4, 5, 1],
        [2, 3, 7, 6],
        [0, 2, 6, 4],
        [1, 5, 7, 3],
    ];
    let mut out = Vec::new();
    for q in quads {
        for tri in [[q[0], q[1], q[2]], [q[0], q[2], q[3]]] {
            let mut t = [0f32; 9];
            for (k, &i) in tri.iter().enumerate() {
                t[3 * k..3 * k + 3].copy_from_slice(&p(i));
            }
            out.push(t);
        }
    }
    out
}
