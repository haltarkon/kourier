//! 3MF: ZIP + OPC root relationship + streaming XML. Units to mm, components with transforms,
//! production extension parts (`p:path`), standard colours (`basematerials`, `m:colorgroup`).

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use zip::ZipArchive;
use zip::result::ZipError;

use crate::slicer::{self, Look, ObjCfg, Slicer};
use crate::{
    Bound, Format, MAX_COLOURS, MAX_COMPONENT_DEPTH, MAX_CONFIG_PART_BYTES, MAX_CONFIG_PARTS,
    MAX_GROUPS, MAX_INSTANCES, MAX_MODEL_PARTS, MAX_MODEL_XML_TOTAL, MAX_OBJECTS, MAX_PATH_BYTES,
    MAX_SMALL_PART_BYTES, MAX_TRIANGLES, MAX_VERTICES, MAX_XML_DEPTH, MAX_XML_TOKEN,
    MAX_ZIP_ENTRIES, Mesh, MeshError,
};

/// The read-ahead of the buffered reader in front of quick-xml; it comes on top of `MAX_XML_TOKEN`.
const READ_AHEAD: usize = 64 << 10;

/// The numeric caps, lowered by tests so a hostile fixture stays small.
pub(crate) struct Caps {
    pub total_xml: u64,
    pub vertices: usize,
    pub objects: usize,
    pub instances: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            total_xml: MAX_MODEL_XML_TOTAL,
            vertices: MAX_VERTICES,
            objects: MAX_OBJECTS,
            instances: MAX_INSTANCES,
        }
    }
}

type Mat = [[f64; 3]; 4];
const IDENTITY: Mat = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.0; 3]];
const NO_PID: u32 = u32::MAX;

/// `a` applied first, then `b` (row vectors: p' = p * M).
fn compose(a: &Mat, b: &Mat) -> Mat {
    let mut c = [[0.0; 3]; 4];
    for i in 0..4 {
        for j in 0..3 {
            let mut s = 0.0;
            for k in 0..3 {
                s += a[i][k] * b[k][j];
            }
            if i == 3 {
                s += b[3][j];
            }
            c[i][j] = s;
        }
    }
    c
}

/// A transform's translation is in the unit of the part that declares it; the rotation part has none.
fn with_unit(mut m: Mat, unit: f64) -> Mat {
    for v in &mut m[3] {
        *v *= unit;
    }
    m
}

struct Comp {
    /// The interned part name (`Interner`), `None` = the part the component is in.
    path: Option<u16>,
    id: u32,
    m: Mat,
}

#[derive(Default)]
struct Object {
    verts: Vec<[f32; 3]>,
    tris: Vec<[u32; 3]>,
    /// (pid, p1) per triangle, `pid == NO_PID` when the triangle has none.
    props: Vec<[u32; 2]>,
    default: Option<(u32, u32)>,
    buildable: bool,
    comps: Vec<Comp>,
}

struct Part {
    objects: HashMap<u32, Object>,
    groups: HashMap<u32, Vec<u32>>,
    /// This part's own unit, in mm.
    unit: f64,
}

struct Item {
    path: Option<u16>,
    id: u32,
    m: Mat,
}

#[derive(Default)]
struct Tally {
    colours: usize,
    vertices: usize,
    triangles: usize,
    objects: usize,
    instances: usize,
    groups: usize,
}

/// Part names, each once, at most `MAX_MODEL_PARTS`; a part's name id is its index in the loader.
struct Interner {
    names: Vec<String>,
    map: HashMap<String, u16>,
}

impl Interner {
    fn intern(&mut self, raw: &[u8]) -> Result<u16, MeshError> {
        if raw.len() > MAX_PATH_BYTES {
            return Err(MeshError::Refused(Bound::PathLength));
        }
        let lossy = String::from_utf8_lossy(raw);
        let name = lossy.trim_start_matches('/');
        if let Some(&i) = self.map.get(name) {
            return Ok(i);
        }
        if self.names.len() >= MAX_MODEL_PARTS {
            return Err(MeshError::Refused(Bound::ModelParts));
        }
        let i = self.names.len() as u16;
        self.names.push(name.to_string());
        self.map.insert(name.to_string(), i);
        Ok(i)
    }
}

