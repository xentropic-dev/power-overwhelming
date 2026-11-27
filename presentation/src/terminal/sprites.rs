//! Box drawing, blocks, braille and Powerline glyphs drawn as shapes that fit their cell
//! exactly, as Ghostty does. Font glyphs for these rarely span the whole
//! cell, so tables get gaps between rows and block art gets seams.
//!
//! Adapted from herdr-app `src/sprites.rs`, licensed under the Apache License, Version 2.0.

use gpui::{Bounds, ContentMask, Hsla, PathBuilder, Pixels, Point, Window, fill, point, px, size};

/// Per direction (up, right, down, left): 0 none, 1 light, 2 heavy, 3 double.
fn lines(c: char) -> Option<[u8; 4]> {
    const LIGHT_HEAVY: &str = "\
        0101 0202 1010 2020 0101 0202 1010 2020 0101 0202 1010 2020 \
        0110 0210 0120 0220 0011 0012 0021 0022 1100 1200 2100 2200 \
        1001 1002 2001 2002 1110 1210 2110 1120 2120 2210 1220 2220 \
        1011 1012 2011 1021 2021 2012 1022 2022 0111 0112 0211 0212 \
        0121 0122 0221 0222 1101 1102 1201 1202 2101 2102 2201 2202 \
        1111 1112 1211 1212 2111 1121 2121 2112 2211 1122 1221 2212 \
        1222 2122 2221 2222 0101 0202 1010 2020";
    const DOUBLE: &str = "\
        0303 3030 0310 0130 0330 0013 0031 0033 1300 3100 3300 1003 \
        3001 3003 1310 3130 3330 1013 3031 3033 0313 0131 0333 1303 \
        3101 3303 1313 3131 3333 0110 0011 1001 1100";
    const SINGLES: &str = "0001 1000 0100 0010 0002 2000 0200 0020 0201 1020 0102 2010";
    let code = c as u32;
    let (table, base) = match code {
        0x2500..=0x254F => (LIGHT_HEAVY, 0x2500),
        0x2550..=0x2570 => (DOUBLE, 0x2550),
        0x2574..=0x257F => (SINGLES, 0x2574),
        _ => return None,
    };
    let word = table.split_whitespace().nth((code - base) as usize)?;
    let mut out = [0u8; 4];
    for (o, b) in out.iter_mut().zip(word.bytes()) {
        *o = b - b'0';
    }
    Some(out)
}

/// Rounded corners (╭╮╯╰): which way their two arms leave the center,
/// (right or left, down or up). Drawn as arcs, as Ghostty does.
fn arc(c: char) -> Option<(f32, f32)> {
    match c {
        '╭' => Some((1.0, 1.0)),
        '╮' => Some((-1.0, 1.0)),
        '╯' => Some((-1.0, -1.0)),
        '╰' => Some((1.0, -1.0)),
        _ => None,
    }
}

/// Dashed lines: (dashes per cell, heavy, horizontal), as Ghostty draws them.
fn dashes(c: char) -> Option<(u8, bool, bool)> {
    Some(match c {
        '┄' => (3, false, true),
        '┅' => (3, true, true),
        '┆' => (3, false, false),
        '┇' => (3, true, false),
        '┈' => (4, false, true),
        '┉' => (4, true, true),
        '┊' => (4, false, false),
        '┋' => (4, true, false),
        '╌' => (2, false, true),
        '╍' => (2, true, true),
        '╎' => (2, false, false),
        '╏' => (2, true, false),
        _ => return None,
    })
}

