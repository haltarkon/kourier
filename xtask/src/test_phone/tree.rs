//! The UI tree of one uiautomator dump, and its small hand-written parser.
//!
//! The dump is machine-made XML (`<hierarchy><node .../></hierarchy>`), so a strict, small parser is
//! enough and no crate is added. The parser keeps going only while the input is well formed: anything
//! else is a [`ParseError`] (never a panic, never a partial tree), and an error carries a byte offset
//! and a fixed reason, never a piece of the input.
//!
//! Texts and content descriptions are the phone's data (names, IDs, codes). The tree therefore offers
//! no getter for them: queries compare against a needle you pass, and [`Node::read_secret`] hands the
//! text out in a [`Secret`]. `Debug` prints counts and resource ids only.

use super::secret::Secret;
use std::fmt;

/// The pixel rectangle of a node: `[left,top][right,bottom]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Bounds {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Bounds {
    pub fn center(&self) -> (i32, i32) {
        (
            self.left + (self.right - self.left) / 2,
            self.top + (self.bottom - self.top) / 2,
        )
    }
    pub fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }
}

/// Why a dump did not parse. Offset and a fixed reason only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub what: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "malformed UI dump at byte {}: {}",
            self.offset, self.what
        )
    }
}

impl std::error::Error for ParseError {}

#[derive(Clone)]
pub struct Node {
    index: usize,
    parent: Option<usize>,
    children: Vec<usize>,
    text: String,
    resource_id: String,
    class: String,
    package: String,
    content_desc: String,
    bounds: Bounds,
    clickable: bool,
    checked: bool,
    selected: bool,
    scrollable: bool,
    enabled: bool,
    focused: bool,
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("index", &self.index)
            .field("resource_id", &self.resource_id)
            .field("class", &self.class)
            .field("has_text", &!self.text.is_empty())
            .field("bounds", &self.bounds)
            .finish()
    }
}

impl Node {
    pub fn index(&self) -> usize {
        self.index
    }
    pub fn resource_id(&self) -> &str {
        &self.resource_id
    }
    pub fn class(&self) -> &str {
        &self.class
    }
    pub fn package(&self) -> &str {
        &self.package
    }
    pub fn bounds(&self) -> Bounds {
        self.bounds
    }
    pub fn clickable(&self) -> bool {
        self.clickable
    }
    pub fn checked(&self) -> bool {
        self.checked
    }
    pub fn selected(&self) -> bool {
        self.selected
    }
    pub fn scrollable(&self) -> bool {
        self.scrollable
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn focused(&self) -> bool {
        self.focused
    }
    pub fn has_text(&self) -> bool {
        !self.text.is_empty()
    }
    pub fn text_is(&self, needle: &str) -> bool {
        self.text == needle
    }
    /// Whether the text holds an ASCII letter or digit (a yes/no about the text, not the text).
    pub fn text_has_ascii_alphanumeric(&self) -> bool {
        self.text.chars().any(|c| c.is_ascii_alphanumeric())
    }
    pub fn text_contains(&self, needle: &str) -> bool {
        !needle.is_empty() && self.text.contains(needle)
    }
    /// Case-insensitive `text_contains` (ASCII lower-casing of both).
    pub fn text_contains_ci(&self, needle: &str) -> bool {
        !needle.is_empty()
            && self
                .text
                .to_ascii_lowercase()
                .contains(&needle.to_ascii_lowercase())
    }
    pub fn desc_is(&self, needle: &str) -> bool {
        self.content_desc == needle
    }
    pub fn desc_contains(&self, needle: &str) -> bool {
        !needle.is_empty() && self.content_desc.contains(needle)
    }
    /// The text in a [`Secret`] (a copy; the tree keeps its own until it is dropped).
    pub fn read_secret(&self) -> Secret {
        Secret::new(self.text.clone().into_bytes())
    }
    /// The content description in a [`Secret`].
    pub fn read_desc_secret(&self) -> Secret {
        Secret::new(self.content_desc.clone().into_bytes())
    }
}

#[derive(Default)]
pub struct UiTree {
    nodes: Vec<Node>,
}

impl fmt::Debug for UiTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UiTree")
            .field("nodes", &self.nodes.len())
            .field(
                "with_text",
                &self.nodes.iter().filter(|n| n.has_text()).count(),
            )
            .field(
                "with_id",
                &self
                    .nodes
                    .iter()
                    .filter(|n| !n.resource_id.is_empty())
                    .count(),
            )
            .finish()
    }
}

