//! `index.html`: the archive as a page, like Kourier's bubbles. INERT BY CONSTRUCTION: it shows
//! other people's content in a browser, so it is one self-contained file with a restrictive
//! Content-Security-Policy, no script, no event attribute, no `style` built from data, every text
//! escaped, links only http and https, a picture only when the archive holds it and its content
//! proved it one a browser may show.

use std::fmt::Write as _;

use super::entries::Shown;
use super::record::{FileOut, Item, Run, TextOut, http_href};
use crate::i18n::{trc, trfc, trnc};
use crate::richtext::escape_into;
use crate::time::{Calendar, TimeContext, file_size};

const CSS: &str = "\
:root{color-scheme:light dark;--bg:#f4f5f7;--fg:#1c1e21;--own:#d6e9ff;--other:#fff;--quiet:#6b7280;--line:#d8dce2;--link:#0b57d0}\
@media (prefers-color-scheme:dark){:root{--bg:#16181b;--fg:#e6e8eb;--own:#1f3a5f;--other:#25282d;--quiet:#9aa1ab;--line:#3a3f46;--link:#8ab4f8}}\
body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.4 system-ui,sans-serif}\
header,main,footer{max-width:46rem;margin:0 auto;padding:0 1rem}\
h1{font-size:1.4rem;margin:1.2rem 0 .2rem}.meta,.time,.notice,.gone,footer{color:var(--quiet);font-size:.85rem}\
h2.day{font-size:.9rem;text-align:center;color:var(--quiet);margin:1.4rem 0 .6rem}\
.msg{max-width:80%;margin:.35rem 0;padding:.45rem .7rem;border-radius:.8rem;background:var(--other);border:1px solid var(--line);overflow-wrap:anywhere}\
.msg.own{margin-left:auto;background:var(--own)}\
.name{font-weight:600;font-size:.85rem}.text{white-space:pre-wrap}\
.quote{border-left:3px solid var(--line);padding-left:.5rem;margin:.2rem 0;color:var(--quiet);font-size:.9rem}\
.notice{text-align:center;margin:.6rem 0}\
.spoiler{background:var(--line);border-radius:.2rem}.mention{font-weight:600}\
code{font-family:ui-monospace,monospace}a{color:var(--link)}\
img{max-width:100%;height:auto;border-radius:.4rem;display:block}\
.reactions{margin-top:.2rem}.reactions span{display:inline-block;margin-right:.5rem}\
.file{display:block;margin:.2rem 0}.time{text-align:right}details{font-size:.85rem;color:var(--quiet)}\
footer{padding:1.5rem 1rem}";

/// What the page needs besides the items.
pub(crate) struct Page<'a> {
    pub title: &'a str,
    pub lang: &'a str,
    pub now: i64,
    pub calendar: &'a dyn Calendar,
}

fn esc(out: &mut String, s: &str) {
    escape_into(out, s);
}