/// Rectangles in cell fractions (x0, y0, x1, y1) and an alpha, for block elements.
fn blocks(c: char) -> Option<(Vec<[f32; 4]>, f32)> {
    let e = 1.0 / 8.0;
    let r = |x0: f32, y0: f32, x1: f32, y1: f32| [x0, y0, x1, y1];
    let quads = |ul: bool, ur: bool, ll: bool, lr: bool| {
        let mut v = Vec::new();
        if ul {
            v.push(r(0.0, 0.0, 0.5, 0.5))
        }
        if ur {
            v.push(r(0.5, 0.0, 1.0, 0.5))
        }
        if ll {
            v.push(r(0.0, 0.5, 0.5, 1.0))
        }
        if lr {
            v.push(r(0.5, 0.5, 1.0, 1.0))
        }
        v
    };
    Some(match c {
        '▀' => (vec![r(0.0, 0.0, 1.0, 0.5)], 1.0),
        '▁'..='█' => {
            let n = (c as u32 - '▁' as u32 + 1) as f32;
            (vec![r(0.0, 1.0 - n * e, 1.0, 1.0)], 1.0)
        }
        '▉'..='▏' => {
            let n = (8 - (c as u32 - '▉' as u32 + 1)) as f32;
            (vec![r(0.0, 0.0, n * e, 1.0)], 1.0)
        }
        '▐' => (vec![r(0.5, 0.0, 1.0, 1.0)], 1.0),
        '░' => (vec![r(0.0, 0.0, 1.0, 1.0)], 0.25),
        '▒' => (vec![r(0.0, 0.0, 1.0, 1.0)], 0.5),
        '▓' => (vec![r(0.0, 0.0, 1.0, 1.0)], 0.75),
        '▔' => (vec![r(0.0, 0.0, 1.0, e)], 1.0),
        '▕' => (vec![r(1.0 - e, 0.0, 1.0, 1.0)], 1.0),
        '▖' => (quads(false, false, true, false), 1.0),
        '▗' => (quads(false, false, false, true), 1.0),
        '▘' => (quads(true, false, false, false), 1.0),
        '▙' => (quads(true, false, true, true), 1.0),
        '▚' => (quads(true, false, false, true), 1.0),
        '▛' => (quads(true, true, true, false), 1.0),
        '▜' => (quads(true, true, false, true), 1.0),
        '▝' => (quads(false, true, false, false), 1.0),
        '▞' => (quads(false, true, true, false), 1.0),
        '▟' => (quads(false, true, true, true), 1.0),
        _ => return None,
    })
}

/// Ghostty's dot sizing, with vertical spacing even across adjacent cells.
/// Bit order is 1,2,3,7 down the left, 4,5,6,8 down the right.
fn braille(c: char, width: i32, height: i32) -> Option<Vec<[i32; 4]>> {
    if !('\u{2800}'..='\u{28FF}').contains(&c) {
        return None;
    }
    let mut dot = (width / 4).min(height / 8);
    let (mut xs, ys) = (width / 4, height / 8);
    let (mut xm, ym) = (xs / 2, ys / 2);
    let mut xr = width - 2 * xm - xs - 2 * dot;
    let mut yr = height - 2 * ym - 3 * ys - 4 * dot;
    if xr >= 2 && yr >= 4 && dot == 0 {
        dot += 1;
        xr -= 2;
        yr -= 4;
    }
    if xr >= 2 && xm == 0 {
        xm = 1;
        xr -= 2;
    }
    if yr >= 2 && ym == 0 {
        yr -= 2;
    }
    if xr >= 1 {
        xs += 1;
        xr -= 1;
    }
    if yr >= 3 {
        yr -= 3;
    }
    if xr >= 2 {
        xm += 1;
        xr -= 2;
    }
    if yr >= 2 {
        yr -= 2;
    }
    if xr >= 2 && yr >= 4 {
        dot += 1;
    }
    let mut rects = Vec::new();
    for (bit, col, row) in [
        (0, 0, 0),
        (1, 0, 1),
        (2, 0, 2),
        (3, 1, 0),
        (4, 1, 1),
        (5, 1, 2),
        (6, 0, 3),
        (7, 1, 3),
    ] {
        if (c as u32) & (1 << bit) != 0 {
            let x = xm + col * (dot + xs);
            let y = ((2 * row + 1) * height - 4 * dot + 4) / 8;
            rects.push([x, y, x + dot, y + dot]);
        }
    }
    Some(rects)
}

/// Record symbol, which few terminal fonts cover.
const RECORD: char = '\u{23FA}';