impl Drop for UiTree {
    /// Best effort in safe Rust: the texts are overwritten before their memory is freed.
    fn drop(&mut self) {
        for n in &mut self.nodes {
            super::secret::scrub(std::mem::take(&mut n.text).into_bytes());
            super::secret::scrub(std::mem::take(&mut n.content_desc).into_bytes());
        }
    }
}

impl UiTree {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter()
    }
    pub fn find(&self, mut pred: impl FnMut(&Node) -> bool) -> Vec<&Node> {
        self.nodes.iter().filter(|n| pred(n)).collect()
    }
    pub fn by_id(&self, id: &str) -> Vec<&Node> {
        self.find(|n| n.resource_id == id)
    }
    /// Resource id ending in `suffix` (e.g. `/send_button`), for ids whose package prefix varies.
    pub fn by_id_ending(&self, suffix: &str) -> Vec<&Node> {
        self.find(|n| !suffix.is_empty() && n.resource_id.ends_with(suffix))
    }
    pub fn by_text(&self, text: &str) -> Vec<&Node> {
        self.find(|n| n.text_is(text))
    }
    pub fn by_text_containing(&self, needle: &str) -> Vec<&Node> {
        self.find(|n| n.text_contains(needle))
    }
    pub fn by_desc(&self, desc: &str) -> Vec<&Node> {
        self.find(|n| n.desc_is(desc))
    }
    pub fn by_class(&self, class: &str) -> Vec<&Node> {
        self.find(|n| n.class == class)
    }
    pub fn has_id(&self, id: &str) -> bool {
        self.nodes.iter().any(|n| n.resource_id == id)
    }
    /// Whether this screen still shows `node` where it was: the same resource id, class, text,
    /// content-desc and bounds. A retried tap is made only then (`tap_and_wait`), so a tap that DID
    /// take effect is never repeated on whatever the next screen has at that place.
    pub fn still_shows(&self, node: &Node) -> bool {
        self.nodes.iter().any(|n| {
            n.resource_id == node.resource_id
                && n.class == node.class
                && n.text == node.text
                && n.content_desc == node.content_desc
                && n.bounds == node.bounds
        })
    }
    pub fn has_text(&self, text: &str) -> bool {
        self.nodes.iter().any(|n| n.text_is(text))
    }
    pub fn has_text_containing(&self, needle: &str) -> bool {
        self.nodes.iter().any(|n| n.text_contains(needle))
    }
    /// The one node with this id; an error when there is none or more than one.
    pub fn require_id(&self, id: &str) -> anyhow::Result<&Node> {
        let v = self.by_id(id);
        match v.as_slice() {
            [one] => Ok(one),
            [] => anyhow::bail!("no node with resource id {id}"),
            many => anyhow::bail!("{} nodes with resource id {id}", many.len()),
        }
    }
    pub fn parent(&self, n: &Node) -> Option<&Node> {
        n.parent.and_then(|p| self.nodes.get(p))
    }
    pub fn children(&self, n: &Node) -> Vec<&Node> {
        n.children
            .iter()
            .filter_map(|&c| self.nodes.get(c))
            .collect()
    }
    /// All nodes below `n`, depth first.
    pub fn descendants(&self, n: &Node) -> Vec<&Node> {
        let mut out = Vec::new();
        let mut stack: Vec<usize> = n.children.iter().rev().copied().collect();
        while let Some(i) = stack.pop() {
            if let Some(c) = self.nodes.get(i) {
                out.push(c);
                stack.extend(c.children.iter().rev().copied());
            }
        }
        out
    }
    /// The nearest ancestor (or `n` itself) that is clickable: a label inside a row is tapped through it.
    pub fn clickable_ancestor<'a>(&'a self, n: &'a Node) -> Option<&'a Node> {
        let mut cur = Some(n);
        while let Some(c) = cur {
            if c.clickable {
                return Some(c);
            }
            cur = self.parent(c);
        }
        None
    }

    /// Parse a uiautomator dump.
    pub fn parse(xml: &str) -> Result<UiTree, ParseError> {
        Parser {
            s: xml.as_bytes(),
            src: xml,
            pos: 0,
        }
        .run()
    }
}