/// What was read from the ZIP: each local header once, and no more compressed bytes than the file has.
struct Reads {
    seen: HashSet<u64>,
    compressed: u64,
    limit: u64,
}

impl Reads {
    fn admit<R: Read>(&mut self, e: &zip::read::ZipFile<'_, R>) -> Result<(), MeshError> {
        self.compressed = self.compressed.saturating_add(e.compressed_size());
        if !self.seen.insert(e.header_start()) || self.compressed > self.limit {
            return Err(MeshError::Refused(Bound::ZipStructure));
        }
        Ok(())
    }
}

fn zip_err(e: ZipError) -> MeshError {
    match e {
        ZipError::UnsupportedArchive(_) => MeshError::Unsupported("zip feature"),
        ZipError::FileNotFound => MeshError::Malformed("part missing"),
        _ => MeshError::Malformed("zip"),
    }
}

struct Counted<'a, R> {
    inner: R,
    total: &'a Cell<u64>,
    limit: u64,
    /// Bytes read since the last XML event (the caller resets it after every event).
    token: &'a Cell<u64>,
    trip: &'a Cell<Option<Bound>>,
}

impl<R: Read> Read for Counted<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.total.set(self.total.get() + n as u64);
        self.token.set(self.token.get() + n as u64);
        if self.total.get() > self.limit {
            self.trip.set(Some(Bound::XmlTotal));
            return Err(io::Error::other("xml budget"));
        }
        if self.token.get() > MAX_XML_TOKEN + READ_AHEAD as u64 {
            self.trip.set(Some(Bound::XmlToken));
            return Err(io::Error::other("xml token"));
        }
        Ok(n)
    }
}

/// Stored or deflate only.
fn check_method<R: Read>(entry: &zip::read::ZipFile<'_, R>) -> Result<(), MeshError> {
    match entry.compression() {
        zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated => Ok(()),
        _ => Err(MeshError::Unsupported("zip compression method")),
    }
}

type Zip<'a> = ZipArchive<Cursor<&'a [u8]>>;