/// Recognizes one sprite scalar. Grapheme clusters with modifiers still go to the text shaper.
pub(super) fn sprite_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    matches!(c, '\u{2500}'..='\u{259F}' | '\u{2800}'..='\u{28FF}' | '\u{E0B0}'..='\u{E0BF}' | RECORD)
        .then_some(c)
}

/// Paints a sprite in the given cell. Geometry is calculated in device pixels so adjoining
/// sprites share the same edges at fractional display scales.
pub(super) fn paint(c: char, cell: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let scale = window.scale_factor();
    let device = |p: Pixels| (f32::from(p) * scale).round();
    let (x, y, right, bottom) = (
        device(cell.origin.x),
        device(cell.origin.y),
        device(cell.right()),
        device(cell.bottom()),
    );
    let (w, h) = (right - x, bottom - y);
    if w <= 0.0 || h <= 0.0 {
        return;
    }

    let logical = |d: f32| px(d / scale);
    let at = |x: f32, y: f32| point(logical(x), logical(y));
    let mut rect = |x0: f32, y0: f32, x1: f32, y1: f32, color: Hsla| {
        let (x0, y0, x1, y1) = (
            x0.round().max(x),
            y0.round().max(y),
            x1.round().min(right),
            y1.round().min(bottom),
        );
        if x1 > x0 && y1 > y0 {
            window.paint_quad(fill(
                Bounds::new(at(x0, y0), size(logical(x1 - x0), logical(y1 - y0))),
                color,
            ));
        }
    };

    if let Some(dots) = braille(c, w as i32, h as i32) {
        for [x0, y0, x1, y1] in dots {
            rect(
                x + x0 as f32,
                y + y0 as f32,
                x + x1 as f32,
                y + y1 as f32,
                color,
            );
        }
        return;
    }
    if c == RECORD {
        let d = (w * 0.62).min(h * 0.5).round().max(1.0);
        let (x0, y0) = ((x + (w - d) / 2.0).round(), (y + (h - d) / 2.0).round());
        window.paint_quad(
            fill(Bounds::new(at(x0, y0), size(logical(d), logical(d))), color)
                .corner_radii(logical(d / 2.0)),
        );
        return;
    }

    let light = (w / 8.0).round().max(1.0);
    if ('\u{E0B0}'..='\u{E0BF}').contains(&c) {
        powerline(c, at, (x, y, w, h), logical(light), color, window);
        return;
    }
    if ('╱'..='╳').contains(&c) {
        // Extend the stroke through the corners so it reaches the edges at full width.
        let extension = light / w.max(h);
        let (dx, dy) = (w * extension, h * extension);
        let mut path = PathBuilder::stroke(logical(light));
        if c != '╲' {
            path.move_to(at(x - dx, bottom + dy));
            path.line_to(at(right + dx, y - dy));
        }
        if c != '╱' {
            path.move_to(at(x - dx, y - dy));
            path.line_to(at(right + dx, bottom + dy));
        }
        if let Ok(path) = path.build() {
            let bounds = Bounds::new(at(x, y), size(logical(w), logical(h)));
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                window.paint_path(path, color);
            });
        }
        return;
    }
    if let Some((rects, alpha)) = blocks(c) {
        let mut color = color;
        color.a *= alpha;
        for [x0, y0, x1, y1] in rects {
            rect(x + x0 * w, y + y0 * h, x + x1 * w, y + y1 * h, color);
        }
        return;
    }

    let (cx, cy) = (x + (w / 2.0).floor(), y + (h / 2.0).floor());
    if let Some((sx, sy)) = arc(c) {
        // Match the pixel-aligned edges of a straight arm at each endpoint.
        let stroke_offset = (light % 2.0) / 2.0;
        let (cx, cy) = (cx + stroke_offset, cy + stroke_offset);
        let ex = if sx > 0.0 { right } else { x };
        let ey = if sy > 0.0 { bottom } else { y };
        let (r, k) = ((ex - cx).abs().min((ey - cy).abs()), 0.25);
        let mut path = PathBuilder::stroke(logical(light));
        path.move_to(at(cx, ey));
        path.line_to(at(cx, cy + sy * r));
        path.cubic_bezier_to(
            at(cx + sx * r, cy),
            at(cx, cy + sy * k * r),
            at(cx + sx * k * r, cy),
        );
        path.line_to(at(ex, cy));
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
        return;
    }
    let Some(rects) = line_rects(c, (x, y, w, h), light) else {
        return;
    };
    // Arms overlap at the center. Painting them separately would stack a
    // translucent (faint) color there, so paint their union as disjoint quads.
    for [x0, y0, x1, y1] in disjoint(rects) {
        rect(x0, y0, x1, y1, color);
    }
}

