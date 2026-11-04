//! Justified rows for an album of pictures and videos (architecture §8.3, "shown whole").
//!
//! The user's words: "do not crop images/video in previews, show full image fit into maximal
//! size for it". An album therefore has no fixed grid of cells: every picture keeps its own
//! shape, the pictures of a row share one height and their widths follow their shapes, and the
//! rows fill the album's width. This module chooses the rows, once per message when the row's
//! blocks are built; the pixel step (the album's width is only known to the layout) is a few
//! lines of QML in `AlbumBlock.qml` over what [`AlbumRow`] holds. No Qt, no clock, no hash
//! order: the same shapes always give the same rows.
//!
//! All sizes below are in units of the album's width (gaps ignored for the choice).

/// A tile is at most 2.5 times as tall as wide; a narrower picture sits whole and centred in a
/// tile of this shape, on the bubble's own colour.
pub const ASPECT_MIN: f64 = 0.4;
/// A tile is at most 5 times as wide as tall (about the width limit over the minimum height);
/// a wider picture sits whole and centred in a tile of this shape.
pub const ASPECT_MAX: f64 = 5.0;
/// The aspect of a picture whose size is not known yet (a 4:3 box, as the single picture's).
pub const ASPECT_UNKNOWN: f64 = 4.0 / 3.0;
/// The height a row would ideally have.
pub const ROW_IDEAL: f64 = 0.45;
/// The height the album would ideally have.
pub const TOTAL_IDEAL: f64 = 0.75;
/// The album is at most this tall: the single picture's height limit (1.5 times the width).
pub const TOTAL_MAX: f64 = 1.5;
/// When no cut keeps both [`TOTAL_MAX`] and [`TILE_MIN`] (a mix of very different shapes), the
/// best cut within this height is taken instead (the one whose smallest tile is the largest).
pub const TOTAL_HARD: f64 = 2.0;
/// The smallest side a tile should have: the round badge (2.6 grid units) fits whole in a
/// tile at the width limit (16 grid units). A cut with a smaller tile is the fallback's.
pub const TILE_MIN: f64 = 2.6 / 16.0;

/// Two costs closer than this are a tie.
const EPS: f64 = 1e-9;

/// The box aspect (width over height) of one tile: the picture's own shape, clamped to
/// [`ASPECT_MIN`]..[`ASPECT_MAX`]; an unknown size (either side 0) is [`ASPECT_UNKNOWN`].
pub fn tile_aspect(width: u32, height: u32) -> f64 {
    if width == 0 || height == 0 {
        return ASPECT_UNKNOWN;
    }
    (f64::from(width) / f64::from(height)).clamp(ASPECT_MIN, ASPECT_MAX)
}

/// One row of an album: consecutive tiles of one height that fill the width.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AlbumRow {
    /// Index of the row's first tile in the album's shown tiles.
    pub first: usize,
    /// How many tiles the row has.
    pub count: usize,
    /// The sum of the tiles' aspects: the row is `1 / aspect` of the width tall.
    pub aspect: f64,
    /// For each tile of the row, the share (0 … 1) of the row's width at which it begins.
    pub prefix: Vec<f64>,
}

impl AlbumRow {
    /// The row's height in units of the album's width.
    pub fn height(&self) -> f64 {
        1.0 / self.aspect
    }
}

/// The rows of an album whose shown tiles have `aspects` (at most [`crate::timeline::ALBUM_TILES`]
/// of them: the fewer the tiles, the fewer the cuts, 2^(n-1) for n tiles).
///
/// The rows are consecutive runs of the tiles in message order (so the order of the tiles is the
/// viewer's and the reading order). Every cut into rows is tried; the cut with the lowest cost
/// wins among those that keep the album at most [`TOTAL_MAX`] tall and every tile at least
/// [`TILE_MIN`] on its short side: the cost is the sum over the rows of `ln(height / ROW_IDEAL)`
/// squared plus `ln(total / TOTAL_IDEAL)` squared. A tie goes to fewer rows, then to the
/// lexicographically smaller list of row lengths ((2, 3) before (3, 2)). When no cut keeps both
/// limits the cut within [`TOTAL_HARD`] whose smallest tile is the largest wins (ties as above);
/// the cut into one row is at most 1.25 tall, so there is always one.
pub fn album_rows(aspects: &[f64]) -> Vec<AlbumRow> {
    let n = aspects.len();
    if n == 0 {
        return Vec::new();
    }
    let mut best: Option<(Score, Vec<usize>)> = None;
    for lens in cuts(n) {
        let score = score(aspects, &lens);
        if score.total > TOTAL_HARD + EPS {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(held, held_lens)| better(&score, &lens, held, held_lens))
        {
            best = Some((score, lens));
        }
    }
    // The cut into one row is within every bound, so there is a best.
    let lens = best.map_or_else(|| vec![n], |(_, lens)| lens);
    build_rows(aspects, &lens)
}