const MAX_NODES: usize = 200_000;

struct Parser<'a> {
    s: &'a [u8],
    src: &'a str,
    pos: usize,
}

fn err<T>(offset: usize, what: &'static str) -> Result<T, ParseError> {
    Err(ParseError { offset, what })
}

impl Parser<'_> {
    fn run(mut self) -> Result<UiTree, ParseError> {
        let mut nodes: Vec<Node> = Vec::new();
        // (element name, arena index when it is a node)
        let mut stack: Vec<(String, Option<usize>)> = Vec::new();
        let mut seen_root = false;
        while let Some(rel) = self.s[self.pos..].iter().position(|&b| b == b'<') {
            let at = self.pos + rel;
            if self.s[self.pos..at]
                .iter()
                .any(|b| !b.is_ascii_whitespace())
                && stack.is_empty()
            {
                return err(self.pos, "text outside the root element");
            }
            self.pos = at;
            let rest = &self.s[at..];
            if rest.starts_with(b"<?") {
                self.pos = self.skip_to(b"?>", "unterminated declaration")?;
            } else if rest.starts_with(b"<!--") {
                self.pos = self.skip_to(b"-->", "unterminated comment")?;
            } else if rest.starts_with(b"<!") {
                self.pos = self.skip_to(b">", "unterminated declaration")?;
            } else if rest.starts_with(b"</") {
                let (name, end) = self.close_tag(at + 2)?;
                match stack.pop() {
                    Some((open, _)) if open == name => {}
                    Some(_) => return err(at, "closing tag does not match"),
                    None => return err(at, "closing tag without an opening one"),
                }
                self.pos = end;
            } else {
                let (name, attrs, self_closing, end) = self.open_tag(at + 1)?;
                if stack.is_empty() {
                    if seen_root {
                        return err(at, "second root element");
                    }
                    seen_root = true;
                }
                let idx = match name.as_str() {
                    "hierarchy" => None,
                    "node" => {
                        if nodes.len() >= MAX_NODES {
                            return err(at, "too many nodes");
                        }
                        let parent = stack.iter().rev().find_map(|(_, i)| *i);
                        let idx = nodes.len();
                        nodes.push(build_node(idx, parent, attrs, at)?);
                        if let Some(p) = parent {
                            nodes[p].children.push(idx);
                        }
                        Some(idx)
                    }
                    _ => return err(at, "unexpected element"),
                };
                if !self_closing {
                    stack.push((name, idx));
                }
                self.pos = end;
            }
        }
        if !stack.is_empty() {
            return err(self.s.len(), "unclosed element");
        }
        if !seen_root {
            return err(0, "no element");
        }
        Ok(UiTree { nodes })
    }

    fn skip_to(&self, pat: &[u8], what: &'static str) -> Result<usize, ParseError> {
        match self.s[self.pos..].windows(pat.len()).position(|w| w == pat) {
            Some(i) => Ok(self.pos + i + pat.len()),
            None => err(self.pos, what),
        }
    }

    fn close_tag(&self, from: usize) -> Result<(String, usize), ParseError> {
        let Some(rel) = self.s[from..].iter().position(|&b| b == b'>') else {
            return err(from, "unterminated closing tag");
        };
        let name = self.src[from..from + rel].trim();
        if name.is_empty() {
            return err(from, "empty closing tag");
        }
        Ok((name.to_string(), from + rel + 1))
    }

    #[allow(clippy::type_complexity)]
    fn open_tag(
        &self,
        from: usize,
    ) -> Result<(String, Vec<(String, String)>, bool, usize), ParseError> {
        let mut p = from;
        let s = self.s;
        let name_end = s[p..]
            .iter()
            .position(|&b| b.is_ascii_whitespace() || b == b'/' || b == b'>')
            .map(|i| p + i);
        let Some(name_end) = name_end else {
            return err(from, "unterminated tag");
        };
        if name_end == p {
            return err(p, "empty element name");
        }
        let name = self.src[p..name_end].to_string();
        p = name_end;
        let mut attrs = Vec::new();
        loop {
            while p < s.len() && s[p].is_ascii_whitespace() {
                p += 1;
            }
            match s.get(p) {
                None => return err(p, "unterminated tag"),
                Some(b'>') => return Ok((name, attrs, false, p + 1)),
                Some(b'/') => {
                    return if s.get(p + 1) == Some(&b'>') {
                        Ok((name, attrs, true, p + 2))
                    } else {
                        err(p, "stray slash in a tag")
                    };
                }
                Some(_) => {}
            }
            let an_end = s[p..]
                .iter()
                .position(|&b| b == b'=' || b.is_ascii_whitespace() || b == b'>' || b == b'/')
                .map(|i| p + i);
            let Some(an_end) = an_end else {
                return err(p, "unterminated tag");
            };
            if an_end == p {
                return err(p, "empty attribute name");
            }
            let aname = self.src[p..an_end].to_string();
            p = an_end;
            while p < s.len() && s[p].is_ascii_whitespace() {
                p += 1;
            }
            if s.get(p) != Some(&b'=') {
                return err(p, "attribute without a value");
            }
            p += 1;
            while p < s.len() && s[p].is_ascii_whitespace() {
                p += 1;
            }
            let q = match s.get(p) {
                Some(&q) if q == b'"' || q == b'\'' => q,
                _ => return err(p, "attribute value not quoted"),
            };
            p += 1;
            let Some(rel) = s[p..].iter().position(|&b| b == q) else {
                return err(p, "unterminated attribute value");
            };
            let raw = &self.src[p..p + rel];
            let value = decode_entities(raw).map_err(|what| ParseError { offset: p, what })?;
            p += rel + 1;
            if let Some(&c) = s.get(p)
                && !(c.is_ascii_whitespace() || c == b'/' || c == b'>')
            {
                return err(p, "attributes not separated");
            }
            attrs.push((aname, value));
        }
    }
}

