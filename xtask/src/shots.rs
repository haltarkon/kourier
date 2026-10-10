//! `cargo xtask shots`: the screenshot matrix of the UI (§10.1), rendered by several app starts
//! at a time and compared with stored pictures.
//!
//! Every page renders through the real core on a temporary profile, in two window sizes and two
//! Breeze color schemes ([`super::SHOT_PAGES`]). A render into the default directory is compared
//! with the picture of the same name in the baseline directory (`target/shots-baseline`, local:
//! the pixels depend on the installed fonts, Breeze and Qt). A picture that differs is rendered
//! again, and only one that differs in every render counts as changed (a page that still loads
//! when it is grabbed differs in a few pixels now and then). `cargo xtask ci` skips the whole
//! step when nothing that can change a picture changed since the last full compared render.
//!
//! Renders share nothing. Each app start makes its own temporary profile (`KOURIER_HOME`), takes
//! no D-Bus name (offscreen: the desktop integration is off) and opens no port; the Breeze
//! `kdeglobals` of each color scheme is written once before the first render and only read by
//! them; each picture has its own file name.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use super::{
    SHOT_PAGES, SHOT_SIZES, SHOT_THEMES, cargo, color_scheme_file, qml_problems, root, run,
    shot_luma, shot_mode, target_dir, theme_home, theme_problem,
};

/// App starts at a time when `--jobs` is not given.
const DEFAULT_JOBS: usize = 4;

/// Pages whose picture is random by design, so there is nothing to compare: the rotating QR code
/// carries a fresh random payload in every render (thousands of pixels differ). Both sizes and
/// both color schemes are skipped; the checks of a render (clean exit, no QML warning, theme)
/// still apply.
const SHOT_PAGES_NOT_COMPARED: [&str; 1] = ["qr"];

/// A pixel differs when any of its channels differs by more than this.
const PIXEL_TOLERANCE: u8 = 24;

/// A picture differs when more than this many of its pixels differ. An absolute number, not a
/// share of the picture: a label that gains a few words changes some 270 pixels, which is
/// 0.08 % of a narrow page and 0.03 % of a wide one, and a limit of 0.05 % let the wide page
/// pass (seen with "Read more (N words)"). One changed character is more than this; what is
/// below it is the odd pixel of an edge drawn a moment early. Pictures that differ by load
/// timing are settled by the re-renders.
const MAX_DIFFERING_PIXELS: usize = 24;

/// Pages that show pictures. A picture is grabbed a fixed time after the page opened, and a
/// thumbnail in it may or may not have been replaced by its full-size picture yet (more so
/// with several renders at a time): the edges of the shapes in it then differ by a few
/// hundred pixels from render to render, in every one of three renders. Until a screenshot
/// waits for the page's pictures to be final, these pages are compared with the looser
/// [`PICTURE_PAGE_ONE_IN`]; a change of their text or layout smaller than that is not seen.
const SHOT_PAGES_WITH_PICTURES: [&str; 19] = [
    "media",
    "viewer",
    "files",
    "album",
    "albumviewer",
    "revisions",
    "originals",
    "mediamenu",
    "deleteattachment",
    "folderfiles",
    "folderzip",
    // The chats of these pages show a picture too (`mediaShot` in `MainWindow.qml`): a full
    // run of 2026-10-08 flagged `edit` and `compose` for 48 and 68 pixels inside a photo.
    "compose",
    "folder",
    "quotes",
    "menu",
    "react",
    "edit",
    "reply",
    "delete",
];

/// A page with pictures differs when more than one pixel in this many differs (0.15 %).
const PICTURE_PAGE_ONE_IN: usize = 667;

/// Renders of a picture after the first one when it differs from the baseline, and when its only
/// problem is a QML warning.
const RETRIES: usize = 2;

/// The directory `cargo xtask shots` writes to unless it is given another one.
fn default_dir() -> PathBuf {
    root().join("target/shots")
}

/// The stored pictures the default directory's render is compared with.
fn baseline_dir() -> PathBuf {
    root().join("target/shots-baseline")
}

/// The command line of `cargo xtask shots`.
#[derive(Debug, PartialEq)]
struct Args {
    /// Where the pictures go; none: [`default_dir`], with the comparison.
    dir: Option<PathBuf>,
    /// Only these pages (`--page a,b`).
    pages: Option<Vec<String>>,
    /// Only this color scheme (`--theme light|dark`).
    theme: Option<String>,
    /// Only this window size (`--size wide|narrow`).
    size: Option<String>,
    /// App starts at a time.
    jobs: usize,
    /// Build the app first; `--no-build` renders with the binary that is there.
    build: bool,
    /// Make the pictures just rendered the baseline.
    accept: bool,
    /// Compare with the baseline; `--no-compare` only renders and checks.
    compare: bool,
    /// `--matrix`: the look-at run of [`VARIANTS`] instead of the compared one.
    matrix: bool,
    /// Only these variants of the matrix run (`--variant a,b`).
    variants: Option<Vec<String>>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            dir: None,
            pages: None,
            theme: None,
            size: None,
            jobs: DEFAULT_JOBS,
            build: true,
            accept: false,
            compare: true,
            matrix: false,
            variants: None,
        }
    }
}

impl Args {
    /// Every page, size and color scheme: the render a stamp can vouch for.
    fn is_full(&self) -> bool {
        self.pages.is_none() && self.theme.is_none() && self.size.is_none()
    }
}

/// Parses `[dir] [--page a,b,...] [--theme light|dark] [--size wide|narrow] [--jobs N]
/// [--no-build] [--accept] [--no-compare]`; a value follows its flag as `--page=a` or `--page a`.
fn parse_args(args: &[String]) -> Result<Args> {
    let mut out = Args::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (arg.as_str(), None),
        };
        let mut value = || -> Result<String> {
            match inline {
                Some(v) => Ok(v.to_owned()),
                None => it
                    .next()
                    .cloned()
                    .with_context(|| format!("{flag} needs a value")),
            }
        };
        match flag {
            "--page" => {
                let list = value()?;
                let pages = out.pages.get_or_insert_with(Vec::new);
                for page in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    if !SHOT_PAGES.contains(&page) {
                        bail!(
                            "--page: unknown page {page:?} (see SHOT_PAGES in xtask/src/main.rs)"
                        );
                    }
                    pages.push(page.to_owned());
                }
                if pages.is_empty() {
                    bail!("--page needs at least one page");
                }
            }
            "--theme" => {
                let theme = value()?;
                if !SHOT_THEMES.iter().any(|(name, ..)| *name == theme) {
                    bail!("--theme: {theme:?} is not light or dark");
                }
                out.theme = Some(theme);
            }
            "--size" => {
                let size = value()?;
                if !SHOT_SIZES.iter().any(|(name, ..)| *name == size) {
                    bail!("--size: {size:?} is not wide or narrow");
                }
                out.size = Some(size);
            }
            "--jobs" => {
                let jobs = value()?;
                out.jobs = match jobs.parse() {
                    Ok(n) if n >= 1 => n,
                    _ => bail!("--jobs: {jobs:?} is not a number of at least 1"),
                };
            }
            "--variant" => {
                let list = value()?;
                let variants = out.variants.get_or_insert_with(Vec::new);
                for name in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    if !VARIANTS.iter().any(|v| v.name == name) {
                        let known: Vec<_> = VARIANTS.iter().map(|v| v.name).collect();
                        bail!(
                            "--variant: unknown variant {name:?} (one of {})",
                            known.join(", ")
                        );
                    }
                    variants.push(name.to_owned());
                }
                if variants.is_empty() {
                    bail!("--variant needs at least one variant");
                }
            }
            "--no-build" | "--accept" | "--no-compare" | "--matrix" => {
                if inline.is_some() {
                    bail!("{flag} takes no value");
                }
                match flag {
                    "--no-build" => out.build = false,
                    "--accept" => out.accept = true,
                    "--matrix" => out.matrix = true,
                    _ => out.compare = false,
                }
            }
            flag if flag.starts_with('-') => bail!("unknown option {flag}"),
            _ => {
                if out.dir.replace(PathBuf::from(arg)).is_some() {
                    bail!("only one directory can be given (got {arg:?} as well)");
                }
            }
        }
    }
    if out.accept && out.dir.is_some() {
        bail!(
            "--accept makes the pictures of the default directory the baseline; drop the directory"
        );
    }
    if out.matrix && (out.accept || out.dir.is_some() || out.theme.is_some()) {
        bail!(
            "--matrix renders into target/shots-matrix/<variant>/ with each variant's own color \
             scheme and compares nothing: drop --accept, --theme and the directory"
        );
    }
    if out.variants.is_some() && !out.matrix {
        bail!("--variant belongs to --matrix");
    }
    Ok(out)
}

