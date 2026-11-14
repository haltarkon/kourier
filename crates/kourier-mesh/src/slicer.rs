//! Slicer configs of a 3MF: the filament colour of every triangle (Bambu Studio, OrcaSlicer,
//! PrusaSlicer) and the parts a slicer does not print. The keys are in
//! `notes/kourier/viewer-3d-slicer-keys.md`. A fault in a config (missing, too large, not UTF-8,
//! malformed, a bound broken) is never a fault of the mesh: the config is ignored as a whole.

use std::collections::HashMap;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use serde::Deserialize;

use crate::threemf::colour_hex;
use crate::{MAX_COLOURS, MAX_XML_DEPTH};

pub(crate) const BAMBU_MODEL: &str = "Metadata/model_settings.config";
pub(crate) const BAMBU_PROJECT: &str = "Metadata/project_settings.config";
pub(crate) const PRUSA_MODEL: &str = "Metadata/Slic3r_PE_model.config";
pub(crate) const PRUSA_PROJECT: &str = "Metadata/Slic3r_PE.config";

/// What a triangle takes from the config: a filament colour (`0` = none) and whether it is a
/// part the slicer does not print.
#[derive(Clone, Copy, Default)]
pub(crate) struct Look {
    pub colour: u32,
    pub drop: bool,
}

struct Range {
    first: u32,
    last: u32,
    look: Look,
}

pub(crate) struct ObjCfg {
    pub base: Look,
    /// Sorted by `first`, inclusive, never overlapping, within the object's own triangles.
    ranges: Vec<Range>,
    /// Bambu: by the `objectid` of the object's direct component.
    pub parts: HashMap<u32, Look>,
}

impl ObjCfg {
    /// The look of the object's own triangle `k`: O(log ranges).
    pub fn own(&self, k: usize) -> Look {
        if self.ranges.is_empty() {
            return self.base;
        }
        let k = k as u64;
        let i = self.ranges.partition_point(|r| u64::from(r.first) <= k);
        match i.checked_sub(1).map(|i| &self.ranges[i]) {
            Some(r) if u64::from(r.last) >= k => r.look,
            _ => self.base,
        }
    }
}

/// The config of a file, by the build item's object id in the root model part.
pub(crate) struct Slicer {
    pub objects: HashMap<u32, ObjCfg>,
    pub any_drop: bool,
}

impl Slicer {
    /// Drops the ranges that are reversed, past the object's triangles or overlapping the one
    /// before; `tris` is the own triangle count of the root part's object.
    pub fn finish(&mut self, tris: impl Fn(u32) -> Option<usize>) {
        for (id, o) in &mut self.objects {
            let n = tris(*id).unwrap_or(0) as u64;
            o.ranges
                .retain(|r| r.first <= r.last && u64::from(r.last) < n);
            o.ranges.sort_by_key(|r| r.first);
            let mut end: Option<u32> = None;
            o.ranges.retain(|r| {
                let ok = end.is_none_or(|e| r.first > e);
                if ok {
                    end = Some(r.last);
                }
                ok
            });
        }
        self.any_drop = self.objects.values().any(|o| {
            o.base.drop || o.ranges.iter().any(|r| r.look.drop) || o.parts.values().any(|l| l.drop)
        });
    }
}

// ---------- XML walker ----------

enum Ev<'a> {
    Open {
        parent: Option<&'a [u8]>,
        name: &'a [u8],
        el: &'a BytesStart<'a>,
    },
    Close {
        name: &'a [u8],
    },
}