/// Box drawing lines and dashes as rectangles snapped to device pixels and
/// clipped to the cell. They may overlap.
fn line_rects(c: char, (x, y, w, h): (f32, f32, f32, f32), light: f32) -> Option<Vec<[f32; 4]>> {
    let (right, bottom) = (x + w, y + h);
    let (cx, cy) = (x + (w / 2.0).floor(), y + (h / 2.0).floor());
    let mut rects = Vec::new();
    if let Some((n, heavy, horizontal)) = dashes(c) {
        // Gaps are at most half the cell. A horizontal line keeps half a gap
        // at each end and a vertical one its gap below, so neighbors tile evenly.
        let t = if heavy { light * 2.0 } else { light };
        let (len, from) = if horizontal { (w, x) } else { (h, y) };
        let gap = (len / (2.0 * n as f32)).min(light.max(4.0));
        let dash = (len - gap * n as f32) / n as f32;
        let mut offset = from + if horizontal { gap / 2.0 } else { 0.0 };
        for _ in 0..n {
            if horizontal {
                rects.push([offset, cy - t / 2.0, offset + dash, cy + t / 2.0]);
            } else {
                rects.push([cx - t / 2.0, offset, cx + t / 2.0, offset + dash]);
            }
            offset += dash + gap;
        }
        return Some(snap(rects, (x, y, right, bottom)));
    }
    let dirs = lines(c)?;
    let thick = |k: u8| if k == 2 { light * 2.0 } else { light };
    // Where a line along an arm starts, measured from the center toward the
    // arm: `before` and `after` are the crossing arms on this line's side and
    // the other side. Negative reaches past the center so joins close.
    let cover = |k: u8| if k == 3 { light * 1.5 } else { thick(k) / 2.0 };
    let start = |before: u8, after: u8, double: bool| -> f32 {
        match (before, after) {
            // A double line stops at a double crossing on its side (inner corner).
            (3, _) if double => light / 2.0,
            (0, 0) => 0.0,
            (0, k) | (k, _) => -cover(k),
        }
    };
    // Arms: (direction, sign along the axis, horizontal?).
    for (i, sign, horizontal) in [
        (0usize, -1.0f32, false),
        (1, 1.0, true),
        (2, 1.0, false),
        (3, -1.0, true),
    ] {
        let k = dirs[i];
        if k == 0 {
            continue;
        }
        // The crossing arms on each side of this arm (up/down for a horizontal arm).
        let (side_a, side_b) = if horizontal {
            (dirs[0], dirs[2])
        } else {
            (dirs[3], dirs[1])
        };
        let double = [
            (-light, light, side_a, side_b),
            (light, light, side_b, side_a),
        ];
        let single = [(0.0, thick(k), side_a.max(side_b), side_a.min(side_b))];
        let strokes: &[_] = if k == 3 { &double } else { &single };
        for &(off, t, before, after) in strokes {
            let near = start(before, after, k == 3);
            let (c, edge) = if horizontal {
                (cx, if sign > 0.0 { right } else { x })
            } else {
                (cy, if sign > 0.0 { bottom } else { y })
            };
            let from = c + sign * near;
            let (a0, a1) = if sign > 0.0 {
                (from, edge)
            } else {
                (edge, from)
            };
            if horizontal {
                rects.push([a0, cy + off - t / 2.0, a1, cy + off + t / 2.0]);
            } else {
                rects.push([cx + off - t / 2.0, a0, cx + off + t / 2.0, a1]);
            }
        }
    }
    Some(snap(rects, (x, y, right, bottom)))
}