fn read_small(z: &mut Zip, reads: &mut Reads, name: &str) -> Result<Option<Vec<u8>>, MeshError> {
    let entry = match z.by_name(name) {
        Ok(e) => e,
        Err(ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(zip_err(e)),
    };
    check_method(&entry)?;
    reads.admit(&entry)?;
    let mut buf = Vec::new();
    entry
        .take(MAX_SMALL_PART_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|_| MeshError::Malformed("part unreadable"))?;
    if buf.len() as u64 > MAX_SMALL_PART_BYTES {
        return Err(MeshError::Refused(Bound::SmallPart));
    }
    Ok(Some(buf))
}

/// A slicer config part. `Ok(None)` for every fault of the part itself (missing, a method we do not
/// read, too large, unreadable): a config never fails the mesh. Only the archive guard (`admit`)
/// refuses the whole file. At most `MAX_CONFIG_PARTS` are read in all (`count`).
fn read_config(
    z: &mut Zip,
    reads: &mut Reads,
    count: &mut usize,
    name: &str,
) -> Result<Option<Vec<u8>>, MeshError> {
    if *count >= MAX_CONFIG_PARTS {
        return Ok(None);
    }
    let Ok(entry) = z.by_name(name) else {
        return Ok(None);
    };
    *count += 1;
    if check_method(&entry).is_err() {
        return Ok(None);
    }
    reads.admit(&entry)?;
    let mut buf = Vec::new();
    if entry
        .take(MAX_CONFIG_PART_BYTES + 1)
        .read_to_end(&mut buf)
        .is_err()
        || buf.len() as u64 > MAX_CONFIG_PART_BYTES
    {
        return Ok(None);
    }
    Ok(Some(buf))
}

/// Reads the slicer config of the file: Bambu Studio / OrcaSlicer first, else PrusaSlicer.
fn load_slicer(z: &mut Zip, reads: &mut Reads) -> Result<Option<Slicer>, MeshError> {
    let has = |z: &Zip, n: &str| z.index_for_name(n).is_some();
    let mut count = 0;
    let bambu = has(z, slicer::BAMBU_MODEL) && has(z, slicer::BAMBU_PROJECT);
    let prusa = has(z, slicer::PRUSA_MODEL) && has(z, slicer::PRUSA_PROJECT);
    let (model, project, is_bambu) = if bambu {
        (slicer::BAMBU_MODEL, slicer::BAMBU_PROJECT, true)
    } else if prusa {
        (slicer::PRUSA_MODEL, slicer::PRUSA_PROJECT, false)
    } else {
        return Ok(None);
    };
    let m = read_config(z, reads, &mut count, model)?;
    let p = read_config(z, reads, &mut count, project)?;
    let (Some(m), Some(p)) = (m, p) else {
        return Ok(None);
    };
    Ok(if is_bambu {
        slicer::bambu(&m, &p)
    } else {
        slicer::prusa(&m, &p)
    }
    .ok())
}

fn xml_err(trip: &Cell<Option<Bound>>) -> MeshError {
    match trip.get() {
        Some(b) => MeshError::Refused(b),
        None => MeshError::Malformed("xml"),
    }
}

/// Walks a small XML part: DOCTYPE refused, depth bounded; returns the model relationship target.
fn scan_small(bytes: &[u8]) -> Result<Option<Vec<u8>>, MeshError> {
    let mut x = Reader::from_reader(bytes);
    let mut b = Vec::new();
    let mut depth = 0u32;
    let mut root = None;
    loop {
        let ev = x
            .read_event_into(&mut b)
            .map_err(|_| MeshError::Malformed("xml"))?;
        match &ev {
            Event::DocType(_) => return Err(MeshError::Refused(Bound::DocType)),
            Event::Start(_) => {
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    return Err(MeshError::Refused(Bound::XmlDepth));
                }
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Eof => break,
            _ => {}
        }
        if let Event::Start(el) | Event::Empty(el) = &ev
            && el.local_name().as_ref().as_bytes() == b"Relationship"
        {
            let (mut target, mut is_model) = (None, false);
            for a in el.attributes() {
                let a = a.map_err(|_| MeshError::Malformed("attribute"))?;
                match a.key.as_ref().as_bytes() {
                    b"Target" => target = Some(a.value.as_bytes().to_vec()),
                    b"Type" => is_model = a.value.as_bytes().ends_with(b"/3dmodel"),
                    _ => {}
                }
            }
            if is_model && root.is_none() {
                root = target;
            }
        }
        b.clear();
    }
    Ok(root)
}

fn unit_scale(v: &[u8]) -> Result<f64, MeshError> {
    Ok(match v {
        b"micron" => 0.001,
        b"millimeter" => 1.0,
        b"centimeter" => 10.0,
        b"inch" => 25.4,
        b"foot" => 304.8,
        b"meter" => 1000.0,
        _ => return Err(MeshError::Refused(Bound::Unit)),
    })
}

pub(crate) fn colour_hex(v: &[u8]) -> u32 {
    let Some(s) = v.strip_prefix(b"#") else {
        return 0;
    };
    if !(s.len() == 6 || s.len() == 8) || !s.iter().all(u8::is_ascii_hexdigit) {
        return 0;
    }
    let Ok(t) = std::str::from_utf8(s) else {
        return 0;
    };
    let Ok(n) = u32::from_str_radix(t, 16) else {
        return 0;
    };
    if s.len() == 6 { (n << 8) | 0xFF } else { n }
}

fn parse_transform(v: &[u8]) -> Result<Mat, MeshError> {
    let bad = MeshError::Refused(Bound::Transform);
    let s = std::str::from_utf8(v).map_err(|_| bad)?;
    let mut n = [0f64; 12];
    let mut it = s.split_ascii_whitespace();
    for slot in &mut n {
        let f: f64 = it.next().ok_or(bad)?.parse().map_err(|_| bad)?;
        if !f.is_finite() {
            return Err(bad);
        }
        *slot = f;
    }
    if it.next().is_some() {
        return Err(bad);
    }
    Ok([
        [n[0], n[1], n[2]],
        [n[3], n[4], n[5]],
        [n[6], n[7], n[8]],
        [n[9], n[10], n[11]],
    ])
}

fn num(v: &[u8]) -> Result<f32, MeshError> {
    std::str::from_utf8(v)
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .ok_or(MeshError::Malformed("bad number"))
}

fn int(v: &[u8]) -> Result<u32, MeshError> {
    std::str::from_utf8(v)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or(MeshError::Malformed("bad integer"))
}

/// quick-xml 0.42 hands names and raw attribute values as `str`; the rest of the parser works on bytes.
fn bytes(v: Cow<'_, str>) -> Cow<'_, [u8]> {
    match v {
        Cow::Borrowed(s) => Cow::Borrowed(s.as_bytes()),
        Cow::Owned(s) => Cow::Owned(s.into_bytes()),
    }
}

/// The unprefixed attribute `name` (a core 3MF attribute); a duplicate attribute is malformed.
fn attr<'a>(el: &'a BytesStart, name: &[u8]) -> Result<Option<Cow<'a, [u8]>>, MeshError> {
    let mut found = None;
    for a in el.attributes() {
        let a = a.map_err(|_| MeshError::Malformed("attribute"))?;
        if found.is_none() && a.key.as_ref().as_bytes() == name {
            found = Some(bytes(a.value));
        }
    }
    Ok(found)
}