/// One picture: a page in a size and a color scheme.
#[derive(Clone, Copy, Debug)]
struct Job {
    page: &'static str,
    size: (&'static str, u32, u32),
    theme: (&'static str, &'static str, &'static str),
}

impl Job {
    /// `<page>-<size>-<theme>`, the picture's file stem.
    fn name(&self) -> String {
        format!("{}-{}-{}", self.page, self.size.0, self.theme.0)
    }
}

/// The pictures `args` asks for, color scheme by color scheme, page by page, then size.
fn select_jobs(args: &Args) -> Vec<Job> {
    let mut out = Vec::new();
    for theme in SHOT_THEMES {
        if args.theme.as_deref().is_some_and(|t| t != theme.0) {
            continue;
        }
        for page in SHOT_PAGES {
            if args
                .pages
                .as_ref()
                .is_some_and(|p| !p.iter().any(|p| p == page))
            {
                continue;
            }
            for size in SHOT_SIZES {
                if args.size.as_deref().is_some_and(|s| s != size.0) {
                    continue;
                }
                out.push(Job { page, size, theme });
            }
        }
    }
    out
}

/// A decoded picture: the pixels (red, green, blue, alpha) row by row.
struct Image {
    width: u32,
    height: u32,
    pixels: Vec<[u8; 4]>,
}

impl Image {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

fn read_png(path: &Path) -> Result<Image> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .with_context(|| format!("decode {}", path.display()))?;
    let mut buf = vec![0; reader.output_buffer_size().context("picture too large")?];
    let info = reader
        .next_frame(&mut buf)
        .with_context(|| format!("decode {}", path.display()))?;
    buf.truncate(info.buffer_size());
    let pixels = match info.color_type {
        png::ColorType::Rgba => buf.as_chunks::<4>().0.to_vec(),
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::Grayscale => buf.iter().map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Indexed => bail!("{}: palette left after expansion", path.display()),
    };
    Ok(Image {
        width: info.width,
        height: info.height,
        pixels,
    })
}

fn write_png(path: &Path, image: &Image) -> Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(image.pixels.as_flattened())?;
    writer.finish()?;
    Ok(())
}

/// Whether two pixels differ: any channel by more than [`PIXEL_TOLERANCE`].
fn pixel_differs(a: &[u8; 4], b: &[u8; 4]) -> bool {
    a.iter()
        .zip(b)
        .any(|(x, y)| x.abs_diff(*y) > PIXEL_TOLERANCE)
}

/// How a render differs from the baseline.
#[derive(Debug, PartialEq, Eq)]
enum Diff {
    /// The pictures have different sizes.
    Size {
        baseline: (u32, u32),
        render: (u32, u32),
    },
    /// Same size: `differing` of `total` pixels differ.
    Pixels { differing: usize, total: usize },
}

impl Diff {
    /// Whether this is a change of a picture of `page`: another size, or more differing
    /// pixels than [`MAX_DIFFERING_PIXELS`] (for [`SHOT_PAGES_WITH_PICTURES`]: than one in
    /// [`PICTURE_PAGE_ONE_IN`]).
    fn is_change(&self, page: &str) -> bool {
        match self {
            Diff::Size { .. } => true,
            Diff::Pixels { differing, total } => {
                if SHOT_PAGES_WITH_PICTURES.contains(&page) {
                    differing * PICTURE_PAGE_ONE_IN > *total
                } else {
                    *differing > MAX_DIFFERING_PIXELS
                }
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Diff::Size { baseline, render } => format!(
                "size {}x{} -> {}x{}",
                baseline.0, baseline.1, render.0, render.1
            ),
            Diff::Pixels { differing, total } => format!(
                "{differing} of {total} pixels ({:.2} %)",
                *differing as f64 * 100.0 / (*total).max(1) as f64
            ),
        }
    }
}

fn compare(baseline: &Image, render: &Image) -> Diff {
    if baseline.size() != render.size() {
        return Diff::Size {
            baseline: baseline.size(),
            render: render.size(),
        };
    }
    let differing = baseline
        .pixels
        .iter()
        .zip(&render.pixels)
        .filter(|(a, b)| pixel_differs(a, b))
        .count();
    Diff::Pixels {
        differing,
        total: render.pixels.len(),
    }
}

/// Space between the three panels of a diff picture.
const PANEL_GAP: u32 = 8;

/// The baseline, the render and the render dimmed with its differing pixels marked, side by
/// side (a size change marks nothing: the panels show it).
fn diff_image(baseline: &Image, render: &Image) -> Image {
    let panel_w = baseline.width.max(render.width);
    let height = baseline.height.max(render.height);
    let width = 3 * panel_w + 2 * PANEL_GAP;
    let mut canvas = Image {
        width,
        height,
        pixels: vec![[128, 128, 128, 255]; width as usize * height as usize],
    };
    let paste = |canvas: &mut Image, image: &Image, x0: u32| {
        for (y, row) in image.pixels.chunks(image.width as usize).enumerate() {
            let at = y * width as usize + x0 as usize;
            canvas.pixels[at..at + row.len()].copy_from_slice(row);
        }
    };
    paste(&mut canvas, baseline, 0);
    paste(&mut canvas, render, panel_w + PANEL_GAP);
    let dimmed = Image {
        width: render.width,
        height: render.height,
        pixels: render
            .pixels
            .iter()
            .map(|p| [p[0] / 3 + 170, p[1] / 3 + 170, p[2] / 3 + 170, 255])
            .collect(),
    };
    let marked_x0 = 2 * (panel_w + PANEL_GAP);
    paste(&mut canvas, &dimmed, marked_x0);
    if baseline.size() == render.size() {
        let w = render.width as usize;
        let h = render.height as usize;
        for (i, (a, b)) in baseline.pixels.iter().zip(&render.pixels).enumerate() {
            if !pixel_differs(a, b) {
                continue;
            }
            // A 3 x 3 block, so a thin difference (a cursor) is visible in a whole-window picture.
            let (x, y) = (i % w, i / w);
            for py in y.saturating_sub(1)..=(y + 1).min(h - 1) {
                for px in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                    canvas.pixels[py * width as usize + marked_x0 as usize + px] =
                        [255, 0, 255, 255];
                }
            }
        }
    }
    canvas
}

/// What happened to a picture's comparison.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Not compared (another directory, `--no-compare`, a page in [`SHOT_PAGES_NOT_COMPARED`], or
    /// the render failed).
    NotCompared,
    /// Within the limits of the baseline in render number `renders` (1: the first).
    Same { renders: usize },
    /// No baseline yet: this render became it.
    NewBaseline,
    /// `--accept`: this render became the baseline.
    Accepted,
    /// Differs from the baseline in every render; the text says how.
    Changed(String),
}

