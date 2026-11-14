//! Qt-free 3D model parsing and a software still renderer (binary and ASCII STL, 3MF).
//!
//! `parse` picks the parser by CONTENT; the name or MIME only says whether to try
//! (`looks_like_model`, `NameHint`). Every bound in the design table is a `pub const` below and is
//! enforced by the parsers; a refusal is a typed `MeshError` that never carries file content.
//! `render_still` draws a deterministic orthographic picture without a GPU.

mod raster;
mod slicer;
mod stl;
mod threemf;

#[cfg(test)]
mod prop_tests;
#[cfg(test)]
mod slicer_tests;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;

pub use raster::{Still, StillMode, encode_png, render_still};

/// Largest file `parse` looks at (256 MiB): 2 M binary triangles are 100 MB, ASCII is about 4x.
pub const MAX_FILE_BYTES: usize = 256 << 20;
/// Most triangles a mesh may hold, after 3MF instancing (counted before a non-finite one is dropped).
pub const MAX_TRIANGLES: usize = 2_000_000;
/// Longest ASCII STL line in bytes.
pub const MAX_ASCII_LINE: usize = 4096;
/// Most entries in a 3MF ZIP (central directory bomb).
pub const MAX_ZIP_ENTRIES: usize = 4096;
/// Most uncompressed bytes of all 3MF model XML parts together; the header's size is never trusted.
pub const MAX_MODEL_XML_TOTAL: u64 = 512 << 20;
/// Largest `_rels/.rels` or `[Content_Types].xml` in bytes.
pub const MAX_SMALL_PART_BYTES: u64 = 64 << 10;
/// Largest slicer config part (filament colours) in bytes; a bigger one is ignored, not a refusal.
pub const MAX_CONFIG_PART_BYTES: u64 = 4 << 20;
/// Most slicer config parts read in all.
pub const MAX_CONFIG_PARTS: usize = 8;
/// Longest single XML token (text, comment, CDATA, PI, tag) in bytes: quick-xml buffers a whole
/// token, so one 512 MiB token would be 512 MiB of RAM. The reader's read-ahead (64 KiB) comes on top.
pub const MAX_XML_TOKEN: u64 = 1 << 20;
/// Longest `p:path` attribute (a model part name) in bytes.
pub const MAX_PATH_BYTES: usize = 1024;
/// Most `basematerials` and `m:colorgroup` tables of a file (a table without colours counts too).
pub const MAX_GROUPS: usize = 256;
/// Deepest XML element nesting; `<!DOCTYPE` is refused outright (no entities are ever expanded).
pub const MAX_XML_DEPTH: u32 = 32;
/// Most 3MF model parts (the root and the production extension's `p:path` parts), each read once.
pub const MAX_MODEL_PARTS: usize = 64;
/// Deepest 3MF component nesting below a build item; a cycle is refused.
pub const MAX_COMPONENT_DEPTH: usize = 16;
/// Most colours in all `basematerials` and `m:colorgroup` tables of a file.
pub const MAX_COLOURS: usize = 256;
/// Most vertices stored by a 3MF (all parts together).
pub const MAX_VERTICES: usize = 3 * MAX_TRIANGLES;
/// Most `<object>` elements in a 3MF.
pub const MAX_OBJECTS: usize = 100_000;
/// Most build items and components stored, and most object visits while instancing.
pub const MAX_INSTANCES: usize = 1_000_000;
/// Smallest and largest edge of a still, in pixels (`render_still` clamps).
pub const MIN_STILL_EDGE: u32 = 8;
/// See `MIN_STILL_EDGE`.
pub const MAX_STILL_EDGE: u32 = 1024;
/// Most bounding-box pixels the rasterizer visits (stacked huge triangles); the rest is skipped.
pub const MAX_RASTER_WORK: u64 = 300_000_000;

/// The bound a refusal names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Bound {
    #[error("too many triangles")]
    Triangles,
    #[error("ASCII line too long")]
    AsciiLine,
    #[error("too many ZIP entries")]
    ZipEntries,
    #[error("odd ZIP structure")]
    ZipStructure,
    #[error("XML token too long")]
    XmlToken,
    #[error("model part name too long")]
    PathLength,
    #[error("too many material tables")]
    Groups,
    #[error("model XML too large")]
    XmlTotal,
    #[error("package part too large")]
    SmallPart,
    #[error("XML nested too deeply")]
    XmlDepth,
    #[error("DOCTYPE refused")]
    DocType,
    #[error("vertex index out of range")]
    VertexIndex,
    #[error("too many model parts")]
    ModelParts,
    #[error("components nested too deeply")]
    ComponentDepth,
    #[error("component cycle")]
    ComponentCycle,
    #[error("bad transform")]
    Transform,
    #[error("unknown unit")]
    Unit,
    #[error("too many colours")]
    Colours,
    #[error("too many vertices")]
    Vertices,
    #[error("too many objects")]
    Objects,
    #[error("too many instances")]
    Instances,
}