/// The production extension's `path` attribute: prefixed (`p:path`), never an `xmlns:` declaration.
fn path_attr<'a>(el: &'a BytesStart) -> Result<Option<Cow<'a, [u8]>>, MeshError> {
    let mut found = None;
    for a in el.attributes() {
        let a = a.map_err(|_| MeshError::Malformed("attribute"))?;
        let prefixed = a
            .key
            .prefix()
            .is_some_and(|p| p.as_ref().as_bytes() != b"xmlns");
        if found.is_none() && prefixed && a.key.local_name().as_ref().as_bytes() == b"path" {
            found = Some(bytes(a.value));
        }
    }
    Ok(found)
}

/// Parses one model part. Returns the part and its build items.
fn parse_part<R: io::BufRead>(
    rd: R,
    trip: &Cell<Option<Bound>>,
    token: &Cell<u64>,
    tally: &mut Tally,
    names: &mut Interner,
    caps: &Caps,
) -> Result<(Part, Vec<Item>), MeshError> {
    let mut x = Reader::from_reader(rd);
    let mut b = Vec::with_capacity(4096);
    let mut part = Part {
        objects: HashMap::new(),
        groups: HashMap::new(),
        unit: 1.0,
    };
    let mut items = Vec::new();
    let mut cur: Option<(u32, Object)> = None;
    let mut group: Option<u32> = None;
    let mut depth = 0u32;
    loop {
        let ev = x.read_event_into(&mut b).map_err(|_| xml_err(trip))?;
        let (el, empty) = match &ev {
            Event::DocType(_) => return Err(MeshError::Refused(Bound::DocType)),
            Event::Eof => break,
            Event::Start(e) => (Some(e), false),
            Event::Empty(e) => (Some(e), true),
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                match e.local_name().as_ref().as_bytes() {
                    b"object" => {
                        if let Some((id, o)) = cur.take()
                            && part.objects.insert(id, o).is_some()
                        {
                            return Err(MeshError::Malformed("duplicate object id"));
                        }
                    }
                    b"basematerials" | b"colorgroup" => group = None,
                    _ => {}
                }
                (None, false)
            }
            _ => (None, false),
        };
        if let Some(el) = el {
            if !empty {
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    return Err(MeshError::Refused(Bound::XmlDepth));
                }
            }
            match el.local_name().as_ref().as_bytes() {
                b"model" => {
                    if let Some(u) = attr(el, b"unit")? {
                        part.unit = unit_scale(&u)?;
                    }
                }
                b"object" => {
                    if cur.is_some() {
                        return Err(MeshError::Malformed("nested object"));
                    }
                    tally.objects += 1;
                    if tally.objects > caps.objects {
                        return Err(MeshError::Refused(Bound::Objects));
                    }
                    let id = int(&attr(el, b"id")?.ok_or(MeshError::Malformed("object id"))?)?;
                    let mut o = Object {
                        buildable: !matches!(
                            attr(el, b"type")?.as_deref(),
                            Some(b"support" | b"solidsupport" | b"surface" | b"other")
                        ),
                        ..Object::default()
                    };
                    if let Some(pid) = attr(el, b"pid")? {
                        let pindex = attr(el, b"pindex")?.map(|v| int(&v)).transpose()?;
                        o.default = Some((int(&pid)?, pindex.unwrap_or(0)));
                    }
                    if empty {
                        if part.objects.insert(id, o).is_some() {
                            return Err(MeshError::Malformed("duplicate object id"));
                        }
                    } else {
                        cur = Some((id, o));
                    }
                }
                b"vertex" => {
                    if let Some((_, o)) = cur.as_mut() {
                        tally.vertices += 1;
                        if tally.vertices > caps.vertices {
                            return Err(MeshError::Refused(Bound::Vertices));
                        }
                        let mut p = [0f32; 3];
                        for a in el.attributes() {
                            let a = a.map_err(|_| MeshError::Malformed("attribute"))?;
                            match a.key.as_ref().as_bytes() {
                                b"x" => p[0] = num(a.value.as_bytes())?,
                                b"y" => p[1] = num(a.value.as_bytes())?,
                                b"z" => p[2] = num(a.value.as_bytes())?,
                                _ => {}
                            }
                        }
                        o.verts.push(p);
                    }
                }
                b"triangle" => {
                    if let Some((_, o)) = cur.as_mut() {
                        tally.triangles += 1;
                        if tally.triangles > MAX_TRIANGLES {
                            return Err(MeshError::Refused(Bound::Triangles));
                        }
                        let mut t = [0u32; 3];
                        let (mut pid, mut p1) = (NO_PID, 0);
                        for a in el.attributes() {
                            let a = a.map_err(|_| MeshError::Malformed("attribute"))?;
                            match a.key.as_ref().as_bytes() {
                                b"v1" => t[0] = int(a.value.as_bytes())?,
                                b"v2" => t[1] = int(a.value.as_bytes())?,
                                b"v3" => t[2] = int(a.value.as_bytes())?,
                                b"pid" => pid = int(a.value.as_bytes())?,
                                b"p1" => p1 = int(a.value.as_bytes())?,
                                _ => {}
                            }
                        }
                        o.tris.push(t);
                        o.props.push([pid, p1]);
                    }
                }
                b"component" => {
                    if let Some((_, o)) = cur.as_mut() {
                        tally.instances += 1;
                        if tally.instances > caps.instances {
                            return Err(MeshError::Refused(Bound::Instances));
                        }
                        let path = match path_attr(el)? {
                            Some(p) => Some(names.intern(&p)?),
                            None => None,
                        };
                        o.comps.push(Comp {
                            path,
                            id: int(&attr(el, b"objectid")?
                                .ok_or(MeshError::Malformed("component id"))?)?,
                            m: match attr(el, b"transform")? {
                                Some(t) => with_unit(parse_transform(&t)?, part.unit),
                                None => IDENTITY,
                            },
                        });
                    }
                }
                b"item" => {
                    tally.instances += 1;
                    if tally.instances > caps.instances {
                        return Err(MeshError::Refused(Bound::Instances));
                    }
                    let path = match path_attr(el)? {
                        Some(p) => Some(names.intern(&p)?),
                        None => None,
                    };
                    items.push(Item {
                        path,
                        id: int(&attr(el, b"objectid")?.ok_or(MeshError::Malformed("item id"))?)?,
                        m: match attr(el, b"transform")? {
                            Some(t) => with_unit(parse_transform(&t)?, part.unit),
                            None => IDENTITY,
                        },
                    });
                }
                b"basematerials" | b"colorgroup" => {
                    tally.groups += 1;
                    if tally.groups > MAX_GROUPS {
                        return Err(MeshError::Refused(Bound::Groups));
                    }
                    let id = int(&attr(el, b"id")?.ok_or(MeshError::Malformed("group id"))?)?;
                    if part.groups.insert(id, Vec::new()).is_some() {
                        return Err(MeshError::Malformed("duplicate group id"));
                    }
                    if !empty {
                        group = Some(id);
                    }
                }
                name @ (b"base" | b"color") if group.is_some() => {
                    let key: &[u8] = if name == b"base" {
                        b"displaycolor"
                    } else {
                        b"color"
                    };
                    tally.colours += 1;
                    if tally.colours > MAX_COLOURS {
                        return Err(MeshError::Refused(Bound::Colours));
                    }
                    let c = attr(el, key)?.map(|v| colour_hex(&v)).unwrap_or(0);
                    if let Some(g) = group.and_then(|id| part.groups.get_mut(&id)) {
                        g.push(c);
                    }
                }
                _ => {}
            }
        }
        b.clear();
        token.set(0);
    }
    Ok((part, items))
}