/// What a cut is worth.
struct Score {
    /// The album's height.
    total: f64,
    /// The smallest side of any tile.
    smallest: f64,
    /// The cost of the first kind of cut.
    cost: f64,
    /// It keeps [`TOTAL_MAX`] and [`TILE_MIN`].
    ideal: bool,
}

fn score(aspects: &[f64], lens: &[usize]) -> Score {
    let mut first = 0;
    let mut total = 0.0;
    let mut smallest = f64::MAX;
    let mut cost = 0.0;
    for &count in lens {
        let row = &aspects[first..first + count];
        let height = 1.0 / row.iter().sum::<f64>();
        total += height;
        cost += (height / ROW_IDEAL).ln().powi(2);
        let narrowest = row.iter().copied().fold(f64::MAX, f64::min);
        smallest = smallest.min(height).min(narrowest * height);
        first += count;
    }
    cost += (total / TOTAL_IDEAL).ln().powi(2);
    Score {
        total,
        smallest,
        cost,
        ideal: total <= TOTAL_MAX + EPS && smallest >= TILE_MIN - EPS,
    }
}

/// Whether `a` (cut `a_lens`) beats the held `b` (cut `b_lens`).
fn better(a: &Score, a_lens: &[usize], b: &Score, b_lens: &[usize]) -> bool {
    if a.ideal != b.ideal {
        return a.ideal;
    }
    // Lower is better in both: the cost, or minus the smallest tile.
    let (x, y) = if a.ideal {
        (a.cost, b.cost)
    } else {
        (-a.smallest, -b.smallest)
    };
    if (x - y).abs() > EPS {
        return x < y;
    }
    if a_lens.len() != b_lens.len() {
        return a_lens.len() < b_lens.len();
    }
    a_lens < b_lens
}

/// Every way to cut `n` tiles into consecutive rows, as row lengths, in a fixed order.
fn cuts(n: usize) -> impl Iterator<Item = Vec<usize>> {
    (0..1_usize << (n - 1)).map(move |mask| {
        let mut lens = Vec::new();
        let mut run = 1;
        for gap in 0..n - 1 {
            if mask >> gap & 1 == 1 {
                lens.push(run);
                run = 1;
            } else {
                run += 1;
            }
        }
        lens.push(run);
        lens
    })
}