/// Why a file is not a mesh. Never carries file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MeshError {
    #[error("model file too large")]
    TooLarge,
    #[error("malformed model file: {0}")]
    Malformed(&'static str),
    #[error("unsupported model file: {0}")]
    Unsupported(&'static str),
    #[error("model file refused: {0}")]
    Refused(Bound),
}

/// What the name or MIME said: only whether to try. The content picks the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameHint {
    /// The name or MIME looked like a model: a headerless binary STL is tried too.
    Model,
    /// Nothing said so: only self-identifying content (3MF ZIP, ASCII STL) is accepted.
    None,
}

/// The format the content turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    StlBinary,
    StlAscii,
    ThreeMf,
}

/// A triangle soup in millimetres. Every `Ok` mesh has at least one triangle, finite values,
/// `positions.len() % 9 == 0`, at most `MAX_TRIANGLES` triangles and one colour per triangle.
#[derive(Clone, PartialEq)]
pub struct Mesh {
    /// 9 floats per triangle (x y z of three corners), mm.
    pub positions: Vec<f32>,
    /// RGBA (`0xRRGGBBAA`) per triangle, `0` = no colour of its own; `None` when no triangle has one.
    pub colors: Option<Vec<u32>>,
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub format: Format,
}

/// Never prints a coordinate: a `{:?}` in a log must not carry file content.
impl std::fmt::Debug for Mesh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mesh")
            .field("triangles", &self.triangle_count())
            .field("coloured", &self.colors.is_some())
            .field("min", &self.min)
            .field("max", &self.max)
            .field("format", &self.format)
            .finish()
    }
}

impl Mesh {
    pub fn triangle_count(&self) -> usize {
        self.positions.len() / 9
    }

    pub(crate) fn new(
        positions: Vec<f32>,
        colors: Vec<u32>,
        format: Format,
    ) -> Result<Mesh, MeshError> {
        if positions.is_empty() {
            return Err(MeshError::Malformed("no triangles"));
        }
        let mut min = [f32::MAX; 3];
        let mut max = [f32::MIN; 3];
        for p in positions.as_chunks::<3>().0.iter() {
            for k in 0..3 {
                min[k] = min[k].min(p[k]);
                max[k] = max[k].max(p[k]);
            }
        }
        let colors = if colors.iter().any(|&c| c != 0) {
            Some(colors)
        } else {
            None
        };
        Ok(Mesh {
            positions,
            colors,
            min,
            max,
            format,
        })
    }
}

/// Whether a file name or MIME type says "try the model parser" (never a promise it parses).
pub fn looks_like_model(name: Option<&str>, mime: Option<&str>) -> bool {
    if let Some(name) = name {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".stl") || lower.ends_with(".3mf") {
            return true;
        }
    }
    if let Some(mime) = mime {
        let essence = mime
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        return matches!(
            essence.as_str(),
            "model/stl"
                | "model/3mf"
                | "application/sla"
                | "application/vnd.ms-pki.stl"
                | "application/vnd.ms-package.3dmanufacturing-3dmodel+xml"
        );
    }
    false
}

/// Parses a model; the content picks the format (ZIP with a 3D model relationship: 3MF; `solid` and
/// facets: ASCII STL, a binary STL whose header starts with `solid` wins when the ASCII parse
/// fails and `size == 84 + 50 n` holds; otherwise binary when the size rule holds).
pub fn parse(bytes: &[u8], hint: NameHint) -> Result<Mesh, MeshError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(MeshError::TooLarge);
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return threemf::parse(bytes, &threemf::Caps::default());
    }
    let head = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let head = head.trim_ascii_start();
    if head.len() >= 5 && head[..5].eq_ignore_ascii_case(b"solid") {
        return match stl::parse_ascii(bytes) {
            Ok(m) => Ok(m),
            Err(e) => {
                if stl::size_rule_holds(bytes) {
                    stl::parse_binary(bytes)
                } else {
                    Err(e)
                }
            }
        };
    }
    if hint == NameHint::None {
        return Err(MeshError::Unsupported("no self-identifying content"));
    }
    stl::parse_binary(bytes)
}