struct Loader<'a> {
    zip: Zip<'a>,
    /// Indexed by the part's name id; `None` until loaded.
    parts: Vec<Option<Part>>,
    names: Interner,
    reads: Reads,
    total: Cell<u64>,
    token: Cell<u64>,
    trip: Cell<Option<Bound>>,
    tally: Tally,
    caps: &'a Caps,
    /// The slicer's config, when the file has one that reads.
    slicer: Option<Slicer>,
}

impl Loader<'_> {
    fn load(&mut self, ix: usize) -> Result<Vec<Item>, MeshError> {
        let name = self.names.names[ix].clone();
        let entry = self.zip.by_name(&name).map_err(zip_err)?;
        check_method(&entry)?;
        self.reads.admit(&entry)?;
        let counted = Counted {
            inner: entry,
            total: &self.total,
            limit: self.caps.total_xml,
            token: &self.token,
            trip: &self.trip,
        };
        let rd = BufReader::with_capacity(READ_AHEAD, counted);
        self.token.set(0);
        let (part, items) = parse_part(
            rd,
            &self.trip,
            &self.token,
            &mut self.tally,
            &mut self.names,
            self.caps,
        )?;
        self.parts
            .resize_with(self.names.names.len().max(ix + 1), || None);
        self.parts[ix] = Some(part);
        Ok(items)
    }
}