fn build_rows(aspects: &[f64], lens: &[usize]) -> Vec<AlbumRow> {
    let mut first = 0;
    lens.iter()
        .map(|&count| {
            let row = &aspects[first..first + count];
            let sum: f64 = row.iter().sum();
            let mut begin = 0.0;
            let prefix = row
                .iter()
                .map(|a| {
                    let at = begin / sum;
                    begin += a;
                    at
                })
                .collect();
            let built = AlbumRow {
                first,
                count,
                aspect: sum,
                prefix,
            };
            first += count;
            built
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOT: (u32, u32) = (1080, 2340);

    fn lens(rows: &[AlbumRow]) -> Vec<usize> {
        rows.iter().map(|r| r.count).collect()
    }

    fn rows_of(shapes: &[(u32, u32)]) -> Vec<usize> {
        let aspects: Vec<f64> = shapes.iter().map(|&(w, h)| tile_aspect(w, h)).collect();
        lens(&album_rows(&aspects))
    }

    fn all(n: usize, shape: (u32, u32)) -> Vec<(u32, u32)> {
        vec![shape; n]
    }

    #[test]
    fn unknown_sizes_are_four_to_three_and_the_ends_are_clamped() {
        assert_eq!(tile_aspect(0, 0), ASPECT_UNKNOWN);
        assert_eq!(tile_aspect(800, 0), ASPECT_UNKNOWN);
        assert_eq!(tile_aspect(0, 600), ASPECT_UNKNOWN);
        assert_eq!(tile_aspect(1, 1), 1.0);
        assert_eq!(tile_aspect(1600, 900), 1600.0 / 900.0);
        // A strip (300 x 6000) and a panorama (6000 x 300) are boxes of the clamped shape.
        assert_eq!(tile_aspect(300, 6000), ASPECT_MIN);
        assert_eq!(tile_aspect(6000, 300), ASPECT_MAX);
        assert_eq!(tile_aspect(1080, 2340), 1080.0 / 2340.0);
        assert_eq!(tile_aspect(u32::MAX, 1), ASPECT_MAX);
        assert_eq!(tile_aspect(1, u32::MAX), ASPECT_MIN);
    }

    /// The table of the design (brief 2.2), row by row. Mutating a constant or the tie-break
    /// fails a row of it.
    #[test]
    fn the_rows_of_the_design_table() {
        let l43 = (4, 3);
        let l34 = (3, 4);
        let sq = (1, 1);
        let wide = (16, 9);
        let t25 = (5, 2);
        let pano = (8, 1);
        let t = |shapes: &[(u32, u32)]| rows_of(shapes);
        assert_eq!(t(&all(2, l43)), [2], "2 x 4:3");
        assert_eq!(t(&all(2, l34)), [2], "2 x 3:4");
        assert_eq!(t(&all(2, SHOT)), [2], "2 phone screenshots");
        assert_eq!(t(&all(2, t25)), [1, 1], "2 x 2.5:1");
        assert_eq!(t(&all(3, l43)), [1, 2], "3 x 4:3");
        assert_eq!(t(&all(3, sq)), [3], "3 x 1:1");
        assert_eq!(t(&all(3, l34)), [3], "3 x 3:4");
        assert_eq!(t(&all(3, wide)), [1, 2], "3 x 16:9");
        for shape in [sq, l43, l34] {
            assert_eq!(t(&all(4, shape)), [2, 2], "4 x {shape:?}");
            assert_eq!(t(&all(5, shape)), [2, 3], "5 x {shape:?}");
        }
        assert_eq!(t(&all(5, wide)), [1, 2, 2], "5 x 16:9");
        assert_eq!(t(&all(5, SHOT)), [5], "5 phone screenshots");
        assert_eq!(t(&[pano, sq, sq]), [1, 2], "panorama and 2 squares");
        // The shapes of a mix keep the album within the bounds where they can.
        assert_eq!(t(&[SHOT, wide, sq]), [2, 1], "screenshot, 16:9, square");
        assert_eq!(t(&[(1, 20), sq, sq]), [3], "strip and 2 squares");
        assert_eq!(t(&all(2, pano)), [1, 1], "2 panoramas");
        assert_eq!(t(&all(3, pano)), [1, 1, 1], "3 panoramas");
    }

    /// The extreme case: no cut keeps both the height and the smallest tile, so the cut within
    /// the hard bound whose smallest tile is the largest wins: one row, both whole, the
    /// screenshot small (the viewer shows it big).
    #[test]
    fn a_panorama_beside_a_screenshot_is_one_low_row() {
        let aspects = [tile_aspect(8000, 1000), tile_aspect(SHOT.0, SHOT.1)];
        let rows = album_rows(&aspects);
        assert_eq!(lens(&rows), [2]);
        let total: f64 = rows.iter().map(AlbumRow::height).sum();
        assert!(total < 0.2, "{total}");
        // Both stacked would be 2.37 tall: past the hard bound.
        let stacked = 1.0 / aspects[0] + 1.0 / aspects[1];
        assert!(stacked > TOTAL_HARD, "{stacked}");
    }

    #[test]
    fn a_tie_goes_to_fewer_rows_then_to_the_smaller_list_of_lengths() {
        // Four equal tiles: (2, 2) beats (1, 3), (3, 1) and the single rows by cost; (3, 2)
        // and (2, 3) of five equal tiles cost the same, and the smaller list wins.
        assert_eq!(rows_of(&all(5, (1, 1))), [2, 3]);
        let rows = album_rows(&[1.0; 5]);
        assert_eq!(lens(&rows), [2, 3]);
        // Mirror-image shapes cost the same cut the other way round: the smaller list stays.
        assert_eq!(lens(&album_rows(&[1.0, 1.0, 1.0, 1.0])), [2, 2]);
    }

    /// Costs closer than the tolerance are a tie (a real tie of two cuts of different row
    /// counts hardly occurs in shapes, so the rule is pinned on the comparison itself).
    #[test]
    fn equal_costs_are_a_tie_that_fewer_rows_then_the_smaller_list_wins() {
        let s = |cost: f64, ideal: bool| Score {
            total: 1.0,
            smallest: if ideal { 0.3 } else { 0.1 },
            cost,
            ideal,
        };
        assert!(better(&s(1.0, true), &[5], &s(1.0 + 1e-10, true), &[2, 3]));
        assert!(!better(&s(1.0 + 1e-10, true), &[2, 3], &s(1.0, true), &[5]));
        assert!(better(&s(1.0, true), &[2, 3], &s(1.0, true), &[3, 2]));
        assert!(!better(&s(1.0, true), &[3, 2], &s(1.0, true), &[2, 3]));
        assert!(!better(&s(1.0, true), &[2, 3], &s(1.0, true), &[2, 3]));
        // A real difference in cost is no tie, and a cut within the limits beats any other.
        assert!(better(&s(1.0, true), &[2, 3], &s(1.1, true), &[5]));
        assert!(better(&s(9.0, true), &[2, 3], &s(0.1, false), &[5]));
        // Among cuts that keep no limit the larger smallest tile wins, ties as above.
        let f = |smallest: f64| Score {
            total: 1.0,
            smallest,
            cost: 0.0,
            ideal: false,
        };
        assert!(better(&f(0.12), &[2, 3], &f(0.10), &[5]));
        assert!(better(&f(0.10), &[5], &f(0.10), &[2, 3]));
    }

    #[test]
    fn rows_are_consecutive_and_their_prefix_marks_each_tile() {
        let aspects = [1.5, 0.75, 1.0, 2.0, 0.5];
        let rows = album_rows(&aspects);
        let mut next = 0;
        for row in &rows {
            assert_eq!(row.first, next);
            assert_eq!(row.prefix.len(), row.count);
            assert!((row.prefix[0]).abs() < 1e-12);
            let sum: f64 = aspects[row.first..row.first + row.count].iter().sum();
            assert!((row.aspect - sum).abs() < 1e-12);
            let mut begin = 0.0;
            for (j, a) in aspects[row.first..row.first + row.count].iter().enumerate() {
                assert!((row.prefix[j] - begin / sum).abs() < 1e-12);
                begin += a;
            }
            next += row.count;
        }
        assert_eq!(next, aspects.len());
        assert!(album_rows(&[]).is_empty());
        assert_eq!(lens(&album_rows(&[1.0])), [1]);
    }

    /// Every cut reached from the grid of aspects, for 2 to 5 tiles (59,049 albums of five).
    const GRID: [f64; 9] = [0.05, 0.3, 0.45, 0.75, 1.0, 1.33, 1.78, 2.5, 8.0];

    fn each_grid_album(mut check: impl FnMut(&[f64])) {
        for n in 2..=5_usize {
            let mut index = vec![0_usize; n];
            loop {
                let aspects: Vec<f64> = index
                    .iter()
                    .map(|&i| GRID[i].clamp(ASPECT_MIN, ASPECT_MAX))
                    .collect();
                check(&aspects);
                // Next in base 9.
                let mut k = 0;
                while k < n {
                    index[k] += 1;
                    if index[k] < GRID.len() {
                        break;
                    }
                    index[k] = 0;
                    k += 1;
                }
                if k == n {
                    break;
                }
            }
        }
    }

    #[test]
    fn every_tile_has_its_own_shape_and_the_rows_fill_the_width() {
        each_grid_album(|aspects| {
            let rows = album_rows(aspects);
            let mut next = 0;
            let mut total = 0.0;
            for row in &rows {
                assert_eq!(row.first, next, "{aspects:?}: consecutive");
                assert!(row.count >= 1);
                let h = row.height();
                let mut width = 0.0;
                for (j, a) in aspects[row.first..row.first + row.count].iter().enumerate() {
                    // The tile is a x h wide and h tall: its shape is its aspect, exactly.
                    let begin = row.prefix[j];
                    let end = row.prefix.get(j + 1).copied().unwrap_or(1.0);
                    assert!(((end - begin) - a / row.aspect).abs() < 1e-9, "{aspects:?}");
                    assert!((a * h - (end - begin)).abs() < 1e-9, "{aspects:?}");
                    width += a * h;
                }
                assert!(
                    (width - 1.0).abs() < 1e-9,
                    "{aspects:?}: the row fills the width"
                );
                total += h;
                next += row.count;
            }
            assert_eq!(next, aspects.len(), "{aspects:?}");
            assert!(total <= TOTAL_HARD + 1e-9, "{aspects:?}: {total}");
            assert_eq!(rows, album_rows(aspects), "{aspects:?}: same output twice");
        });
    }

    /// Whenever some cut keeps both limits, the chosen one does; when none can, the chosen cut
    /// is the one within the hard bound whose smallest tile is the largest.
    #[test]
    fn the_limits_hold_whenever_a_cut_can_keep_them() {
        let mut fallbacks = 0_u32;
        let mut kept = 0_u32;
        each_grid_album(|aspects| {
            let chosen = lens(&album_rows(aspects));
            let got = score(aspects, &chosen);
            if cuts(aspects.len()).any(|c| score(aspects, &c).ideal) {
                kept += 1;
                assert!(
                    got.ideal,
                    "{aspects:?}: {chosen:?} {} {}",
                    got.total, got.smallest
                );
            } else {
                fallbacks += 1;
                for cut in cuts(aspects.len()) {
                    let other = score(aspects, &cut);
                    if other.total <= TOTAL_HARD {
                        assert!(
                            got.smallest >= other.smallest - 1e-9,
                            "{aspects:?}: {chosen:?} vs {cut:?}"
                        );
                    }
                }
            }
        });
        // The grid is mostly extremes; both kinds of album occur in it.
        assert!(kept > 1000 && fallbacks > 1000, "{kept} {fallbacks}");
    }

    /// The pixel step of `AlbumBlock.qml` (`cells`), mirrored: the album's width `w`, the gap
    /// between tiles `gap`; every cell as `[x, y, w, h]` and the album's height.
    fn cells(rows: &[AlbumRow], w: i64, gap: i64) -> (Vec<[i64; 4]>, i64) {
        let round = |x: f64| x.round() as i64;
        let mut y = 0;
        let mut out = Vec::new();
        for row in rows {
            let k = row.count as i64;
            let inner = w - (k - 1) * gap;
            let h = round(inner as f64 / row.aspect).max(1);
            for j in 0..row.count {
                let left = round(row.prefix[j] * inner as f64) + j as i64 * gap;
                let right = if j + 1 == row.count {
                    w
                } else {
                    round(row.prefix[j + 1] * inner as f64) + j as i64 * gap
                };
                out.push([left, y, right - left, h]);
            }
            y += h + gap;
        }
        (out, y - gap)
    }

    /// Every cell of the pixel step is within 1 px of an exact box of its tile's shape: some
    /// height h' in [bh - 1, bh + 1] has |a h' - bw| <= 1 (the definition of "within 1 px" for the
    /// step and for the Qt tests).
    fn within_a_pixel(a: f64, bw: i64, bh: i64) -> bool {
        let (lo, hi) = (a * (bh as f64 - 1.0), a * (bh as f64 + 1.0));
        lo <= bw as f64 + 1.0 + 1e-9 && hi >= bw as f64 - 1.0 - 1e-9
    }

    #[test]
    fn the_pixel_step_keeps_each_box_within_a_pixel_of_its_shape() {
        const GAP: i64 = 2;
        let mut albums = 0_u32;
        // A sample of widths: the narrowest window, the odd ones, the width limit.
        for w in [120_i64, 151, 200, 233, 287, 288] {
            each_grid_album(|aspects| {
                albums += 1;
                let rows = album_rows(aspects);
                let (cells, height) = cells(&rows, w, GAP);
                assert_eq!(cells.len(), aspects.len());
                let mut next_y = 0;
                let mut row_start = 0;
                for row in &rows {
                    let mut x_end = 0;
                    let top = cells[row_start][1];
                    assert_eq!(top, next_y, "{aspects:?} at {w}: rows touch with one gap");
                    for j in 0..row.count {
                        let [x, y, bw, bh] = cells[row_start + j];
                        let a = aspects[row.first + j];
                        assert!(x >= 0 && x + bw <= w, "{aspects:?} at {w}: inside");
                        let before = if j == 0 { 0 } else { x_end + GAP };
                        assert_eq!(x, before, "{aspects:?} at {w}: one gap between tiles");
                        assert_eq!(y, top, "{aspects:?}: one row, one top");
                        assert!(
                            within_a_pixel(a, bw, bh),
                            "{aspects:?} at {w}: tile {j} of {:?} is {bw}x{bh}, shape {a}",
                            lens(&rows)
                        );
                        x_end = x + bw;
                    }
                    assert_eq!(x_end, w, "{aspects:?} at {w}: the row fills the width");
                    next_y = top + cells[row_start][3] + GAP;
                    row_start += row.count;
                }
                assert_eq!(height, next_y - GAP);
            });
        }
        assert!(albums > 300_000, "{albums}");
    }
}
