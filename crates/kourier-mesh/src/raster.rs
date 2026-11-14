//! The still picture: orthographic, azimuth 35 degrees, elevation 25 degrees, Z up, fit to 92 %,
//! 2x2 supersampling with a premultiplied downsample, transparent background, flat shading.
//! One thread, deterministic.

use crate::{MAX_RASTER_WORK, MAX_STILL_EDGE, MIN_STILL_EDGE, Mesh};

/// How the picture is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StillMode {
    /// The triangle's own colour, else `fallback` (`0xRRGGBBAA`, alpha ignored: pictures are opaque).
    Colour { fallback: u32 },
    /// Grey = light factor, alpha = coverage: the image provider multiplies it by a tint.
    Shade,
}

/// A straight-alpha RGBA picture, `width * height * 4` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Still {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

type V3 = [f64; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn norm(a: V3) -> V3 {
    let l = dot(a, a).sqrt();
    if l > 0.0 && l.is_finite() {
        [a[0] / l, a[1] / l, a[2] / l]
    } else {
        [0.0, 0.0, 1.0]
    }
}
fn corner(t: &[f32], k: usize) -> V3 {
    [t[3 * k] as f64, t[3 * k + 1] as f64, t[3 * k + 2] as f64]
}

const SS: usize = 2;

/// Renders `mesh` into a square picture of `edge_px` (clamped to `MIN_STILL_EDGE..=MAX_STILL_EDGE`).
pub fn render_still(mesh: &Mesh, edge_px: u32, mode: StillMode) -> Still {
    let size = edge_px.clamp(MIN_STILL_EDGE, MAX_STILL_EDGE) as usize;
    let w = size * SS;
    let (az, el) = (35f64.to_radians(), 25f64.to_radians());
    let d = [az.sin() * el.cos(), -az.cos() * el.cos(), el.sin()];
    let nd = [-d[0], -d[1], -d[2]];
    let r = norm(cross(nd, [0.0, 0.0, 1.0]));
    let u = cross(r, nd);
    let light = norm([
        d[0] + 0.4 * u[0] - 0.3 * r[0],
        d[1] + 0.4 * u[1] - 0.3 * r[1],
        d[2] + 0.4 * u[2] - 0.3 * r[2],
    ]);
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for p in mesh.positions.as_chunks::<3>().0.iter() {
        let p = [p[0] as f64, p[1] as f64, p[2] as f64];
        let (px, py) = (dot(p, r), dot(p, u));
        x0 = x0.min(px);
        x1 = x1.max(px);
        y0 = y0.min(py);
        y1 = y1.max(py);
    }
    let mut rgba = vec![0u32; w * w];
    if x0 <= x1 {
        let span = (x1 - x0).max(y1 - y0).max(1e-9);
        let scale = w as f64 * 0.92 / span;
        let (cx, cy) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
        let mut zb = vec![f32::NEG_INFINITY; w * w];
        let mut work = 0u64;
        for (i, t) in mesh.positions.as_chunks::<9>().0.iter().enumerate() {
            let (mut x, mut y, mut z) = ([0f32; 3], [0f32; 3], [0f32; 3]);
            for k in 0..3 {
                let p = corner(t, k);
                x[k] = (w as f64 * 0.5 + (dot(p, r) - cx) * scale) as f32;
                y[k] = (w as f64 * 0.5 - (dot(p, u) - cy) * scale) as f32;
                z[k] = dot(p, d) as f32;
            }
            let a = corner(t, 0);
            let n = norm(cross(sub(corner(t, 1), a), sub(corner(t, 2), a)));
            let diff = dot(n, light).abs();
            let sky = 0.5 + 0.5 * n[2].abs();
            let kk = 0.38 + 0.12 * sky + 0.50 * diff;
            let colour = match mode {
                StillMode::Shade => {
                    let g = ((255.0 * kk).min(255.0)) as u32;
                    (g << 24) | (g << 16) | (g << 8) | 0xFF
                }
                StillMode::Colour { fallback } => {
                    let own = mesh
                        .colors
                        .as_ref()
                        .and_then(|c| c.get(i).copied())
                        .filter(|&c| c != 0)
                        .unwrap_or(fallback);
                    let ch = |sh: u32| (((own >> sh) & 0xFF) as f64 * kk).min(255.0) as u32;
                    (ch(24) << 24) | (ch(16) << 16) | (ch(8) << 8) | 0xFF
                }
            };
            work += raster(&x, &y, &z, colour, &mut rgba, &mut zb, w);
            if work > MAX_RASTER_WORK {
                break;
            }
        }
    }
    // downsample, premultiplied
    let mut out = vec![0u8; size * size * 4];
    for oy in 0..size {
        for ox in 0..size {
            let (mut r_, mut g_, mut b_, mut a_) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let c = rgba[(oy * SS + sy) * w + ox * SS + sx];
                    let a = c & 0xFF;
                    r_ += (c >> 24) * a;
                    g_ += ((c >> 16) & 0xFF) * a;
                    b_ += ((c >> 8) & 0xFF) * a;
                    a_ += a;
                }
            }
            let o = (oy * size + ox) * 4;
            // With no coverage every sum is 0 and the pixel stays transparent.
            let d = a_.max(1);
            out[o] = (r_ / d) as u8;
            out[o + 1] = (g_ / d) as u8;
            out[o + 2] = (b_ / d) as u8;
            out[o + 3] = (a_ / (SS * SS) as u32) as u8;
        }
    }
    Still {
        rgba: out,
        width: size as u32,
        height: size as u32,
    }
}