struct Out {
    pos: Vec<f32>,
    col: Vec<u32>,
    considered: usize,
    visits: usize,
}

/// A linear look at the ZIP's signatures before `ZipArchive::new` walks it: the zip crate retries
/// every end-of-central-directory record it finds and re-parses the central directory each time,
/// and reads every central header before any cap of ours. More than 4 end records (or zip64 end
/// records), or more central headers than `MAX_ZIP_ENTRIES` plus a little slack, is refused.
fn prescan(bytes: &[u8]) -> Result<(), MeshError> {
    let (mut eocd, mut eocd64, mut central) = (0usize, 0usize, 0usize);
    for w in bytes.windows(4) {
        if w[0] == b'P' && w[1] == b'K' {
            match (w[2], w[3]) {
                (5, 6) => eocd += 1,
                (6, 6) => eocd64 += 1,
                (1, 2) => central += 1,
                _ => {}
            }
            if eocd > 4 || eocd64 > 4 {
                return Err(MeshError::Refused(Bound::ZipStructure));
            }
            if central > MAX_ZIP_ENTRIES + 16 {
                return Err(MeshError::Refused(Bound::ZipEntries));
            }
        }
    }
    Ok(())
}

/// The zip crate reports a method it was built without as a plain archive error: on that error
/// path, look at the central directory's methods ourselves (stored 0 and deflate 8 only).
fn has_unsupported_method(bytes: &[u8]) -> bool {
    let mut i = 0;
    while let Some(at) = bytes[i..].windows(4).position(|w| w == b"PK\x01\x02") {
        let at = i + at;
        if at + 12 <= bytes.len() {
            let method = u16::from_le_bytes([bytes[at + 10], bytes[at + 11]]);
            if method != 0 && method != 8 {
                return true;
            }
        }
        i = at + 4;
    }
    false
}