fn snap(rects: Vec<[f32; 4]>, (x, y, right, bottom): (f32, f32, f32, f32)) -> Vec<[f32; 4]> {
    rects
        .into_iter()
        .map(|[x0, y0, x1, y1]| {
            [
                x0.round().max(x),
                y0.round().max(y),
                x1.round().min(right),
                y1.round().min(bottom),
            ]
        })
        .filter(|[x0, y0, x1, y1]| x1 > x0 && y1 > y0)
        .collect()
}

/// The union of rectangles as non-overlapping ones: split along every edge,
/// keep covered grid cells and merge each covered run within a row band.
fn disjoint(rects: Vec<[f32; 4]>) -> Vec<[f32; 4]> {
    let edges = |a: usize, b: usize| {
        let mut v: Vec<f32> = rects.iter().flat_map(|r| [r[a], r[b]]).collect();
        v.sort_by(f32::total_cmp);
        v.dedup();
        v
    };
    let (xs, ys) = (edges(0, 2), edges(1, 3));
    let mut out = Vec::new();
    for band in ys.windows(2) {
        let (y0, y1) = (band[0], band[1]);
        let mut run: Option<f32> = None;
        for (i, col) in xs.windows(2).enumerate() {
            let covered = rects
                .iter()
                .any(|r| r[0] <= col[0] && col[1] <= r[2] && r[1] <= y0 && y1 <= r[3]);
            match (covered, run) {
                (true, None) => run = Some(col[0]),
                (false, Some(x0)) => {
                    out.push([x0, y0, col[0], y1]);
                    run = None;
                }
                _ => {}
            }
            if i == xs.len() - 2
                && let Some(x0) = run.take()
            {
                out.push([x0, y0, col[1], y1]);
            }
        }
    }
    out
}

/// Nerd Font powerline separators (U+E0B0..E0BF): arrows, half circles and
/// slants, drawn to fill the cell exactly as Ghostty does. Fonts that have
/// them rarely match the cell, and fonts that don't fall back to whatever
/// font has those codes (on Linux, sometimes digits).
fn powerline(
    c: char,
    at: impl Fn(f32, f32) -> Point<Pixels>,
    (x, y, w, h): (f32, f32, f32, f32),
    line: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let solid = (c as u32).is_multiple_of(2);
    // A solid shape reaches a device pixel past its cell's sides, so its flat
    // edge meets the neighbor's color without an anti-aliased seam.
    let ext = if solid { 1.0 } else { 0.0 };
    let p = |u: f32, v: f32| at(x + u * w + (u - 0.5) * 2.0 * ext, y + v * h);
    let mut path = if solid {
        PathBuilder::fill()
    } else {
        PathBuilder::stroke(line)
    };
    // A half ellipse is two quarter arcs, each approximated by a cubic.
    const K: f32 = 0.552_284_8;
    match c {
        '\u{E0B0}' | '\u{E0B1}' => {
            path.add_polygon(&[p(0.0, 0.0), p(1.0, 0.5), p(0.0, 1.0)], solid)
        }
        '\u{E0B2}' | '\u{E0B3}' => {
            path.add_polygon(&[p(1.0, 0.0), p(0.0, 0.5), p(1.0, 1.0)], solid)
        }
        '\u{E0B4}' | '\u{E0B5}' => {
            path.move_to(p(0.0, 0.0));
            path.cubic_bezier_to(p(1.0, 0.5), p(K, 0.0), p(1.0, 0.5 - K / 2.0));
            path.cubic_bezier_to(p(0.0, 1.0), p(1.0, 0.5 + K / 2.0), p(K, 1.0));
            if solid {
                path.close();
            }
        }
        '\u{E0B6}' | '\u{E0B7}' => {
            path.move_to(p(1.0, 0.0));
            path.cubic_bezier_to(p(0.0, 0.5), p(1.0 - K, 0.0), p(0.0, 0.5 - K / 2.0));
            path.cubic_bezier_to(p(1.0, 1.0), p(0.0, 0.5 + K / 2.0), p(1.0 - K, 1.0));
            if solid {
                path.close();
            }
        }
        '\u{E0B8}' => path.add_polygon(&[p(0.0, 0.0), p(1.0, 1.0), p(0.0, 1.0)], true),
        '\u{E0BA}' => path.add_polygon(&[p(1.0, 0.0), p(1.0, 1.0), p(0.0, 1.0)], true),
        '\u{E0BC}' => path.add_polygon(&[p(0.0, 0.0), p(1.0, 0.0), p(0.0, 1.0)], true),
        '\u{E0BE}' => path.add_polygon(&[p(0.0, 0.0), p(1.0, 0.0), p(1.0, 1.0)], true),
        '\u{E0B9}' | '\u{E0BF}' => path.add_polygon(&[p(0.0, 0.0), p(1.0, 1.0)], false),
        '\u{E0BB}' | '\u{E0BD}' => path.add_polygon(&[p(0.0, 1.0), p(1.0, 0.0)], false),
        _ => return,
    }
    if let Ok(path) = path.build() {
        // The overshoot only removes the seam at this cell's own edges; the
        // mask keeps it from painting over the neighbors' last columns.
        let bounds = clip_bounds((x, y, w, h), at);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_path(path, color);
        });
    }
}