/// Walks a config XML: DOCTYPE refused, depth bounded, no entity is expanded. `Err` = ignore it.
fn walk(bytes: &[u8], mut on: impl FnMut(Ev) -> Result<(), ()>) -> Result<(), ()> {
    let mut x = Reader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut stack: Vec<Vec<u8>> = Vec::new();
    loop {
        let ev = x.read_event_into(&mut buf).map_err(|_| ())?;
        match &ev {
            Event::DocType(_) => return Err(()),
            Event::Eof => return if stack.is_empty() { Ok(()) } else { Err(()) },
            Event::Start(el) | Event::Empty(el) => {
                let empty = matches!(ev, Event::Empty(_));
                if !empty && stack.len() as u32 + 1 > MAX_XML_DEPTH {
                    return Err(());
                }
                let local = el.local_name();
                let parent = stack.last().map(Vec::as_slice);
                on(Ev::Open {
                    parent,
                    name: local.as_ref().as_bytes(),
                    el,
                })?;
                if empty {
                    on(Ev::Close {
                        name: local.as_ref().as_bytes(),
                    })?;
                } else {
                    stack.push(local.as_ref().as_bytes().to_vec());
                }
            }
            Event::End(_) => {
                let name = stack.pop().ok_or(())?;
                on(Ev::Close { name: &name })?;
            }
            _ => {}
        }
        buf.clear();
    }
}

fn attr(el: &BytesStart, name: &[u8]) -> Result<Option<Vec<u8>>, ()> {
    for a in el.attributes() {
        let a = a.map_err(|_| ())?;
        if a.key.as_ref().as_bytes() == name {
            return Ok(Some(a.value.as_bytes().to_vec()));
        }
    }
    Ok(None)
}

fn uint(v: &[u8]) -> Result<u32, ()> {
    std::str::from_utf8(v)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or(())
}

fn opt_uint(el: &BytesStart, name: &[u8]) -> Result<Option<u32>, ()> {
    attr(el, name)?.map(|v| uint(&v)).transpose()
}

/// `<metadata key=".." value=".."/>`: the value of `key` when the element is that key.
fn meta_value(el: &BytesStart, key: &[u8]) -> Result<Option<Vec<u8>>, ()> {
    if attr(el, b"key")?.is_some_and(|k| k == key) {
        Ok(Some(attr(el, b"value")?.ok_or(())?))
    } else {
        Ok(None)
    }
}

// ---------- the colour lists ----------

fn palette(list: &[String]) -> Vec<u32> {
    list.iter().map(|s| colour_hex(s.as_bytes())).collect()
}

#[derive(Deserialize)]
struct ProjectJson {
    #[serde(default)]
    filament_colour: Vec<String>,
}

/// `key = value` lines (a `;` before the key is the Prusa comment form); the last of a key wins.
fn text_lists(text: &str, keys: &[&str]) -> Result<HashMap<String, Vec<String>>, ()> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim_start();
        let line = line.strip_prefix(';').unwrap_or(line).trim_start();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if keys.contains(&k) {
            let list = split_list(v.trim())?;
            out.insert(k.to_string(), list);
        }
    }
    Ok(out)
}