pub(crate) fn parse(bytes: &[u8], caps: &Caps) -> Result<Mesh, MeshError> {
    match parse_inner(bytes, caps) {
        Err(MeshError::Malformed("zip")) if has_unsupported_method(bytes) => {
            Err(MeshError::Unsupported("zip compression method"))
        }
        r => r,
    }
}

fn parse_inner(bytes: &[u8], caps: &Caps) -> Result<Mesh, MeshError> {
    prescan(bytes)?;
    let mut zip = ZipArchive::new(Cursor::new(bytes)).map_err(zip_err)?;
    if zip.len() > MAX_ZIP_ENTRIES {
        return Err(MeshError::Refused(Bound::ZipEntries));
    }
    let mut reads = Reads {
        seen: HashSet::new(),
        compressed: 0,
        limit: bytes.len() as u64,
    };
    let rels = read_small(&mut zip, &mut reads, "_rels/.rels")?
        .ok_or(MeshError::Unsupported("no package relationships"))?;
    let root = scan_small(&rels)?.ok_or(MeshError::Unsupported("no 3D model relationship"))?;
    if let Some(ct) = read_small(&mut zip, &mut reads, "[Content_Types].xml")? {
        scan_small(&ct)?;
    }
    let mut ld = Loader {
        zip,
        parts: Vec::new(),
        names: Interner {
            names: Vec::new(),
            map: HashMap::new(),
        },
        reads,
        total: Cell::new(0),
        token: Cell::new(0),
        trip: Cell::new(None),
        tally: Tally::default(),
        caps,
        slicer: None,
    };
    let root_ix = ld.names.intern(&root)? as usize;
    let items = ld.load(root_ix)?;
    // Load every part the items and components named, each once (loading may name more).
    let mut k = 0;
    while k < ld.names.names.len() {
        if ld.parts.get(k).is_none_or(|p| p.is_none()) {
            ld.load(k)?;
        }
        k += 1;
    }
    if items.is_empty() {
        return Err(MeshError::Malformed("no build items"));
    }
    ld.slicer = load_slicer(&mut ld.zip, &mut ld.reads)?;
    if let (Some(s), Some(root)) = (ld.slicer.as_mut(), ld.parts[root_ix].as_ref()) {
        s.finish(|id| root.objects.get(&id).map(|o| o.tris.len()));
    }
    let mut out = run(&ld, root_ix, &items, true)?;
    if out.pos.is_empty() && ld.slicer.as_ref().is_some_and(|s| s.any_drop) {
        // Dropping the parts the slicer does not print would leave nothing: keep the mesh as without.
        out = run(&ld, root_ix, &items, false)?;
    }
    Mesh::new(out.pos, out.col, Format::ThreeMf)
}