/// The snapped cell in logical pixels, which no powerline shape may leave.
fn clip_bounds(
    (x, y, w, h): (f32, f32, f32, f32),
    at: impl Fn(f32, f32) -> Point<Pixels>,
) -> Bounds<Pixels> {
    let (origin, end) = (at(x, y), at(x + w, y + h));
    Bounds::new(origin, size(end.x - origin.x, end.y - origin.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braille_patterns_use_all_eight_bits_and_stay_in_the_cell() {
        assert_eq!(braille('A', 8, 22), None);
        let positions = [
            (1, 2),
            (1, 7),
            (1, 13),
            (5, 2),
            (5, 7),
            (5, 13),
            (1, 18),
            (5, 18),
        ];
        for pattern in 0u32..256 {
            let c = char::from_u32(0x2800 + pattern).unwrap();
            let expected: Vec<_> = positions
                .iter()
                .enumerate()
                .filter(|(bit, _)| pattern & (1 << bit) != 0)
                .map(|(_, &(x, y))| [x, y, x + 2, y + 2])
                .collect();
            assert_eq!(braille(c, 8, 22).unwrap(), expected);
        }
        for w in 0..80 {
            for h in 0..160 {
                for [x0, y0, x1, y1] in braille('⣿', w, h).unwrap() {
                    assert!(0 <= x0 && x0 <= x1 && x1 <= w);
                    assert!(0 <= y0 && y0 <= y1 && y1 <= h);
                    assert_eq!(x1 - x0, y1 - y0);
                }
                let dots = braille('⡇', w, h).unwrap();
                let gaps = [
                    dots[1][1] - dots[0][1],
                    dots[2][1] - dots[1][1],
                    dots[3][1] - dots[2][1],
                    h + dots[0][1] - dots[3][1],
                ];
                assert!(
                    gaps.iter().max().unwrap() - gaps.iter().min().unwrap() <= 1,
                    "{w}x{h}: {gaps:?}"
                );
            }
        }
    }

    #[test]
    fn recognizes_every_sprite_range_and_only_single_scalars() {
        for c in ('\u{2500}'..='\u{259F}')
            .chain('\u{2800}'..='\u{28FF}')
            .chain('\u{E0B0}'..='\u{E0BF}')
            .chain([RECORD])
        {
            assert_eq!(sprite_char(c.encode_utf8(&mut [0; 4])), Some(c));
        }
        for text in [
            "",
            "A",
            "⏺\u{FE0F}",
            "─\u{301}",
            "⠁⠂",
            "\u{24FF}",
            "\u{25A0}",
            "\u{27FF}",
            "\u{2900}",
            "\u{E0AF}",
            "\u{E0C0}",
        ] {
            assert_eq!(sprite_char(text), None, "{text:?}");
        }
    }

    #[test]
    fn tables_cover_their_ranges() {
        assert_eq!(lines('─'), Some([0, 1, 0, 1]));
        assert_eq!(lines('┼'), Some([1, 1, 1, 1]));
        assert_eq!(lines('╋'), Some([2, 2, 2, 2]));
        assert_eq!(lines('┌'), Some([0, 1, 1, 0]));
        assert_eq!(lines('┘'), Some([1, 0, 0, 1]));
        assert_eq!(lines('╏'), Some([2, 0, 2, 0]));
        assert_eq!(lines('╔'), Some([0, 3, 3, 0]));
        assert_eq!(lines('╬'), Some([3, 3, 3, 3]));
        assert_eq!(lines('╭'), Some([0, 1, 1, 0]));
        assert_eq!(lines('╰'), Some([1, 1, 0, 0]));
        assert_eq!(lines('╱'), None);
        assert_eq!(lines('╿'), Some([2, 0, 1, 0]));
        for c in ('\u{2500}'..='\u{2570}').chain('\u{2574}'..='\u{257F}') {
            assert!(lines(c).is_some(), "{c} has no entry");
        }
        for c in '\u{2580}'..='\u{259F}' {
            assert!(blocks(c).is_some(), "{c} has no entry");
        }
    }

    #[test]
    fn rounded_corners_and_dashes_have_their_own_shapes() {
        for c in ['╭', '╮', '╯', '╰'] {
            assert!(arc(c).is_some(), "{c}");
        }
        for c in ['┄', '┅', '┆', '┇', '┈', '┉', '┊', '┋', '╌', '╍', '╎', '╏']
        {
            assert!(dashes(c).is_some(), "{c}");
        }
        assert_eq!(arc('┌'), None);
        assert_eq!(dashes('─'), None);
    }

    fn coverage(rects: &[[f32; 4]], px: f32, py: f32) -> usize {
        rects
            .iter()
            .filter(|r| r[0] <= px && px < r[2] && r[1] <= py && py < r[3])
            .count()
    }

    #[test]
    fn faint_intersections_paint_every_pixel_once() {
        // A translucent (SGR 2) color must not stack where arms cross.
        for c in ['─', '┬', '┼', '╋', '╬', '┿', '╪', '╫', '┄', '╍'] {
            for (w, h) in [(8.0, 17.0), (9.0, 20.0), (16.0, 34.0)] {
                let light = (w / 8.0f32).round().max(1.0);
                let rects = disjoint(line_rects(c, (3.0, 5.0, w, h), light).unwrap());
                for py in 5..5 + h as i32 {
                    for px in 3..3 + w as i32 {
                        let n = coverage(&rects, px as f32 + 0.5, py as f32 + 0.5);
                        assert!(n <= 1, "{c} {w}x{h} pixel ({px},{py}) painted {n} times");
                    }
                }
            }
        }
    }

    #[test]
    fn disjoint_keeps_the_union() {
        let rects = line_rects('╋', (0.0, 0.0, 10.0, 21.0), 1.0).unwrap();
        let union = disjoint(rects.clone());
        for py in 0..21 {
            for px in 0..10 {
                let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
                assert_eq!(coverage(&rects, fx, fy) > 0, coverage(&union, fx, fy) == 1);
            }
        }
    }

    #[test]
    fn powerline_stays_inside_its_cell_between_contrasting_backgrounds() {
        // Cells at device x 0..9, 9..18, 18..27; the separator sits in the middle.
        let at = |x: f32, y: f32| point(px(x / 2.0), px(y / 2.0));
        let b = clip_bounds((9.0, 0.0, 9.0, 20.0), at);
        assert_eq!(b.origin.x, px(4.5));
        assert_eq!(b.right(), px(9.0));
        assert_eq!(b.origin.y, px(0.0));
        assert_eq!(b.bottom(), px(10.0));
    }
}