fn decode_entities(raw: &str) -> Result<String, &'static str> {
    if !raw.contains('&') {
        return Ok(raw.to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        let Some(semi) = rest.find(';') else {
            return Err("unterminated entity");
        };
        let ent = &rest[..semi];
        rest = &rest[semi + 1..];
        match ent {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let code =
                    if let Some(h) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                        u32::from_str_radix(h, 16).ok()
                    } else if let Some(d) = ent.strip_prefix('#') {
                        d.parse::<u32>().ok()
                    } else {
                        return Err("unknown entity");
                    };
                match code.and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None => return Err("bad character reference"),
                }
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn parse_bounds(v: &str) -> Option<Bounds> {
    // [l,t][r,b]
    let v = v.strip_prefix('[')?.strip_suffix(']')?;
    let (a, b) = v.split_once("][")?;
    let (l, t) = a.split_once(',')?;
    let (r, bt) = b.split_once(',')?;
    Some(Bounds {
        left: l.trim().parse().ok()?,
        top: t.trim().parse().ok()?,
        right: r.trim().parse().ok()?,
        bottom: bt.trim().parse().ok()?,
    })
}

fn build_node(
    index: usize,
    parent: Option<usize>,
    attrs: Vec<(String, String)>,
    at: usize,
) -> Result<Node, ParseError> {
    let mut n = Node {
        index,
        parent,
        children: Vec::new(),
        text: String::new(),
        resource_id: String::new(),
        class: String::new(),
        package: String::new(),
        content_desc: String::new(),
        bounds: Bounds::default(),
        clickable: false,
        checked: false,
        selected: false,
        scrollable: false,
        enabled: true,
        focused: false,
    };
    let mut have_bounds = false;
    for (k, v) in attrs {
        let flag = |v: &str| match v {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => err(at, "boolean attribute is neither true nor false"),
        };
        match k.as_str() {
            "text" => n.text = v,
            "resource-id" => n.resource_id = v,
            "class" => n.class = v,
            "package" => n.package = v,
            "content-desc" => n.content_desc = v,
            "bounds" => {
                n.bounds = match parse_bounds(&v) {
                    Some(b) => b,
                    None => return err(at, "bad bounds"),
                };
                have_bounds = true;
            }
            "clickable" => n.clickable = flag(&v)?,
            "checked" => n.checked = flag(&v)?,
            "selected" => n.selected = flag(&v)?,
            "scrollable" => n.scrollable = flag(&v)?,
            "enabled" => n.enabled = flag(&v)?,
            "focused" => n.focused = flag(&v)?,
            _ => {}
        }
    }
    if !have_bounds {
        return err(at, "node without bounds");
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>
<hierarchy rotation="0">
  <node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="a.b" content-desc="" clickable="false" enabled="true" scrollable="false" bounds="[0,0][1080,1920]">
    <node index="0" text="Tom &amp; Jerry &lt;3 &quot;q&quot; &apos;a&apos; &#65;&#x42;" resource-id="a.b:id/title" class="android.widget.TextView" package="a.b" content-desc="" clickable="true" checked="true" selected="true" bounds="[10,20][110,60]" />
    <node index="1" text="" resource-id="a.b:id/list" class="android.widget.ListView" package="a.b" content-desc="the list" scrollable="true" bounds="[0,100][1080,1900]">
      <node index="0" text="row one" resource-id="a.b:id/row" class="android.widget.TextView" package="a.b" content-desc="" clickable="true" bounds="[0,100][1080,200]"/>
      <node index="1" text="row two" resource-id="a.b:id/row" class="android.widget.TextView" package="a.b" content-desc="" bounds="[0,200][1080,300]"/>
    </node>
  </node>
</hierarchy>"#;

    #[test]
    fn test_phone_parse_attributes_nesting_entities_bounds() {
        let t = UiTree::parse(DUMP).unwrap();
        assert_eq!(t.len(), 5);
        let title = t.require_id("a.b:id/title").unwrap();
        assert!(title.text_is("Tom & Jerry <3 \"q\" 'a' AB"));
        assert!(title.clickable() && title.checked() && title.selected());
        assert!(!title.scrollable());
        assert_eq!(
            title.bounds(),
            Bounds {
                left: 10,
                top: 20,
                right: 110,
                bottom: 60
            }
        );
        assert_eq!(title.bounds().center(), (60, 40));
        assert_eq!(title.class(), "android.widget.TextView");
        assert_eq!(title.package(), "a.b");
        let list = t.require_id("a.b:id/list").unwrap();
        assert!(list.scrollable() && list.desc_is("the list"));
        assert_eq!(t.children(list).len(), 2);
        let root = t.parent(list).unwrap();
        assert_eq!(root.class(), "android.widget.FrameLayout");
        assert!(t.parent(root).is_none());
        assert_eq!(t.descendants(root).len(), 4);
    }

    #[test]
    fn test_phone_queries() {
        let t = UiTree::parse(DUMP).unwrap();
        assert_eq!(t.by_id("a.b:id/row").len(), 2);
        assert!(t.require_id("a.b:id/row").is_err());
        assert!(t.require_id("a.b:id/none").is_err());
        assert_eq!(t.by_id_ending(":id/row").len(), 2);
        assert_eq!(t.by_text("row one").len(), 1);
        assert_eq!(t.by_text("row").len(), 0);
        assert_eq!(t.by_text_containing("row").len(), 2);
        assert_eq!(t.by_text_containing("").len(), 0);
        assert_eq!(t.by_desc("the list").len(), 1);
        assert_eq!(t.by_class("android.widget.TextView").len(), 3);
        assert!(t.has_id("a.b:id/list") && !t.has_id("x"));
        let row2 = t.by_text("row two")[0];
        let anc = t.clickable_ancestor(row2);
        assert!(anc.is_none());
        let row1 = t.by_text("row one")[0];
        assert_eq!(t.clickable_ancestor(row1).unwrap().index(), row1.index());
    }

    #[test]
    fn test_phone_debug_prints_no_text() {
        let t = UiTree::parse(DUMP).unwrap();
        let s = format!("{t:?}");
        assert!(!s.contains("Jerry") && !s.contains("row one"), "{s}");
        assert!(s.contains("nodes: 5"));
        let n = format!("{:?}", t.by_text("row one")[0]);
        assert!(
            !n.contains("row one") && n.contains("has_text: true"),
            "{n}"
        );
    }

    #[test]
    fn test_phone_parse_rejects_malformed() {
        let bad = [
            "",
            "   ",
            "<hierarchy>",
            "<hierarchy></node>",
            "<hierarchy><node bounds=\"[0,0][1,1]\"></hierarchy>",
            "<hierarchy><node bounds=\"[0,0][1,1]\" text=\"a></node></hierarchy>",
            "<hierarchy><node bounds=[0,0][1,1] /></hierarchy>",
            "<hierarchy><node /></hierarchy>",
            "<hierarchy><node bounds=\"[0,0][x,1]\" /></hierarchy>",
            "<hierarchy><node bounds=\"[0,0][1,1]\" text=\"&bogus;\" /></hierarchy>",
            "<hierarchy><node bounds=\"[0,0][1,1]\" text=\"&#xFFFFFFFF;\" /></hierarchy>",
            "<hierarchy><node bounds=\"[0,0][1,1]\" text=\"&amp\" /></hierarchy>",
            "<hierarchy><node bounds=\"[0,0][1,1]\" clickable=\"yes\" /></hierarchy>",
            "<hierarchy><other/></hierarchy>",
            "<hierarchy/><hierarchy/>",
            "junk <hierarchy/>",
            "<hierarchy><node bounds=\"[0,0][1,1]\"text=\"a\"/></hierarchy>",
            "<",
            "<hierarchy",
            "<?xml",
            "ERROR: null root node returned by UiTestAutomationBridge.",
        ];
        for b in bad {
            assert!(UiTree::parse(b).is_err(), "should fail: {b:?}");
        }
        assert_eq!(UiTree::parse("<hierarchy/>").unwrap().len(), 0);
    }

    #[test]
    fn test_phone_parse_error_carries_no_input() {
        let e = UiTree::parse(
            "<hierarchy><node bounds=\"[0,0][1,1]\" text=\"SECRET-1\" nope></node></hierarchy>",
        )
        .unwrap_err();
        let shown = format!("{e} {e:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
    }

    #[test]
    fn test_phone_parse_does_not_panic_on_truncations() {
        for cut in 0..DUMP.len() {
            if DUMP.is_char_boundary(cut) {
                let _ = UiTree::parse(&DUMP[..cut]);
            }
        }
        let deep = format!(
            "<hierarchy>{}{}</hierarchy>",
            "<node bounds=\"[0,0][1,1]\">".repeat(20_000),
            "</node>".repeat(20_000)
        );
        let t = UiTree::parse(&deep).unwrap();
        assert_eq!(t.len(), 20_000);
        let root = t.iter().next().unwrap();
        assert_eq!(t.descendants(root).len(), 19_999);
    }

    #[test]
    fn test_phone_read_secret_comes_from_the_node() {
        let t = UiTree::parse(DUMP).unwrap();
        let s = t.by_text("row one")[0].read_secret();
        assert!(s.with(|b| b == b"row one"));
    }
}