fn run(ld: &Loader, root_ix: usize, items: &[Item], slicer: bool) -> Result<Out, MeshError> {
    let mut out = Out {
        pos: Vec::new(),
        col: Vec::new(),
        considered: 0,
        visits: 0,
    };
    let mut stack = Vec::new();
    for it in items {
        let part = it.path.map_or(root_ix, usize::from);
        let cfg = ld
            .slicer
            .as_ref()
            .filter(|_| slicer && part == root_ix)
            .and_then(|s| s.objects.get(&it.id));
        let fil = Fil {
            cfg,
            base: cfg.map_or(Look::default(), |c| c.base),
        };
        expand(ld, part, it.id, &it.m, &mut stack, &mut out, true, fil)?;
    }
    Ok(out)
}

/// The slicer's say over one object: its config (only the build item's object has one) and the look
/// every triangle without a config of its own takes.
#[derive(Clone, Copy)]
struct Fil<'a> {
    cfg: Option<&'a ObjCfg>,
    base: Look,
}

fn group_colour(part: &Part, pid: u32, idx: u32) -> u32 {
    part.groups
        .get(&pid)
        .and_then(|g| g.get(idx as usize))
        .copied()
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn expand(
    ld: &Loader,
    part: usize,
    id: u32,
    m: &Mat,
    stack: &mut Vec<(usize, u32)>,
    out: &mut Out,
    top: bool,
    fil: Fil,
) -> Result<(), MeshError> {
    out.visits += 1;
    if out.visits > ld.caps.instances {
        return Err(MeshError::Refused(Bound::Instances));
    }
    if stack.contains(&(part, id)) {
        return Err(MeshError::Refused(Bound::ComponentCycle));
    }
    if stack.len() > MAX_COMPONENT_DEPTH {
        return Err(MeshError::Refused(Bound::ComponentDepth));
    }
    let p = ld.parts[part]
        .as_ref()
        .ok_or(MeshError::Malformed("model part missing"))?;
    let obj = p
        .objects
        .get(&id)
        .ok_or(MeshError::Malformed("object missing"))?;
    if top && !obj.buildable {
        return Ok(());
    }
    let default = obj
        .default
        .map(|(pid, i)| group_colour(p, pid, i))
        .unwrap_or(0);
    out.considered += obj.tris.len();
    if out.considered > MAX_TRIANGLES {
        return Err(MeshError::Refused(Bound::Triangles));
    }
    for (k, t) in obj.tris.iter().enumerate() {
        let mut tri = [0f32; 9];
        let mut ok = true;
        for (c, &vi) in t.iter().enumerate() {
            let v = obj
                .verts
                .get(vi as usize)
                .ok_or(MeshError::Refused(Bound::VertexIndex))?;
            // The vertex is in this part's own unit; transforms already carry mm translations.
            let v = [
                v[0] as f64 * p.unit,
                v[1] as f64 * p.unit,
                v[2] as f64 * p.unit,
            ];
            for j in 0..3 {
                let r = v[0] * m[0][j] + v[1] * m[1][j] + v[2] * m[2][j] + m[3][j];
                let f = r as f32;
                ok &= f.is_finite();
                tri[3 * c + j] = f;
            }
        }
        let look = fil.cfg.map_or(fil.base, |c| c.own(k));
        if !ok || look.drop {
            continue;
        }
        let [pid, p1] = obj.props[k];
        let colour = if pid != NO_PID {
            match group_colour(p, pid, p1) {
                0 => default,
                c => c,
            }
        } else {
            default
        };
        out.pos.extend_from_slice(&tri);
        out.col.push(if colour == 0 { look.colour } else { colour });
    }
    if !obj.comps.is_empty() {
        stack.push((part, id));
        for c in &obj.comps {
            let child = c.path.map_or(part, usize::from);
            let sub = Fil {
                cfg: None,
                base: fil
                    .cfg
                    .and_then(|cfg| cfg.parts.get(&c.id).copied())
                    .unwrap_or(fil.base),
            };
            expand(ld, child, c.id, &compose(&c.m, m), stack, out, false, sub)?;
        }
        stack.pop();
    }
    Ok(())
}