/// What one picture's render (and re-renders) found.
struct Outcome {
    failures: Vec<String>,
    luma: Option<u32>,
    /// The render printed QML problems (they are in `failures`).
    qml_warnings: bool,
    verdict: Verdict,
}

impl Outcome {
    /// The progress line after the picture's name.
    fn describe(&self) -> String {
        let mut out = String::new();
        if self.qml_warnings {
            out.push_str(" (QML warnings)");
        }
        match &self.verdict {
            Verdict::NotCompared | Verdict::Same { renders: 1 } => {}
            Verdict::Same { renders } => out.push_str(&format!(" (same in render {renders})")),
            Verdict::NewBaseline => out.push_str(" (new baseline)"),
            Verdict::Accepted => out.push_str(" (accepted)"),
            Verdict::Changed(how) => out.push_str(&format!(" CHANGED: {how}")),
        }
        out
    }
}

/// How the pictures of a run are handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Render and check only.
    Plain,
    /// Compare with the baseline.
    Compare,
    /// Make the renders the baseline.
    Accept,
}

/// Everything the workers share.
struct Ctx<'a> {
    bin: &'a Path,
    dir: &'a Path,
    /// The config home (Breeze `kdeglobals`) of each color scheme, by name.
    homes: BTreeMap<&'static str, PathBuf>,
    mode: Mode,
    /// The matrix run's variant: what it changes in the render's environment.
    variant: Option<&'a Variant>,
}

impl Ctx<'_> {
    fn png(&self, job: &Job) -> PathBuf {
        self.dir.join(format!("{}.png", job.name()))
    }
}

/// The checks of one render of a picture and its luminance.
#[derive(Default)]
struct Render {
    /// A start that did not exit cleanly, no picture, no luminance.
    failures: Vec<String>,
    /// QML problems from the app's stderr.
    qml: Vec<String>,
    luma: Option<u32>,
}

/// Renders `job` once into its file (`label` names it in failures).
fn render(ctx: &Ctx, job: &Job, label: &str) -> Render {
    let png = ctx.png(job);
    let _ = std::fs::remove_file(&png);
    let (_, w, h) = job.size;
    let mut render = Render::default();
    let mut command = Command::new(ctx.bin);
    command
        .arg(shot_mode(job.page))
        .arg(format!("--page={}", job.page))
        .arg(format!("--size={w}x{h}"))
        .arg(format!("--screenshot={}", png.display()))
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_QUICK_BACKEND", "software")
        .env("QT_QPA_PLATFORMTHEME", "kde")
        .env("QT_QUICK_CONTROLS_STYLE", "org.kde.desktop")
        .env("QT_SCALE_FACTOR", "1")
        .env("QT_FORCE_STDERR_LOGGING", "1")
        .env("XDG_CONFIG_HOME", &ctx.homes[job.theme.0])
        .env("LANG", "en_US.UTF-8")
        .env("LANGUAGE", "en");
    if let Some(variant) = ctx.variant {
        for (name, value) in variant.env {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        if variant.catalogs {
            // The locale of the variant decides, not the one this command was started in.
            command
                .env_remove("LC_ALL")
                .env_remove("LC_MESSAGES")
                .env("KOURIER_LOCALE_DIR", locale_dir());
        }
    }
    let out = command.output();
    let out = match out {
        Ok(out) => out,
        Err(e) => {
            render
                .failures
                .push(format!("{label}: run {}: {e}", ctx.bin.display()));
            return render;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let problems = qml_problems(&stderr);
    if !out.status.success() {
        render
            .failures
            .push(format!("{label}: exit {}", out.status));
    }
    if !png.is_file() {
        render
            .failures
            .push(format!("{label}: no screenshot written"));
    }
    render.qml = problems.iter().map(|p| format!("{label}: {p}")).collect();
    render.luma = shot_luma(&stdout);
    if render.luma.is_none() {
        render
            .failures
            .push(format!("{label}: no luminance reported"));
    }
    render
}

/// Renders a picture, compares it with the baseline as `ctx.mode` says and returns what was found.
fn process(ctx: &Ctx, job: &Job) -> Outcome {
    let name = job.name();
    let last = render(ctx, job, &name);
    let mut out = Outcome {
        qml_warnings: !last.qml.is_empty(),
        failures: last.failures.into_iter().chain(last.qml).collect(),
        luma: last.luma,
        verdict: Verdict::NotCompared,
    };
    let compared = !SHOT_PAGES_NOT_COMPARED.contains(&job.page);
    if ctx.mode == Mode::Plain || !compared || !ctx.png(job).is_file() {
        return out;
    }
    match store_or_compare(ctx, job, &mut out) {
        Ok(verdict) => out.verdict = verdict,
        Err(e) => out.failures.push(format!("{name}: {e:#}")),
    }
    out
}

/// `--accept` and a first render: the picture becomes the baseline (a render that failed a check
/// does not). Otherwise the comparison with the baseline, with the re-renders and the diff
/// picture of a change.
fn store_or_compare(ctx: &Ctx, job: &Job, out: &mut Outcome) -> Result<Verdict> {
    let name = job.name();
    let png = ctx.png(job);
    let stored = baseline_dir().join(format!("{name}.png"));
    let diff_png = ctx.dir.join("diff").join(format!("{name}.png"));
    let _ = std::fs::remove_file(&diff_png);
    let accept = |out: &Outcome, verdict: Verdict| -> Result<Verdict> {
        if !out.failures.is_empty() {
            return Ok(Verdict::NotCompared);
        }
        std::fs::create_dir_all(baseline_dir())?;
        std::fs::copy(&png, &stored)
            .with_context(|| format!("store {} as the baseline", png.display()))?;
        Ok(verdict)
    };
    if ctx.mode == Mode::Accept {
        return accept(out, Verdict::Accepted);
    }
    if !stored.is_file() {
        return accept(out, Verdict::NewBaseline);
    }
    let baseline = read_png(&stored)?;
    let mut renders = 1;
    let mut diff = compare(&baseline, &read_png(&png)?);
    while diff.is_change(job.page) && renders <= RETRIES {
        renders += 1;
        let again = render(ctx, job, &format!("{name} (render {renders})"));
        out.failures.extend(again.failures);
        if !png.is_file() {
            break;
        }
        diff = compare(&baseline, &read_png(&png)?);
    }
    if !diff.is_change(job.page) {
        return Ok(Verdict::Same { renders });
    }
    let render = read_png(&png)?;
    std::fs::create_dir_all(ctx.dir.join("diff"))?;
    write_png(&diff_png, &diff_image(&baseline, &render))?;
    Ok(Verdict::Changed(diff.describe()))
}

/// Runs `jobs` on `workers` threads; the progress line of each picture is printed as it
/// finishes. The outcomes come back in the order of `jobs`.
fn render_all(ctx: &Ctx, jobs: &[Job], workers: usize) -> Vec<Outcome> {
    let next = AtomicUsize::new(0);
    let finished = Mutex::new(Vec::with_capacity(jobs.len()));
    std::thread::scope(|scope| {
        for _ in 0..workers.clamp(1, jobs.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    let out = process(ctx, job);
                    let mut done = finished.lock().unwrap();
                    let line = format!(
                        "shots: [{}/{}] {}.png{}",
                        done.len() + 1,
                        jobs.len(),
                        job.name(),
                        out.describe()
                    );
                    done.push((i, out));
                    eprintln!("{line}");
                }
            });
        }
    });
    let mut finished = finished.into_inner().unwrap();
    finished.sort_by_key(|(i, _)| *i);
    finished.into_iter().map(|(_, out)| out).collect()
}