/// A `;`-separated list; an element in double quotes may hold `;` (C-style `\"` and `\\`). Longer
/// than `MAX_COLOURS` is a fault.
fn split_list(v: &str) -> Result<Vec<String>, ()> {
    let mut out = Vec::new();
    if v.is_empty() {
        return Ok(out);
    }
    let (mut cur, mut quoted, mut esc) = (String::new(), false, false);
    for ch in v.chars() {
        if esc {
            cur.push(match ch {
                'n' => '\n',
                't' => '\t',
                c => c,
            });
            esc = false;
        } else if quoted && ch == '\\' {
            esc = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if ch == ';' && !quoted {
            out.push(std::mem::take(&mut cur).trim().to_string());
            if out.len() > MAX_COLOURS {
                return Err(());
            }
        } else {
            cur.push(ch);
        }
    }
    out.push(cur.trim().to_string());
    if out.len() > MAX_COLOURS {
        return Err(());
    }
    Ok(out)
}

fn text_of(b: &[u8]) -> Result<&str, ()> {
    std::str::from_utf8(b).map_err(|_| ())
}

// ---------- Bambu Studio / OrcaSlicer ----------

/// The filament colours of `project_settings.config`: JSON, or the old INI form.
fn bambu_palette(project: &[u8]) -> Result<Vec<u32>, ()> {
    let text = text_of(project)?;
    let t = text.trim_start_matches('\u{feff}').trim_start();
    let list = if t.starts_with('{') {
        let p: ProjectJson = serde_json::from_str(t).map_err(|_| ())?;
        p.filament_colour
    } else {
        text_lists(t, &["filament_colour"])?
            .remove("filament_colour")
            .unwrap_or_default()
    };
    if list.len() > MAX_COLOURS {
        return Err(());
    }
    Ok(palette(&list))
}

fn bambu_drop(subtype: &[u8]) -> bool {
    matches!(
        subtype,
        b"negative_part" | b"modifier_part" | b"support_blocker" | b"support_enforcer"
    )
}

/// The extruder as Bambu's own load fix-up reads it: `0` or past the filament count is 1.
fn bambu_index(e: u32, n: usize) -> usize {
    if e == 0 || e as usize > n {
        1
    } else {
        e as usize
    }
}

/// A `<part>` being read: id, raw extruder, whether it is not printed, a triangle range of an old file.
type BambuPart = (u32, u32, bool, Option<(u32, u32)>);

pub(crate) fn bambu(model: &[u8], project: &[u8]) -> Result<Slicer, ()> {
    let pal = bambu_palette(project)?;
    let n = pal.len();
    let colour = |idx: usize| pal.get(idx - 1).copied().unwrap_or(0);
    let mut objects: HashMap<u32, ObjCfg> = HashMap::new();
    // The object being read, its raw extruder, and the part being read: (id, extruder, drop, range).
    let mut cur: Option<(u32, u32)> = None;
    let mut part: Option<BambuPart> = None;
    let mut parts: Vec<(u32, u32, bool)> = Vec::new();
    let mut ranges: Vec<(u32, u32, u32, bool)> = Vec::new();
    walk(model, |ev| {
        match ev {
            Ev::Open { parent, name, el } => match (parent, name) {
                (Some(b"config"), b"object") if cur.is_none() => {
                    cur = Some((uint(&attr(el, b"id")?.ok_or(())?)?, 0));
                    parts.clear();
                    ranges.clear();
                }
                (Some(b"object"), b"part") if cur.is_some() && part.is_none() => {
                    let id = opt_uint(el, b"id")?.unwrap_or(0);
                    let drop = attr(el, b"subtype")?.is_some_and(|s| bambu_drop(&s));
                    let last = opt_uint(el, b"lastid")?.unwrap_or(0);
                    let range = if last > 0 {
                        Some((opt_uint(el, b"firstid")?.unwrap_or(0), last))
                    } else {
                        None
                    };
                    part = Some((id, 0, drop, range));
                }
                (Some(b"object"), b"metadata") => {
                    if let (Some(v), Some(c)) = (meta_value(el, b"extruder")?, cur.as_mut()) {
                        c.1 = uint(&v)?;
                    }
                }
                (Some(b"part"), b"metadata") => {
                    if let (Some(v), Some(p)) = (meta_value(el, b"extruder")?, part.as_mut()) {
                        p.1 = uint(&v)?;
                    }
                }
                _ => {}
            },
            Ev::Close { name } => match name {
                b"part" => {
                    if let Some((id, e, drop, range)) = part.take() {
                        match range {
                            Some((a, b)) => ranges.push((a, b, e, drop)),
                            None => parts.push((id, e, drop)),
                        }
                    }
                }
                b"object" => {
                    if let Some((id, raw)) = cur.take() {
                        let oi = bambu_index(raw, n);
                        let look = |e: u32, drop: bool| Look {
                            colour: colour(if e == 0 { oi } else { bambu_index(e, n) }),
                            drop,
                        };
                        let cfg = ObjCfg {
                            base: Look {
                                colour: colour(oi),
                                drop: false,
                            },
                            ranges: ranges
                                .iter()
                                .map(|&(first, last, e, d)| Range {
                                    first,
                                    last,
                                    look: look(e, d),
                                })
                                .collect(),
                            parts: parts.iter().map(|&(id, e, d)| (id, look(e, d))).collect(),
                        };
                        objects.insert(id, cfg);
                    }
                }
                _ => {}
            },
        }
        Ok(())
    })?;
    Ok(Slicer {
        objects,
        any_drop: false,
    })
}

// ---------- PrusaSlicer ----------

fn prusa_drop(volume_type: &[u8]) -> bool {
    matches!(
        volume_type,
        b"NegativeVolume" | b"ParameterModifier" | b"SupportBlocker" | b"SupportEnforcer"
    )
}

pub(crate) fn prusa(model: &[u8], project: &[u8]) -> Result<Slicer, ()> {
    let mut lists = text_lists(text_of(project)?, &["extruder_colour", "filament_colour"])?;
    let extruder = palette(&lists.remove("extruder_colour").unwrap_or_default());
    let filament = palette(&lists.remove("filament_colour").unwrap_or_default());
    // PrusaSlicer's own display rule: the extruder's colour, else the filament's; past both: none.
    let colour = |e: u32| -> u32 {
        let i = e.max(1) as usize - 1;
        match extruder.get(i).copied().unwrap_or(0) {
            0 => filament.get(i).copied().unwrap_or(0),
            c => c,
        }
    };
    let mut objects: HashMap<u32, ObjCfg> = HashMap::new();
    let mut cur: Option<(u32, u32)> = None;
    // (first, last, extruder, volume_type drop, modifier drop)
    let mut vol: Option<(u32, u32, u32, Option<bool>, bool)> = None;
    let mut vols: Vec<(u32, u32, u32, bool)> = Vec::new();
    walk(model, |ev| {
        match ev {
            Ev::Open { parent, name, el } => match (parent, name) {
                (Some(b"config"), b"object") if cur.is_none() => {
                    cur = Some((uint(&attr(el, b"id")?.ok_or(())?)?, 0));
                    vols.clear();
                }
                (Some(b"object"), b"volume") if cur.is_some() && vol.is_none() => {
                    let first = uint(&attr(el, b"firstid")?.ok_or(())?)?;
                    let last = uint(&attr(el, b"lastid")?.ok_or(())?)?;
                    vol = Some((first, last, 0, None, false));
                }
                (Some(b"object"), b"metadata") => {
                    if let (Some(v), Some(c)) = (meta_value(el, b"extruder")?, cur.as_mut()) {
                        c.1 = uint(&v)?;
                    }
                }
                (Some(b"volume"), b"metadata") => {
                    if let Some(v) = vol.as_mut() {
                        if let Some(x) = meta_value(el, b"extruder")? {
                            v.2 = uint(&x)?;
                        } else if let Some(x) = meta_value(el, b"volume_type")? {
                            v.3 = Some(prusa_drop(&x));
                        } else if let Some(x) = meta_value(el, b"modifier")? {
                            v.4 = x == b"1";
                        }
                    }
                }
                _ => {}
            },
            Ev::Close { name } => match name {
                b"volume" => {
                    if let Some((a, b, e, vt, m)) = vol.take() {
                        vols.push((a, b, e, vt.unwrap_or(m)));
                    }
                }
                b"object" => {
                    if let Some((id, raw)) = cur.take() {
                        let oe = raw.max(1);
                        let cfg = ObjCfg {
                            base: Look {
                                colour: colour(oe),
                                drop: false,
                            },
                            ranges: vols
                                .iter()
                                .map(|&(first, last, e, drop)| Range {
                                    first,
                                    last,
                                    look: Look {
                                        colour: colour(if e == 0 { oe } else { e }),
                                        drop,
                                    },
                                })
                                .collect(),
                            parts: HashMap::new(),
                        };
                        objects.insert(id, cfg);
                    }
                }
                _ => {}
            },
        }
        Ok(())
    })?;
    Ok(Slicer {
        objects,
        any_drop: false,
    })
}