/// `files/<name>` as a URL: every byte but an unreserved one in `%XX`, the `/` kept.
fn url_path(entry: &str) -> String {
    let mut out = String::with_capacity(entry.len());
    for b in entry.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn runs(out: &mut String, t: &TextOut) {
    for r in &t.runs {
        run(out, r);
    }
}

/// A URL as a reader compares it with the text next to it: no `http://` or `https://`, no one
/// trailing `/`, no case.
fn bare(u: &str) -> String {
    let lower = u.to_lowercase();
    let t = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    t.strip_suffix('/').unwrap_or(t).to_owned()
}

fn run(out: &mut String, r: &Run) {
    let mut close: Vec<&str> = Vec::new();
    // A link whose visible text is not its address is written as the text, then the address in
    // brackets: only the address is a link, so the reader sees where it goes.
    let href = r.href.as_deref().and_then(http_href);
    let hidden = href.is_some_and(|h| bare(h) != bare(&r.text));
    if let Some(h) = href
        && !hidden
    {
        out.push_str("<a href=\"");
        esc(out, h);
        out.push_str("\" rel=\"noopener noreferrer\">");
        close.push("</a>");
    }
    if r.mention.is_some() {
        out.push_str("<span class=\"mention\">");
        close.push("</span>");
    }
    for s in &r.styles {
        let (o, c) = match *s {
            "bold" => ("<b>", "</b>"),
            "italic" => ("<i>", "</i>"),
            "strike" => ("<s>", "</s>"),
            "mono" => ("<code>", "</code>"),
            _ => ("<span class=\"spoiler\">", "</span>"),
        };
        out.push_str(o);
        close.push(c);
    }
    esc(out, &r.text);
    for c in close.iter().rev() {
        out.push_str(c);
    }
    if let Some(h) = href
        && hidden
    {
        out.push_str(" (<a href=\"");
        esc(out, h);
        out.push_str("\" rel=\"noopener noreferrer\">");
        esc(out, h);
        out.push_str("</a>)");
    }
}

fn file(out: &mut String, f: &FileOut) {
    let name = f.name.clone().unwrap_or_default();
    match (&f.path, f.state) {
        (Some(p), _) if f.shown == Shown::Picture => {
            let url = url_path(p);
            out.push_str("<a class=\"file\" href=\"");
            esc(out, &url);
            out.push_str("\"><img src=\"");
            esc(out, &url);
            out.push_str("\" alt=\"");
            esc(out, &name);
            out.push_str("\"></a>");
        }
        (Some(p), _) if f.shown == Shown::Link => {
            out.push_str("<a class=\"file\" href=\"");
            esc(out, &url_path(p));
            out.push_str("\">");
            esc(out, if name.is_empty() { p } else { &name });
            out.push_str(" · ");
            out.push_str(&file_size(f.size));
            out.push_str("</a>");
        }
        (Some(p), _) => {
            // No `href`: a file that a browser could open as a live page (HTML, SVG, XML, an
            // unknown or missing extension) is opened from the folder, not from this page.
            out.push_str("<span class=\"file\">");
            esc(out, if name.is_empty() { p } else { &name });
            out.push_str(" · ");
            out.push_str(&file_size(f.size));
            out.push_str(" · ");
            esc(out, p);
            out.push_str("<br><span class=\"gone\">");
            esc(
                out,
                &trc(
                    "@info:status a file in the exported chat that the page does not link",
                    "Open it from the folder, not from this page",
                ),
            );
            out.push_str("</span></span>");
        }
        (None, state) => {
            out.push_str("<span class=\"file gone\">");
            esc(out, &name);
            if !name.is_empty() {
                out.push_str(" · ");
            }
            let why = if state == "not_on_this_computer" {
                trc(
                    "@info:status a file missing from the exported chat",
                    "Not on this computer",
                )
            } else {
                trc(
                    "@info:status a file left out of the exported chat",
                    "Left out of this export",
                )
            };
            esc(out, &why);
            out.push_str("</span>");
        }
    }
    if let Some(c) = &f.caption {
        out.push_str("<div class=\"text\">");
        esc(out, c);
        out.push_str("</div>");
    }
}

/// The page.
pub(crate) fn index(items: &[Item], page: &Page<'_>) -> String {
    // Absolute dates: a clock far in the future makes every header the full date.
    let far = TimeContext::new(4_102_444_800_000, page.calendar);
    let here = TimeContext::new(page.now, page.calendar);
    let mut o = String::with_capacity(1024 + items.len() * 400);
    o.push_str("<!doctype html>\n<html lang=\"");
    esc(&mut o, page.lang);
    o.push_str("\">\n<head>\n<meta charset=\"utf-8\">\n");
    o.push_str("<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src 'self' file:; style-src 'unsafe-inline'; media-src 'self'; base-uri 'none'; form-action 'none'\">\n");
    o.push_str("<meta name=\"referrer\" content=\"no-referrer\">\n");
    o.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>");
    esc(&mut o, page.title);
    o.push_str("</title>\n<style>");
    o.push_str(CSS);
    o.push_str("</style>\n</head>\n<body>\n<header><h1>");
    esc(&mut o, page.title);
    o.push_str("</h1><p class=\"meta\">");
    let count = trnc(
        "@info:intext heading of the exported chat page",
        "%1 message",
        "%1 messages",
        items.len() as u64,
    );
    esc(&mut o, &count);
    o.push_str(" · ");
    let when = format!("{} {}", here.short_date(page.now), here.clock(page.now));
    esc(
        &mut o,
        &trfc(
            "@info:intext heading of the exported chat page, %1 a date and time",
            "Exported %1",
            &[&when],
        ),
    );
    o.push_str("</p></header>\n<main>\n");
    let mut day = None;
    for it in items {
        let d = far.day_number(it.ts);
        if day != Some(d) {
            day = Some(d);
            o.push_str("<h2 class=\"day\">");
            esc(&mut o, &far.date_header(d));
            o.push_str("</h2>\n");
        }
        message(&mut o, it, &here);
    }
    o.push_str("</main>\n<footer>");
    esc(
        &mut o,
        &trc(
            "@info:intext footer of the exported chat page",
            "Exported with Kourier",
        ),
    );
    o.push_str("</footer>\n</body>\n</html>\n");
    o
}

fn message(o: &mut String, it: &Item, ctx: &TimeContext<'_>) {
    let quiet = matches!(it.kind, "notice" | "call");
    if quiet {
        let _ = write!(o, "<div class=\"notice\" id=\"{}\">", it.id);
        esc(o, &it.words);
        let _ = writeln!(o, " · {}</div>", ctx.clock(it.ts));
        return;
    }
    let _ = write!(
        o,
        "<div class=\"msg {}\" id=\"{}\">",
        if it.own { "own" } else { "other" },
        it.id
    );
    if !it.own
        && let Some(n) = &it.author_name
    {
        o.push_str("<div class=\"name\">");
        esc(o, n);
        o.push_str("</div>");
    }
    if let Some(r) = &it.reply {
        o.push_str("<div class=\"quote\">");
        if let Some(n) = &it.reply_author_name {
            o.push_str("<b>");
            esc(o, n);
            o.push_str("</b> ");
        }
        esc(o, &r.excerpt);
        o.push_str("</div>");
    }
    if let Some(t) = &it.text {
        o.push_str("<div class=\"text\">");
        runs(o, t);
        o.push_str("</div>");
    }
    if !it.words.is_empty() {
        o.push_str("<div class=\"text\">");
        esc(o, &it.words);
        o.push_str("</div>");
    }
    if it.kind == "poll"
        && let Some((_, super::record::J::Obj(f))) = &it.special
        && let Some((_, super::record::J::Arr(opts))) = f.iter().find(|(k, _)| *k == "options")
    {
        o.push_str("<ul>");
        for op in opts {
            if let super::record::J::Raw(serde_json::Value::String(s)) = op {
                o.push_str("<li>");
                esc(o, s);
                o.push_str("</li>");
            }
        }
        o.push_str("</ul>");
    }
    for f in &it.files {
        file(o, f);
    }
    if !it.reactions.is_empty() {
        o.push_str("<div class=\"reactions\">");
        for (emoji, by) in &it.reactions {
            o.push_str("<span>");
            esc(o, emoji);
            if by.len() > 1 {
                let _ = write!(o, " {}", by.len());
            }
            o.push_str("</span>");
        }
        o.push_str("</div>");
    }
    o.push_str("<div class=\"time\">");
    o.push_str(&ctx.clock(it.ts));
    if it.edited_at.is_some() {
        o.push_str(" · ");
        esc(o, &trc("@info:status on an exported message", "edited"));
    }
    o.push_str("</div>");
    if !it.edits.is_empty() {
        o.push_str("<details><summary>");
        esc(
            o,
            &trc(
                "@info:status earlier versions of an exported message",
                "Earlier versions",
            ),
        );
        o.push_str("</summary>");
        for (at, t) in &it.edits {
            o.push_str("<div class=\"text\">");
            esc(o, &t.plain);
            let _ = at;
            o.push_str("</div>");
        }
        o.push_str("</details>");
    }
    o.push_str("</div>\n");
}