/// The pages of the look-at run: the list, a chat, the composer, the settings and a chat with a
/// long text. Together they hold most kinds of control the UI is made of.
const MATRIX_PAGES: [&str; 5] = ["list", "chat", "compose", "settings", "readmore"];

/// Where the look-at run writes: a directory per variant below this one.
/// Where the language variants of the matrix find their catalogs (what `cargo xtask i18n
/// build` writes by default).
fn locale_dir() -> PathBuf {
    target_dir().join("locale")
}

fn matrix_dir() -> PathBuf {
    root().join("target/shots-matrix")
}

/// A color scheme file for the look-at run.
enum Scheme {
    /// One KDE installed: `/usr/share/color-schemes/<name>.colors`.
    Installed(&'static str),
    /// The text of a file of this repository (`xtask/assets/`).
    Bundled(&'static str),
}

/// A general font of 14 pt (the default is 10 pt) and a small one of 11 pt (8 pt), written the
/// way Plasma's font settings write them to `kdeglobals`; the family is the one the render
/// uses anyway.
const FONT_14_PT: &str = "[General]\nfont=Noto Sans,14,-1,5,400,0,0,0,0,0,0,0,0,0,0,1\n\
    smallestReadableFont=Noto Sans,11,-1,5,400,0,0,0,0,0,0,0,0,0,0,1\n";

/// One variant of the look-at run: the usual render with one thing changed.
struct Variant {
    name: &'static str,
    /// What it shows (the summary line).
    about: &'static str,
    /// `light` or `dark`: the last part of its picture names.
    theme: &'static str,
    scheme: Scheme,
    icons: &'static str,
    /// Lines added to the render's `kdeglobals`.
    kdeglobals: &'static str,
    /// Changes to the render's environment after the usual ones: set, or remove (`None`).
    env: &'static [(&'static str, Option<&'static str>)],
    /// The render reads the catalogs of `cargo xtask i18n build` (a language variant).
    catalogs: bool,
}

/// The variants of `cargo xtask shots --matrix`, each a place where the default pictures cannot
/// show a fault. Breeze Light is the base of those that do not change the color scheme.
const VARIANTS: [Variant; 9] = [
    Variant {
        name: "classic",
        about: "Breeze Classic color scheme",
        theme: "light",
        scheme: Scheme::Installed("BreezeClassic"),
        icons: "breeze",
        kdeglobals: "",
        env: &[],
        catalogs: false,
    },
    Variant {
        name: "hc-light",
        about: "high-contrast light scheme (xtask/assets/HighContrastLight.colors)",
        theme: "light",
        scheme: Scheme::Bundled(include_str!("../assets/HighContrastLight.colors")),
        icons: "breeze",
        kdeglobals: "",
        env: &[],
        catalogs: false,
    },
    Variant {
        name: "hc-dark",
        about: "high-contrast dark scheme (xtask/assets/HighContrastDark.colors)",
        theme: "dark",
        scheme: Scheme::Bundled(include_str!("../assets/HighContrastDark.colors")),
        icons: "breeze-dark",
        kdeglobals: "",
        env: &[],
        catalogs: false,
    },
    Variant {
        name: "font-14",
        about: "general font 14 pt, small font 11 pt (Breeze Light)",
        theme: "light",
        scheme: Scheme::Installed("BreezeLight"),
        icons: "breeze",
        kdeglobals: FONT_14_PT,
        env: &[],
        catalogs: false,
    },
    Variant {
        name: "rtl",
        about: "right-to-left layout from a Hebrew locale (Breeze Light)",
        theme: "light",
        scheme: Scheme::Installed("BreezeLight"),
        icons: "breeze",
        kdeglobals: "",
        // Qt takes the layout direction from the locale unless the application sets one; Hebrew
        // is right-to-left and, unlike Arabic, written with the digits the pages already show.
        env: &[("LANG", Some("he_IL.UTF-8")), ("LANGUAGE", Some("he"))],
        catalogs: false,
    },
    Variant {
        name: "no-kde",
        about: "no KDE platform theme, the style the application picks itself (Breeze Light)",
        theme: "light",
        scheme: Scheme::Installed("BreezeLight"),
        icons: "breeze",
        kdeglobals: "",
        env: &[
            ("QT_QPA_PLATFORMTHEME", None),
            ("QT_QUICK_CONTROLS_STYLE", None),
        ],
        catalogs: false,
    },
    Variant {
        name: "no-kde-fusion",
        about: "no KDE platform theme, Qt's Fusion style (Breeze Light)",
        theme: "light",
        scheme: Scheme::Installed("BreezeLight"),
        icons: "breeze",
        kdeglobals: "",
        env: &[
            ("QT_QPA_PLATFORMTHEME", None),
            ("QT_QUICK_CONTROLS_STYLE", Some("Fusion")),
        ],
        catalogs: false,
    },
    Variant {
        name: "x-test",
        about: "the pseudo-language: every translatable text is wrapped in xx…xx (Breeze Light)",
        theme: "light",
        scheme: Scheme::Installed("BreezeLight"),
        icons: "breeze",
        kdeglobals: "",
        env: &[("LANGUAGE", Some("x-test"))],
        catalogs: true,
    },
    Variant {
        name: "uk",
        about: "the Ukrainian translation (Breeze Light)",
        theme: "light",
        scheme: Scheme::Installed("BreezeLight"),
        icons: "breeze",
        kdeglobals: "",
        env: &[("LANG", Some("uk_UA.UTF-8")), ("LANGUAGE", Some("uk"))],
        catalogs: true,
    },
];

/// A config home for `variant`: its color scheme, icon theme and extra lines in `kdeglobals`,
/// with a text cursor that does not blink, like [`theme_home`].
fn variant_home(base: &Path, variant: &Variant) -> Result<PathBuf> {
    let home = base.join(variant.name);
    std::fs::create_dir_all(&home)?;
    let mut globals = match variant.scheme {
        Scheme::Installed(name) => {
            let colors = color_scheme_file(name);
            std::fs::read_to_string(&colors)
                .with_context(|| format!("read {}", colors.display()))?
        }
        Scheme::Bundled(text) => text.to_owned(),
    };
    globals.push_str(&format!(
        "\n[Icons]\nTheme={}\n\n[KDE]\nCursorBlinkRate=0\n\n{}",
        variant.icons, variant.kdeglobals
    ));
    std::fs::write(home.join("kdeglobals"), globals)?;
    Ok(home)
}

/// `cargo xtask shots --matrix [--variant a,b] [--page a,b] [--size wide|narrow] [--jobs N]
/// [--no-build]`: the pages of [`MATRIX_PAGES`], wide and narrow, once per variant of
/// [`VARIANTS`], into `target/shots-matrix/<variant>/`. For looking at the UI where the default
/// pictures cannot show a fault; nothing is compared with a baseline and `ci` does not run it.
/// Every render is checked as in the compared run (clean exit, a picture, no QML warning), but
/// not for light against dark: a variant is one color scheme.
fn run_matrix(args: &Args) -> Result<()> {
    let started = Instant::now();
    let bin = app_binary(args.build)?;
    let variants: Vec<&Variant> = VARIANTS
        .iter()
        .filter(|v| {
            args.variants
                .as_ref()
                .is_none_or(|names| names.iter().any(|n| n == v.name))
        })
        .collect();
    if variants.iter().any(|v| v.catalogs) {
        crate::i18n::build(&locale_dir())?;
    }
    let homes = target_dir().join("shots-env").join("matrix");
    let mut failures = Vec::new();
    let mut summary = Vec::new();
    for variant in variants {
        let dir = matrix_dir().join(variant.name);
        // Only the pictures of this run: an earlier one may have had other pages.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let ctx = Ctx {
            bin: &bin,
            dir: &dir,
            homes: BTreeMap::from([(variant.theme, variant_home(&homes, variant)?)]),
            mode: Mode::Plain,
            variant: Some(variant),
        };
        let theme = (variant.theme, "", "");
        let mut jobs = Vec::new();
        for page in SHOT_PAGES {
            let wanted = match &args.pages {
                Some(pages) => pages.iter().any(|p| p == page),
                None => MATRIX_PAGES.contains(&page),
            };
            for size in SHOT_SIZES {
                if wanted && args.size.as_deref().is_none_or(|s| s == size.0) {
                    jobs.push(Job { page, size, theme });
                }
            }
        }
        eprintln!(
            "shots-matrix: {}: {} pictures, {}",
            variant.name,
            jobs.len(),
            variant.about
        );
        let outcomes = render_all(&ctx, &jobs, args.jobs.min(jobs.len().max(1)));
        let problems: Vec<String> = outcomes
            .iter()
            .flat_map(|o| o.failures.iter().cloned())
            .collect();
        summary.push(format!(
            "{:<14} {} pictures, {} problem(s): {}",
            variant.name,
            jobs.len(),
            problems.len(),
            variant.about
        ));
        failures.extend(problems);
    }
    for f in &failures {
        eprintln!("shots-matrix: {f}");
    }
    eprintln!(
        "shots-matrix: {} ({} s)",
        matrix_dir().display(),
        started.elapsed().as_secs()
    );
    for line in &summary {
        eprintln!("shots-matrix: {line}");
    }
    if !failures.is_empty() {
        bail!("{} screenshot problem(s) in the matrix run", failures.len());
    }
    Ok(())
}

/// `cargo xtask shots [dir] [--page a,b,...] [--theme light|dark] [--size wide|narrow]
/// [--jobs N] [--no-build] [--accept] [--no-compare]`, or the look-at run `--matrix` ([`run_matrix`]).
/// Fails on QML warnings, missing files, a dark rendering that is not darker than the light one
/// (the theme did not apply; see [`theme_problem`] for the pages this cannot be told from)
/// and, in the default directory, on changed pictures.
pub fn shots(args: &[String]) -> Result<()> {
    run_shots(&parse_args(args)?)
}

/// The screenshot step of `cargo xtask ci`: the parallel render, unless nothing that draws
/// changed since the last full compared render.
pub fn ci_step() -> Result<()> {
    match render_digest() {
        Ok(digest) if read_stamp(&default_dir()) == Some(digest) => {
            eprintln!(
                "\n==> screenshots skipped: nothing that draws changed since the last full compared render \
                 (target/shots/.stamp); `cargo xtask shots` renders them anyway"
            );
            return Ok(());
        }
        Ok(_) => {}
        Err(e) => eprintln!("shots: cannot tell whether anything changed ({e:#}); rendering"),
    }
    run_shots(&Args::default())
}

/// The debug binary the renders run, built first unless `build` is false.
fn app_binary(build: bool) -> Result<PathBuf> {
    if build {
        run("build kourier", cargo().args(["build", "-p", "kourier"]))?;
    }
    let bin = target_dir().join("debug/kourier");
    if !bin.is_file() {
        bail!(
            "{} not found{}",
            bin.display(),
            if build {
                ""
            } else {
                " (drop --no-build to build it)"
            }
        );
    }
    Ok(bin)
}

fn run_shots(args: &Args) -> Result<()> {
    if args.matrix {
        return run_matrix(args);
    }
    let started = Instant::now();
    let default = args.dir.is_none();
    let dir = args.dir.clone().unwrap_or_else(default_dir);
    let mode = match (default, args.accept, args.compare) {
        (true, true, _) => Mode::Accept,
        (true, false, true) => Mode::Compare,
        _ => Mode::Plain,
    };
    // A render of everything with the binary these sources build, compared (or accepted):
    // what the next `ci` may skip. The digest is taken before the build, so a source edited
    // during the run makes the stamp stale, not wrong.
    let digest = if mode != Mode::Plain && args.is_full() && args.build {
        match render_digest() {
            Ok(digest) => Some(digest),
            Err(e) => {
                eprintln!("shots: no stamp will be written ({e:#})");
                None
            }
        }
    } else {
        None
    };
    std::fs::create_dir_all(&dir)?;
    if mode != Mode::Plain && args.is_full() {
        // The diff pictures of an earlier run: only this run's changes are left.
        let _ = std::fs::remove_dir_all(dir.join("diff"));
    }
    let bin = app_binary(args.build)?;
    let jobs = select_jobs(args);
    let homes = target_dir().join("shots-env");
    let mut config_homes = BTreeMap::new();
    for (theme, scheme, icons) in SHOT_THEMES {
        if jobs.iter().any(|j| j.theme.0 == theme) {
            config_homes.insert(theme, theme_home(&homes, theme, scheme, icons)?);
        }
    }
    let ctx = Ctx {
        bin: &bin,
        dir: &dir,
        homes: config_homes,
        mode,
        variant: None,
    };
    let workers = args.jobs.min(jobs.len().max(1));
    eprintln!(
        "shots: {} pictures, {workers} at a time{}",
        jobs.len(),
        match mode {
            Mode::Plain => "",
            Mode::Compare => ", compared with target/shots-baseline",
            Mode::Accept => ", accepted as the baseline",
        }
    );
    let outcomes = render_all(&ctx, &jobs, workers);

    let mut failures = Vec::new();
    let mut luma: BTreeMap<(&str, &str), BTreeMap<&str, u32>> = BTreeMap::new();
    let mut changed = Vec::new();
    let (mut new_baselines, mut accepted, mut not_compared) = (0, 0, 0);
    for (job, out) in jobs.iter().zip(&outcomes) {
        failures.extend(out.failures.iter().cloned());
        if let Some(l) = out.luma {
            luma.entry((job.page, job.size.0))
                .or_default()
                .insert(job.theme.0, l);
        }
        match &out.verdict {
            Verdict::Changed(_) => changed.push(job.name()),
            Verdict::NewBaseline => new_baselines += 1,
            Verdict::Accepted => accepted += 1,
            Verdict::NotCompared => not_compared += 1,
            Verdict::Same { .. } => {}
        }
    }
    // Only pages rendered in both color schemes can be told apart.
    for ((page, size), by_theme) in &luma {
        if let (Some(light), Some(dark)) = (by_theme.get("light"), by_theme.get("dark"))
            && let Some(problem) = theme_problem(page, *light, *dark)
        {
            failures.push(format!("{page}-{size}: {problem}"));
        }
    }
    for f in &failures {
        eprintln!("shots: {f}");
    }
    let secs = started.elapsed().as_secs();
    match mode {
        Mode::Plain => eprintln!("shots: {} PNGs in {} ({secs} s)", jobs.len(), dir.display()),
        Mode::Compare => {
            eprintln!(
                "shots: {} pictures, {} changed{}{}",
                jobs.len(),
                changed.len(),
                if changed.is_empty() { "" } else { ": " },
                changed.join(", ")
            );
            eprintln!(
                "shots: {secs} s; {new_baselines} new baselines, {not_compared} not compared"
            );
        }
        Mode::Accept => eprintln!(
            "shots: {} pictures, {accepted} accepted as the baseline in target/shots-baseline ({secs} s)",
            jobs.len()
        ),
    }
    // A compared render that found something no longer vouches for the sources it stamped.
    if mode != Mode::Plain && (!failures.is_empty() || !changed.is_empty()) {
        let _ = std::fs::remove_file(stamp_path(&dir));
    }
    if !failures.is_empty() {
        bail!("{} screenshot problem(s)", failures.len());
    }
    if !changed.is_empty() {
        bail!(
            "{} picture(s) changed: {}. Look at target/shots/diff/<name>.png; if the change is meant, \
             make the new pictures the baseline: cargo xtask shots --accept",
            changed.len(),
            changed.join(", ")
        );
    }
    if let Some(digest) = digest {
        write_stamp(&dir, digest)?;
    }
    Ok(())
}

/// Where the digest of the last full, successful, compared render of `dir` is kept.
fn stamp_path(dir: &Path) -> PathBuf {
    dir.join(".stamp")
}

fn write_stamp(dir: &Path, digest: u64) -> Result<()> {
    let path = stamp_path(dir);
    std::fs::write(&path, format!("{digest:016x}\n"))
        .with_context(|| format!("write {}", path.display()))
}

/// The digest stored in `dir`, if any.
fn read_stamp(dir: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(stamp_path(dir)).ok()?;
    u64::from_str_radix(text.trim(), 16).ok()
}

/// What can change a picture, relative to the workspace root: the crates that draw or feed the
/// UI, the desktop file and icon, the dependency versions and the render code itself.
const DIGEST_ROOTS: [&str; 5] = [
    "crates",
    "packaging",
    "Cargo.lock",
    "xtask/src/main.rs",
    "xtask/src/shots.rs",
];

/// Crates below `crates/` that cannot change a picture: the Signal backend (the render runs on
/// the fake network) and the two test-only crates.
const DIGEST_SKIPPED_CRATES: [&str; 3] = ["kourier-signal", "kourier-e2e", "kourier-coexist"];

/// A hash of every file of [`DIGEST_ROOTS`] (path and content), except `tests/` directories,
/// build output and [`DIGEST_SKIPPED_CRATES`].
fn tree_digest(root: &Path) -> Result<u64> {
    let mut files = Vec::new();
    for rel in DIGEST_ROOTS {
        digest_files(root, Path::new(rel), &mut files)?;
    }
    files.sort();
    let mut hasher = DefaultHasher::new();
    for rel in &files {
        rel.hash(&mut hasher);
        std::fs::read(root.join(rel))
            .with_context(|| format!("read {}", rel.display()))?
            .hash(&mut hasher);
    }
    Ok(hasher.finish())
}

fn digest_files(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let path = root.join(rel);
    if path.is_dir() {
        let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let crate_dir = rel.parent() == Some(Path::new("crates"));
        if name == "tests"
            || name == "target"
            || (crate_dir && DIGEST_SKIPPED_CRATES.contains(&name))
        {
            return Ok(());
        }
        for entry in std::fs::read_dir(&path).with_context(|| format!("read {}", path.display()))? {
            digest_files(root, &rel.join(entry?.file_name()), out)?;
        }
    } else if path.is_file() {
        out.push(rel.to_owned());
    }
    Ok(())
}

/// The digest of everything that can change a picture: [`tree_digest`], Qt's version and the
/// two color scheme files `theme_home` reads.
fn render_digest() -> Result<u64> {
    let qmake = Command::new("/usr/bin/qmake6")
        .args(["-query", "QT_VERSION"])
        .output()
        .context("run /usr/bin/qmake6 -query QT_VERSION")?;
    if !qmake.status.success() {
        bail!("qmake6 -query QT_VERSION failed ({})", qmake.status);
    }
    let mut hasher = DefaultHasher::new();
    tree_digest(&root())?.hash(&mut hasher);
    qmake.stdout.hash(&mut hasher);
    for (_, scheme, _) in SHOT_THEMES {
        let file = color_scheme_file(scheme);
        std::fs::read(&file)
            .with_context(|| format!("read {}", file.display()))?
            .hash(&mut hasher);
    }
    Ok(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args> {
        parse_args(&list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Image {
        Image {
            width,
            height,
            pixels: vec![rgba; width as usize * height as usize],
        }
    }

    /// `image` with its first `n` pixels set to `rgba`.
    fn with_first(mut image: Image, n: usize, rgba: [u8; 4]) -> Image {
        for pixel in image.pixels.iter_mut().take(n) {
            *pixel = rgba;
        }
        image
    }

    #[test]
    fn identical_pictures_are_not_a_change() {
        let a = solid(40, 25, [10, 20, 30, 255]);
        let diff = compare(&a, &solid(40, 25, [10, 20, 30, 255]));
        assert_eq!(
            diff,
            Diff::Pixels {
                differing: 0,
                total: 1000
            }
        );
        assert!(!diff.is_change("chat"));
    }

    #[test]
    fn a_few_differing_pixels_are_below_the_limit() {
        // The limit is 24 pixels, whatever the size, and 24 itself is not above it.
        let base = solid(1000, 1000, [200, 200, 200, 255]);
        for (n, change) in [
            (1, false),
            (24, false),
            (25, true),
            (501, true),
            (5000, true),
        ] {
            let render = with_first(solid(1000, 1000, [200, 200, 200, 255]), n, [0, 0, 0, 255]);
            let diff = compare(&base, &render);
            assert_eq!(
                diff,
                Diff::Pixels {
                    differing: n,
                    total: 1_000_000
                }
            );
            assert_eq!(diff.is_change("chat"), change, "{n} differing pixels");
            // A page with pictures: one pixel in 667 of the 1,000,000.
            assert_eq!(
                diff.is_change("album"),
                n > 1499,
                "{n} differing pixels, album"
            );
        }
    }

    #[test]
    fn a_pixel_differs_above_24_in_any_channel() {
        let base = solid(10, 10, [100, 100, 100, 255]);
        let within = solid(10, 10, [124, 76, 100, 255]);
        assert_eq!(
            compare(&base, &within),
            Diff::Pixels {
                differing: 0,
                total: 100
            }
        );
        for rgba in [
            [125, 100, 100, 255],
            [100, 75, 100, 255],
            [100, 100, 125, 255],
            [100, 100, 100, 230],
        ] {
            assert_eq!(
                compare(&base, &solid(10, 10, rgba)),
                Diff::Pixels {
                    differing: 100,
                    total: 100
                },
                "{rgba:?}"
            );
        }
    }

    #[test]
    fn another_size_is_a_change() {
        let diff = compare(
            &solid(1200, 800, [1, 2, 3, 255]),
            &solid(1200, 640, [1, 2, 3, 255]),
        );
        assert!(diff.is_change("chat") && diff.is_change("album"));
        assert_eq!(diff.describe(), "size 1200x800 -> 1200x640");
    }

    #[test]
    fn the_diff_picture_shows_both_pictures_and_the_difference() {
        let base = solid(30, 20, [200, 200, 200, 255]);
        let render = with_first(solid(30, 20, [200, 200, 200, 255]), 1, [0, 0, 0, 255]);
        let diff = diff_image(&base, &render);
        assert_eq!((diff.width, diff.height), (3 * 30 + 2 * PANEL_GAP, 20));
        let at = |x: u32, y: u32| diff.pixels[(y * diff.width + x) as usize];
        // Baseline, render, then the marked panel, with a gap between them.
        assert_eq!(at(0, 5), [200, 200, 200, 255]);
        assert_eq!(at(30 + PANEL_GAP, 0), [0, 0, 0, 255]);
        assert_eq!(at(30, 5), [128, 128, 128, 255]);
        let marked = 2 * (30 + PANEL_GAP);
        assert_eq!(at(marked, 0), [255, 0, 255, 255]);
        assert_eq!(at(marked + 1, 1), [255, 0, 255, 255]);
        assert_eq!(
            at(marked + 5, 5),
            [200 / 3 + 170, 200 / 3 + 170, 200 / 3 + 170, 255]
        );
        // Pictures of another size sit side by side too.
        let other = diff_image(&base, &solid(30, 30, [0, 0, 0, 255]));
        assert_eq!((other.width, other.height), (3 * 30 + 2 * PANEL_GAP, 30));
    }

    #[test]
    fn pictures_survive_a_trip_through_a_png_file() {
        let dir = TempTree::new("png");
        let image = with_first(solid(17, 9, [1, 2, 3, 255]), 20, [250, 128, 7, 200]);
        let path = dir.0.join("a.png");
        write_png(&path, &image).unwrap();
        let back = read_png(&path).unwrap();
        assert_eq!((back.width, back.height), (17, 9));
        assert_eq!(back.pixels, image.pixels);
    }

    #[test]
    fn the_command_line_without_arguments_renders_everything_compared() {
        let a = args(&[]).unwrap();
        assert_eq!(a, Args::default());
        assert_eq!(a.jobs, 4);
        assert!(a.build && a.compare && !a.accept && a.is_full());
        assert_eq!(select_jobs(&a).len(), SHOT_PAGES.len() * 4);
    }

    #[test]
    fn the_command_line_takes_the_flags_with_or_without_equal_signs() {
        let a = args(&[
            "--page",
            "chat,edit",
            "--page=reply",
            "--theme",
            "dark",
            "--size=narrow",
            "--jobs",
            "2",
            "--no-build",
            "--no-compare",
        ])
        .unwrap();
        assert_eq!(
            a.pages.as_deref(),
            Some(&["chat".into(), "edit".into(), "reply".into()][..])
        );
        assert_eq!(a.theme.as_deref(), Some("dark"));
        assert_eq!(a.size.as_deref(), Some("narrow"));
        assert_eq!(a.jobs, 2);
        assert!(!a.build && !a.compare && !a.accept && !a.is_full());
        assert!(
            args(&["out/dir", "--accept"])
                .unwrap_err()
                .to_string()
                .contains("--accept")
        );
        let a = args(&["--accept", "--page=chat"]).unwrap();
        assert!(a.accept && a.dir.is_none());
        let a = args(&["some/dir"]).unwrap();
        assert_eq!(a.dir, Some(PathBuf::from("some/dir")));
    }

    #[test]
    fn the_command_line_refuses_what_it_does_not_know() {
        for bad in [
            &["--page", "nonesuch"][..],
            &["--page"],
            &["--page", ","],
            &["--theme", "sepia"],
            &["--size", "huge"],
            &["--jobs", "0"],
            &["--jobs", "many"],
            &["--bogus"],
            &["--accept=yes"],
            &["a", "b"],
        ] {
            assert!(args(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_filter_renders_only_those_pictures_in_a_stable_order() {
        let all = select_jobs(&Args::default());
        assert_eq!(all.len(), SHOT_PAGES.len() * 4);
        assert_eq!(all[0].name(), format!("{}-wide-light", SHOT_PAGES[0]));
        assert_eq!(all[1].name(), format!("{}-narrow-light", SHOT_PAGES[0]));
        let names = |a: &Args| select_jobs(a).iter().map(Job::name).collect::<Vec<_>>();
        // Pages given twice or out of order render once, in the order of the matrix.
        let a = args(&["--page", "reply,chat,chat"]).unwrap();
        assert_eq!(
            names(&a),
            [
                "chat-wide-light",
                "chat-narrow-light",
                "reply-wide-light",
                "reply-narrow-light",
                "chat-wide-dark",
                "chat-narrow-dark",
                "reply-wide-dark",
                "reply-narrow-dark",
            ]
        );
        let a = args(&["--page", "edit", "--theme", "dark", "--size", "wide"]).unwrap();
        assert_eq!(names(&a), ["edit-wide-dark"]);
    }

    #[test]
    fn the_pages_without_a_stable_picture_are_pages_of_the_matrix() {
        for page in SHOT_PAGES_NOT_COMPARED {
            assert!(SHOT_PAGES.contains(&page), "{page}");
        }
        assert!(SHOT_PAGES_NOT_COMPARED.contains(&"qr"));
    }

    #[test]
    fn the_matrix_command_line_takes_its_flags_and_refuses_what_it_does_not_use() {
        let a = args(&[
            "--matrix",
            "--variant",
            "rtl,font-14",
            "--size=wide",
            "--no-build",
        ])
        .unwrap();
        assert!(a.matrix && !a.build && !a.accept);
        assert_eq!(
            a.variants.as_deref(),
            Some(&["rtl".into(), "font-14".into()][..])
        );
        assert!(!args(&[]).unwrap().matrix);
        for bad in [
            &["--matrix", "--accept"][..],
            &["--matrix", "--theme", "dark"],
            &["--matrix", "some/dir"],
            &["--variant", "rtl"],
            &["--matrix", "--variant", "nonesuch"],
            &["--matrix", "--variant", ","],
        ] {
            assert!(args(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_matrix_variants_are_distinct_and_their_pictures_are_pages_of_the_matrix() {
        for page in MATRIX_PAGES {
            assert!(SHOT_PAGES.contains(&page), "{page}");
        }
        let mut names: Vec<_> = VARIANTS.iter().map(|v| v.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), VARIANTS.len(), "variant names are unique");
        for v in &VARIANTS {
            assert!(["light", "dark"].contains(&v.theme), "{}", v.name);
            assert!(!v.about.is_empty(), "{}", v.name);
            if let Scheme::Installed(name) = v.scheme {
                assert!(color_scheme_file(name).is_file(), "{name} is not installed");
            }
        }
        // The five looks the task asks for.
        for name in ["classic", "hc-light", "hc-dark", "font-14", "rtl", "no-kde"] {
            assert!(VARIANTS.iter().any(|v| v.name == name), "{name}");
        }
    }

    /// The color of `role` in `group` of a `.colors` file as `[r, g, b]`.
    fn scheme_color(text: &str, group: &str, role: &str) -> [u8; 3] {
        let body = text
            .split(&format!("[{group}]\n"))
            .nth(1)
            .unwrap_or_else(|| panic!("group {group}"))
            .split("\n\n")
            .next()
            .unwrap();
        let value = body
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{role}=")))
            .unwrap_or_else(|| panic!("{group}/{role}"));
        let parts: Vec<u8> = value.split(',').map(|n| n.parse().unwrap()).collect();
        [parts[0], parts[1], parts[2]]
    }

    #[test]
    fn the_high_contrast_schemes_are_pure_and_have_every_group_the_theme_reads() {
        let light = include_str!("../assets/HighContrastLight.colors");
        let dark = include_str!("../assets/HighContrastDark.colors");
        for (text, background, foreground) in [(light, [255; 3], [0; 3]), (dark, [0; 3], [255; 3])]
        {
            for group in [
                "Colors:View",
                "Colors:Window",
                "Colors:Button",
                "Colors:Header",
                "Colors:Tooltip",
                "Colors:Selection",
                "Colors:Complementary",
                "ColorEffects:Disabled",
                "ColorEffects:Inactive",
                "General",
                "KDE",
                "WM",
            ] {
                assert!(text.contains(&format!("[{group}]\n")), "{group}");
            }
            for group in ["Colors:View", "Colors:Window", "Colors:Button"] {
                assert_eq!(scheme_color(text, group, "BackgroundNormal"), background);
                assert_eq!(scheme_color(text, group, "ForegroundNormal"), foreground);
                // The quiet text of a high-contrast scheme is not quiet.
                assert_eq!(scheme_color(text, group, "ForegroundInactive"), foreground);
            }
            // Kirigami's frames take their strength from this key of the `[KDE]` group.
            assert!(text.contains("\nframeContrast=1\n"));
        }
        // The selection is a saturated color with its own readable text.
        assert_eq!(
            scheme_color(light, "Colors:Selection", "BackgroundNormal"),
            [0, 0, 255]
        );
        assert_eq!(
            scheme_color(dark, "Colors:Selection", "BackgroundNormal"),
            [255, 255, 0]
        );
    }

    #[test]
    fn the_font_variant_names_a_font_the_theme_can_parse() {
        // family, point size, pixel size, style hint, weight, then the flags Plasma writes.
        for line in FONT_14_PT.lines().filter(|l| l.contains('=')) {
            let (key, value) = line.split_once('=').unwrap();
            assert!(["font", "smallestReadableFont"].contains(&key), "{key}");
            let fields: Vec<_> = value.split(',').collect();
            assert_eq!(fields.len(), 16, "{line}");
            assert!(fields[1].parse::<u32>().unwrap() >= 11, "{line}");
        }
        assert!(FONT_14_PT.starts_with("[General]\n"));
        assert!(FONT_14_PT.contains("font=Noto Sans,14,"));
    }

    /// A directory below `target/` for a test, removed again.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let dir = target_dir()
                .join("xtask-test")
                .join(format!("{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        fn digest(&self) -> u64 {
            tree_digest(&self.0).unwrap()
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A tree with a file in each place the digest reads or skips.
    fn digest_tree(name: &str) -> TempTree {
        let tree = TempTree::new(name);
        tree.write("crates/kourier-qt/qml/Main.qml", "Item {}");
        tree.write("crates/kourier-qt/src/lib.rs", "// ui");
        tree.write("crates/kourier-qt/tests/ui.rs", "// test");
        tree.write("crates/kourier-signal/src/lib.rs", "// signal");
        tree.write("crates/kourier-signal/tests/mock.rs", "// test");
        tree.write("crates/kourier-e2e/src/lib.rs", "// e2e");
        tree.write("crates/kourier-coexist/src/lib.rs", "// coexist");
        tree.write("crates/kourier-core/tests/db.rs", "// test");
        tree.write("crates/kourier-core/target/junk", "junk");
        tree.write("packaging/kourier.desktop", "[Desktop Entry]");
        tree.write("Cargo.lock", "# lock");
        tree.write("xtask/src/main.rs", "// xtask");
        tree.write("xtask/src/shots.rs", "// shots");
        tree.write("xtask/src/smoke.rs", "// smoke");
        tree.write("docs/architecture.md", "# docs");
        tree
    }

    #[test]
    fn the_digest_follows_what_draws() {
        let tree = digest_tree("digest-draws");
        let base = tree.digest();
        assert_eq!(base, tree.digest(), "the same tree, the same digest");
        for (rel, content) in [
            ("crates/kourier-qt/qml/Main.qml", "Item { x: 1 }"),
            ("crates/kourier-qt/src/lib.rs", "// ui 2"),
            ("crates/kourier-qt/src/new.rs", "// a new file"),
            ("crates/kourier-core/src/lib.rs", "// a new crate file"),
            ("packaging/kourier.desktop", "[Desktop Entry]\nName=K"),
            ("Cargo.lock", "# lock 2"),
            ("xtask/src/main.rs", "// xtask 2"),
            ("xtask/src/shots.rs", "// shots 2"),
        ] {
            let tree = digest_tree("digest-draws-each");
            tree.write(rel, content);
            assert_ne!(tree.digest(), base, "{rel}");
        }
    }

    #[test]
    fn the_stamp_keeps_the_digest_it_was_given() {
        let dir = TempTree::new("stamp");
        assert_eq!(read_stamp(&dir.0), None);
        write_stamp(&dir.0, 0xdead_beef_0123_4567).unwrap();
        assert_eq!(read_stamp(&dir.0), Some(0xdead_beef_0123_4567));
        write_stamp(&dir.0, 7).unwrap();
        assert_eq!(read_stamp(&dir.0), Some(7));
        dir.write(".stamp", "not a number");
        assert_eq!(read_stamp(&dir.0), None);
    }

    #[test]
    fn the_digest_ignores_what_cannot_draw() {
        let tree = digest_tree("digest-ignores");
        let base = tree.digest();
        for (rel, content) in [
            ("crates/kourier-signal/src/lib.rs", "// signal 2"),
            ("crates/kourier-signal/src/new.rs", "// a new file"),
            ("crates/kourier-signal/tests/mock.rs", "// test 2"),
            ("crates/kourier-e2e/src/lib.rs", "// e2e 2"),
            ("crates/kourier-coexist/src/lib.rs", "// coexist 2"),
            ("crates/kourier-qt/tests/ui.rs", "// test 2"),
            ("crates/kourier-qt/tests/new/more.rs", "// a new test"),
            ("crates/kourier-core/tests/db.rs", "// test 2"),
            ("crates/kourier-core/target/junk", "more junk"),
            ("xtask/src/smoke.rs", "// smoke 2"),
            ("docs/architecture.md", "# docs 2"),
        ] {
            tree.write(rel, content);
            assert_eq!(tree.digest(), base, "{rel}");
        }
    }
}