/// Fills one triangle; returns the bounding-box pixels visited (the work budget).
fn raster(
    xs: &[f32; 3],
    ys: &[f32; 3],
    zs: &[f32; 3],
    colour: u32,
    rgba: &mut [u32],
    zb: &mut [f32],
    w: usize,
) -> u64 {
    if xs.iter().chain(ys).chain(zs).any(|v| !v.is_finite()) {
        return 0;
    }
    let (mut x, mut y, mut z) = (*xs, *ys, *zs);
    let mut area = (x[1] - x[0]) * (y[2] - y[0]) - (x[2] - x[0]) * (y[1] - y[0]);
    if area.abs() < 1e-9 {
        return 0;
    }
    if area < 0.0 {
        x.swap(1, 2);
        y.swap(1, 2);
        z.swap(1, 2);
        area = -area;
    }
    let minx = x[0].min(x[1]).min(x[2]).floor().max(0.0) as i64;
    let maxx = (x[0].max(x[1]).max(x[2]).ceil() as i64).min(w as i64 - 1);
    let miny = (y[0].min(y[1]).min(y[2]).floor().max(0.0)) as i64;
    let maxy = (y[0].max(y[1]).max(y[2]).ceil() as i64).min(w as i64 - 1);
    if minx > maxx || miny > maxy {
        return 0;
    }
    let inv = 1.0 / area;
    let e = |ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32| {
        (bx - ax) * (py - ay) - (by - ay) * (px - ax)
    };
    for py in miny..=maxy {
        let fy = py as f32 + 0.5;
        let row = py as usize * w;
        for px in minx..=maxx {
            let fx = px as f32 + 0.5;
            let w0 = e(x[1], y[1], x[2], y[2], fx, fy);
            let w1 = e(x[2], y[2], x[0], y[0], fx, fy);
            let w2 = e(x[0], y[0], x[1], y[1], fx, fy);
            if w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0 {
                let zz = (w0 * z[0] + w1 * z[1] + w2 * z[2]) * inv;
                let i = row + px as usize;
                if zz > zb[i] {
                    zb[i] = zz;
                    rgba[i] = colour;
                }
            }
        }
    }
    ((maxx - minx + 1) * (maxy - miny + 1)) as u64
}

/// Encodes a still as a PNG (8-bit RGBA). Empty only if the picture is not `width * height * 4`.
pub fn encode_png(still: &Still) -> Vec<u8> {
    let mut v = Vec::new();
    let mut e = png::Encoder::new(&mut v, still.width, still.height);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let ok = e
        .write_header()
        .and_then(|mut w| w.write_image_data(&still.rgba))
        .is_ok();
    if ok { v } else { Vec::new() }
}
