//! Terminal cell metrics measured from the font, as Ghostty sizes its grid.
//!
//! Adapted from herdr-app `src/terminal.rs` (`font_cell_metrics`, `adjusted_cell_width`,
//! `adjusted_cell_height` and `glyph_x`), licensed under the Apache License,
//! Version 2.0. herdr reads the font tables through a patched GPUI; this port finds the font
//! file with `fontdb` and reads it with `ttf-parser`, and falls back to what stock GPUI
//! reports when the file can't be found.

use domain::session::Geometry;
use gpui::{Font, FontId, Pixels, Point, Size, TextSystem, point, px, size};
use std::{cell::RefCell, collections::HashMap, sync::LazyLock};

/// The font the terminal grid is measured from.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalFont {
    pub font: Font,
    /// In logical pixels, zoom included.
    pub size: f32,
    /// Cell height multiplier, 0.8–2.0.
    pub line_height: f32,
    /// Cell width multiplier, 0.8–2.0.
    pub cell_width: f32,
}

/// One terminal cell's size and the positions inside it, in logical pixels. Every value is
/// a whole number of device pixels, so the grid lines up with the screen's pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub size: Size<Pixels>,
    /// How far every glyph moves right in its cell, as Ghostty's `renderGlyph` (`glyph_x`).
    pub glyph_x: Pixels,
    /// From the cell's top to the text baseline.
    pub baseline: Pixels,
    /// From the cell's top to the top of the underline.
    pub underline: Pixels,
    pub underline_thickness: Pixels,
    /// From the cell's top to the top of the strikethrough.
    pub strike: Pixels,
    pub strike_thickness: Pixels,
    /// Device pixels per logical pixel.
    pub scale: f32,
}

impl Default for CellMetrics {
    /// The fixed 8×18 grid used before the font is measured.
    fn default() -> Self {
        Self {
            size: size(px(8.), px(18.)),
            glyph_x: px(0.),
            baseline: px(14.),
            underline: px(15.),
            underline_thickness: px(1.),
            strike: px(9.),
            strike_thickness: px(1.),
            scale: 1.,
        }
    }
}

impl CellMetrics {
    /// The cell's size in device pixels, as the PTY and the mouse encoder report it.
    pub fn device_size(&self) -> Size<u32> {
        size(
            self.device_pixels(self.size.width),
            self.device_pixels(self.size.height),
        )
    }

    /// A logical length in whole device pixels.
    pub fn device_pixels(&self, length: Pixels) -> u32 {
        (f32::from(length) * self.scale).round().max(0.) as u32
    }

    /// A logical size in whole device pixels, rounding down, as the cells it holds are counted.
    pub fn whole_device_size(&self, length: Size<Pixels>) -> Size<u32> {
        // The tolerance keeps a length that is a whole number of device pixels from losing one
        // to float error.
        let whole = |length: Pixels| (f32::from(length) * self.scale + 1e-3).floor().max(0.) as u32;
        size(whole(length.width), whole(length.height))
    }

    /// The PTY grid that fits in `grid`: the whole cells, at most `Geometry::MAX_CELLS` each
    /// way. Cell sizes are in device pixels, as Ghostty reports them.
    pub fn grid(&self, grid: Size<Pixels>) -> Geometry {
        let cell = self.device_size();
        let grid = self.whole_device_size(grid);
        let fit = |length: u32, cell: u32| {
            (length / cell.max(1)).clamp(1, u32::from(Geometry::MAX_CELLS)) as u16
        };
        let pixels = |cell: u32| cell.clamp(1, u32::from(Geometry::MAX_CELL_PIXELS)) as u16;
        Geometry {
            cols: fit(grid.width, cell.width),
            rows: fit(grid.height, cell.height),
            cell_width: pixels(cell.width),
            cell_height: pixels(cell.height),
        }
    }

    /// Moves a point onto the nearest device pixel.
    pub fn snap(&self, at: Point<Pixels>) -> Point<Pixels> {
        let snap = |value: Pixels| px((f32::from(value) * self.scale).round() / self.scale);
        point(snap(at.x), snap(at.y))
    }

    /// The top-left corner of a cell, from the grid's (snapped) origin.
    pub fn cell_origin(&self, origin: Point<Pixels>, row: usize, column: usize) -> Point<Pixels> {
        origin
            + point(
                self.size.width * column as f32,
                self.size.height * row as f32,
            )
    }
}

/// Measures the grid for `font` at the window's scale. Results are cached per font, size,
/// scale and multipliers, so calling this every frame is cheap.
pub fn cell_metrics(text: &TextSystem, font: &TerminalFont, scale: f32) -> CellMetrics {
    type Key = (FontId, u32, u32, u32, u32);
    thread_local! {
        static FACES: RefCell<HashMap<FontId, FaceMetrics>> = RefCell::default();
        static CELLS: RefCell<HashMap<Key, CellMetrics>> = RefCell::default();
    }
    let id = text.resolve_font(&font.font);
    let key = (
        id,
        font.size.to_bits(),
        scale.to_bits(),
        font.line_height.to_bits(),
        font.cell_width.to_bits(),
    );
    if let Some(metrics) = CELLS.with_borrow(|cells| cells.get(&key).copied()) {
        return metrics;
    }
    let face = FACES.with_borrow_mut(|faces| {
        *faces
            .entry(id)
            .or_insert_with(|| FaceMetrics::measure(text, id, &font.font))
    });
    let metrics = face.cell(font.size, scale, font.line_height, font.cell_width);
    CELLS.with_borrow_mut(|cells| {
        if cells.len() > 64 {
            cells.clear();
        }
        cells.insert(key, metrics);
    });
    metrics
}

/// The widest printable ASCII advance of `font` at `font_size`, on whole device pixels. For
/// laying out monospace UI text, such as the status bar, in columns.
pub fn column_width(text: &TextSystem, font: &Font, font_size: Pixels, scale: f32) -> Pixels {
    let id = text.resolve_font(font);
    let width = (' '..='~')
        .filter_map(|ch| text.advance(id, font_size, ch).ok())
        .map(|advance| f32::from(advance.width))
        .reduce(f32::max)
        .unwrap_or(f32::from(font_size) * 0.6);
    px(((width * scale).round() / scale).max(1. / scale))
}

/// The font file's metrics in font units (y up, descent negative), as Ghostty reads them.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FaceMetrics {
    units_per_em: f32,
    ascent: f32,
    descent: f32,
    line_gap: f32,
    x_height: Option<f32>,
    cap_height: Option<f32>,
    /// Position and thickness, as the `post` table stores them.
    underline: Option<(f32, f32)>,
    /// Position and thickness, as the `OS/2` table stores them.
    strikeout: Option<(f32, f32)>,
    /// The widest printable ASCII advance.
    advance: f32,
}

/// The system's fonts and the bundled one, for reading tables GPUI doesn't expose.
static FONT_FILES: LazyLock<fontdb::Database> = LazyLock::new(|| {
    let mut fonts = fontdb::Database::new();
    fonts.load_system_fonts();
    fonts.load_font_source(fontdb::Source::Binary(std::sync::Arc::new(
        super::BUNDLED_FONT,
    )));
    fonts
});

/// Loads the system font list on another thread, so the first measurement doesn't wait.
pub fn preload_font_files() {
    std::thread::spawn(|| LazyLock::force(&FONT_FILES));
}

/// Whether the system's fonts or the bundled one have a face of `family`. GPUI's own font list
/// appends its fallback names, so it can't answer this.
pub fn has_family(family: &str) -> bool {
    database_has_family(&FONT_FILES, family)
}

fn database_has_family(fonts: &fontdb::Database, family: &str) -> bool {
    fonts
        .faces()
        .any(|face| face.families.iter().any(|(name, _)| name == family))
}

impl FaceMetrics {
    fn measure(text: &TextSystem, id: FontId, font: &Font) -> Self {
        let reported = Self::from_text_system(text, id);
        // Use the file only when it is the face GPUI resolved: its advances must agree.
        Self::from_file(font)
            .filter(|file| {
                (file.advance / file.units_per_em - reported.advance / reported.units_per_em).abs()
                    < 1e-3
            })
            .unwrap_or(reported)
    }

    fn from_file(font: &Font) -> Option<Self> {
        let family = match font.family.as_ref() {
            // fontdb's generic families aren't the system's; let GPUI answer for them.
            "monospace" | "sans-serif" | "serif" | "system-ui" => return None,
            name => fontdb::Family::Name(name),
        };
        let query = fontdb::Query {
            families: &[family],
            weight: fontdb::Weight(font.weight.0.round().clamp(1., 1000.) as u16),
            stretch: fontdb::Stretch::Normal,
            style: match font.style {
                gpui::FontStyle::Normal => fontdb::Style::Normal,
                gpui::FontStyle::Italic => fontdb::Style::Italic,
                gpui::FontStyle::Oblique => fontdb::Style::Oblique,
            },
        };
        let id = FONT_FILES.query(&query)?;
        FONT_FILES
            .with_face_data(id, |data, index| {
                ttf_parser::Face::parse(data, index)
                    .ok()
                    .map(|face| Self::from_face(&face))
            })
            .flatten()
    }

    fn from_face(face: &ttf_parser::Face) -> Self {
        let tables = face.tables();
        let hhea = tables.hhea;
        let os2 = tables.os2;
        let typo = |os2: ttf_parser::os2::Table| {
            (
                f32::from(os2.typographic_ascender()),
                f32::from(os2.typographic_descender()),
                f32::from(os2.typographic_line_gap()),
            )
        };
        // Ghostty's order: USE_TYPO_METRICS, then hhea, then typo, then the Windows metrics.
        let (ascent, descent, line_gap) = match os2 {
            Some(os2) if os2.use_typographic_metrics() => typo(os2),
            _ if hhea.ascender != 0 || hhea.descender != 0 => (
                f32::from(hhea.ascender),
                f32::from(hhea.descender),
                f32::from(hhea.line_gap),
            ),
            Some(os2) if os2.typographic_ascender() != 0 || os2.typographic_descender() != 0 => {
                typo(os2)
            }
            Some(os2) => (
                f32::from(os2.windows_ascender()),
                -f32::from(os2.windows_descender()),
                0.,
            ),
            None => (
                f32::from(hhea.ascender),
                f32::from(hhea.descender),
                f32::from(hhea.line_gap),
            ),
        };
        let glyph_height = |ch| {
            let glyph = face.glyph_index(ch)?;
            let bounds = face.glyph_bounding_box(glyph)?;
            let height = f32::from(bounds.y_max) - f32::from(bounds.y_min);
            (height > 0.).then_some(height)
        };
        let positive = |value: Option<i16>| value.filter(|value| *value > 0).map(f32::from);
        let line = |metrics: ttf_parser::LineMetrics| {
            (f32::from(metrics.position), f32::from(metrics.thickness))
        };
        let advance = (' '..='~')
            .filter_map(|ch| face.glyph_hor_advance(face.glyph_index(ch)?))
            .max()
            .map_or(f32::from(face.units_per_em()) * 0.6, f32::from);
        Self {
            units_per_em: f32::from(face.units_per_em().max(1)),
            ascent,
            descent,
            line_gap,
            x_height: positive(os2.and_then(|os2| os2.x_height())).or_else(|| glyph_height('x')),
            cap_height: positive(os2.and_then(|os2| os2.capital_height()))
                .or_else(|| glyph_height('H')),
            underline: tables.post.map(|post| line(post.underline_metrics)),
            strikeout: os2.map(|os2| line(os2.strikeout_metrics())),
            advance,
        }
    }

    /// What stock GPUI reports, with Ghostty's approximations for the rest.
    fn from_text_system(text: &TextSystem, id: FontId) -> Self {
        let units_per_em = text.units_per_em(id).max(1) as f32;
        let em = px(units_per_em);
        let positive = |value: Pixels| Some(f32::from(value)).filter(|value| *value > 0.);
        Self {
            units_per_em,
            ascent: f32::from(text.ascent(id, em)),
            descent: -f32::from(text.descent(id, em)).abs(),
            line_gap: 0.,
            x_height: positive(text.x_height(id, em)),
            cap_height: positive(text.cap_height(id, em)),
            underline: None,
            strikeout: None,
            advance: (' '..='~')
                .filter_map(|ch| text.advance(id, em, ch).ok())
                .map(|advance| f32::from(advance.width))
                .reduce(f32::max)
                .unwrap_or(units_per_em * 0.6),
        }
    }

    /// The grid for this face, all in device pixels until the final division by `scale`.
    fn cell(&self, font_size: f32, scale: f32, line_height: f32, cell_width: f32) -> CellMetrics {
        // FreeType uses integer y_ppem for metrics; CoreText uses the requested size.
        let em = if CORETEXT {
            font_size * scale
        } else {
            (font_size * scale).round().max(1.)
        };
        let unit = em / self.units_per_em;
        let (ascent, descent, gap) = (self.ascent, self.descent, self.line_gap);
        let face_height = (ascent - descent + gap) * unit;
        let height = face_height.round().max(1.);
        let baseline_bottom =
            (gap * unit / 2. - descent * unit - (height - face_height) / 2.).round();
        let baseline = height - baseline_bottom;
        let face_baseline = gap * unit / 2. - descent * unit;
        let (height, top) = adjusted_cell_height(
            height,
            face_height,
            baseline_bottom - face_baseline,
            line_height,
        );
        // A cell taller than the PTY can report is cut to the tallest it can, about the face's
        // centre, so the painted cell and the PTY's cell stay the same size.
        let max = f32::from(Geometry::MAX_CELL_PIXELS);
        let cut = (height - max).max(0.);
        let height = height - cut;
        let baseline = (baseline + top - (cut / 2.).floor()).clamp(1., height);
        let ex = self
            .x_height
            .unwrap_or_else(|| self.cap_height.unwrap_or(ascent * 0.75) * 0.75);
        let underline_thickness = self
            .underline
            .filter(|(_, thickness)| *thickness > 0.)
            .map_or(ex * unit * 0.15, |(_, thickness)| thickness * unit);
        let strike_thickness = self
            .strikeout
            .filter(|(_, thickness)| *thickness > 0.)
            .map_or(underline_thickness, |(_, thickness)| thickness * unit);
        let underline_position = self
            .underline
            .filter(|(position, thickness)| *thickness != 0. || *position != 0.)
            .map_or(-underline_thickness, |(position, _)| position * unit);
        let strike_position = self
            .strikeout
            .filter(|(position, thickness)| *thickness != 0. || *position != 0.)
            .map_or((ex * unit + strike_thickness) * 0.5, |(position, _)| {
                position * unit
            });
        let face_width = self.advance * font_size * scale / self.units_per_em;
        // As Ghostty's adjust-cell-width: scale the rounded cell; glyph_x re-centres the face.
        let width = adjusted_cell_width(face_width.round().max(1.), cell_width).min(max);
        let underline_thickness = underline_thickness.ceil().clamp(1., height);
        let strike_thickness = strike_thickness.ceil().clamp(1., height);
        let logical = |device: f32| px(device / scale);
        CellMetrics {
            size: size(logical(width), logical(height)),
            glyph_x: logical(glyph_x(width, face_width)),
            baseline: logical(baseline),
            underline: logical(
                (baseline - underline_position)
                    .round()
                    .clamp(0., height - underline_thickness),
            ),
            underline_thickness: logical(underline_thickness),
            strike: logical(
                (baseline - strike_position)
                    .round()
                    .clamp(0., height - strike_thickness),
            ),
            strike_thickness: logical(strike_thickness),
            scale,
        }
    }
}

// Ghostty's `Metrics` modifier: the cell times (1 + percent / 100) in f64, rounded. Taking the
// product in f32 from the multiplier can land on the other side of a half pixel (30 px at 1.05).
fn adjusted_cell_width(width: f32, multiplier: f32) -> f32 {
    let percent = ((f64::from(multiplier) - 1.) * 100. * 1e4).round() / 1e4;
    ((f64::from(width) * (1. + percent / 100.)).round() as f32).max(1.)
}

// Ghostty puts an odd extra pixel on the edge that best centres the face.
fn adjusted_cell_height(height: f32, face_height: f32, face_y: f32, multiplier: f32) -> (f32, f32) {
    let adjusted = (height * multiplier).round().max(1.);
    let half = (adjusted - height) / 2.;
    let top = if face_y - (height - face_height) / 2. > 0. {
        half.ceil()
    } else {
        half.floor()
    };
    (adjusted, top)
}

/// Ghostty's re-centring of glyphs in their cells (`renderGlyph`), in device pixels: half the
/// cell's difference from the face's unrounded advance. CoreText (`font/face/coretext.zig`)
/// keeps, when the cell is the narrower, only the part under a whole pixel, so glyphs keep
/// their subpixel phase without sliding a pixel left; FreeType (`font/face/freetype.zig`)
/// moves only into a wider cell, by whole pixels, to keep hinting.
fn glyph_x(cell_width: f32, face_width: f32) -> f32 {
    rasterizer_glyph_x(cell_width, face_width, CORETEXT)
}

/// Whether glyphs rasterize with CoreText, as on macOS, rather than FreeType.
const CORETEXT: bool = cfg!(target_os = "macos");

fn rasterizer_glyph_x(cell_width: f32, face_width: f32, coretext: bool) -> f32 {
    let dx = (cell_width - face_width) / 2.;
    if coretext {
        if dx < 0. { dx - dx.trunc() } else { dx }
    } else if dx > 0. {
        dx.round()
    } else {
        0.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JETBRAINS_MONO: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");

    fn jetbrains_mono() -> FaceMetrics {
        FaceMetrics::from_face(&ttf_parser::Face::parse(JETBRAINS_MONO, 0).unwrap())
    }

    /// A database holding the bundled face renamed to each of `families`.
    fn database_with(families: &[&str]) -> fontdb::Database {
        let mut source = fontdb::Database::new();
        source.load_font_source(fontdb::Source::Binary(std::sync::Arc::new(JETBRAINS_MONO)));
        let template = source.faces().next().unwrap().clone();
        let mut fonts = fontdb::Database::new();
        for family in families {
            let mut face = template.clone();
            face.families = vec![(family.to_string(), fontdb::Language::English_UnitedStates)];
            fonts.push_face_info(face);
        }
        fonts
    }

    /// An installed family GPUI also lists in its fallback stack is still installed, and a
    /// missing one isn't, whatever else the machine has.
    #[test]
    fn installed_families_are_found_in_fontdb_even_when_gpui_lists_them() {
        let installed = database_with(&["Noto Sans", "Helvetica", "Liberation Mono"]);
        for family in ["Noto Sans", "Helvetica", "Liberation Mono"] {
            assert!(database_has_family(&installed, family), "{family}");
        }
        assert!(!database_has_family(&installed, "No Such Font"));
        assert!(!database_has_family(&database_with(&[]), "Noto Sans"));
        let liberation_only = database_with(&["Liberation Sans", "Liberation Mono"]);
        for family in ["Noto Sans", "Helvetica"] {
            assert!(!database_has_family(&liberation_only, family), "{family}");
        }
    }

    /// Ghostty's grid for JetBrains Mono (1000 units per em, hhea 1020/-300/0, 600 advance,
    /// x-height 550, underline -155/50, strikeout 320/50), worked by hand in device pixels.
    #[test]
    fn jetbrains_mono_matches_ghostty() {
        let face = jetbrains_mono();
        assert_eq!(
            face,
            FaceMetrics {
                units_per_em: 1000.,
                ascent: 1020.,
                descent: -300.,
                line_gap: 0.,
                x_height: Some(550.),
                cap_height: Some(730.),
                underline: Some((-155., 50.)),
                strikeout: Some((320., 50.)),
                advance: 600.,
            }
        );
        // (size, scale, line height, cell width) → device (width, height, baseline, glyph_x,
        // underline top, underline thickness, strike top).
        // glyph_x differs by rasterizer: FreeType moves whole pixels into a wider cell only,
        // CoreText keeps the subpixel offset.
        let gx = |freetype: f32, coretext: f32| if CORETEXT { coretext } else { freetype };
        #[rustfmt::skip]
        let cases = [
            // 13 px: face 7.8 × 17.16 rounds to 8 × 17; baseline 4 px up from the bottom.
            ((13., 1., 1., 1.), (8., 17., 13., gx(0., 0.1), 15., 1., 9.)),
            // 13 px at 2×: 15.6 × 34.32 → 16 × 34, thickness 1.3 → 2.
            ((13., 2., 1., 1.), (16., 34., 26., gx(0., 0.2), 30., 2., 18.)),
            // 14 px: 8.4 × 18.48 → 8 × 18; a narrower cell never shifts glyphs on FreeType.
            ((14., 1., 1., 1.), (8., 18., 14., gx(0., -0.2), 16., 1., 10.)),
            // 16 px at 1.25×: em 20 → 12 × 26.4 → 12 × 26.
            ((16., 1.25, 1., 1.), (12., 26., 20., 0., 23., 1., 14.)),
            // Line height 1.2: 17 → 20.4 → 20, the odd pixel goes on top.
            ((13., 1., 1.2, 1.), (8., 20., 15., gx(0., 0.1), 17., 1., 11.)),
            // Cell width 1.1: 8 → 8.8 → 9; glyphs move right by round(0.6) = 1 on FreeType.
            ((13., 1., 1., 1.1), (9., 17., 13., gx(1., 0.6), 15., 1., 9.)),
        ];
        for ((font_size, scale, line_height, cell_width), expected) in cases {
            let cell = face.cell(font_size, scale, line_height, cell_width);
            let device = |value: Pixels| f32::from(value) * scale;
            let actual = (
                device(cell.size.width),
                device(cell.size.height),
                device(cell.baseline),
                device(cell.glyph_x),
                device(cell.underline),
                device(cell.underline_thickness),
                device(cell.strike),
            );
            let close = |a: f32, b: f32| (a - b).abs() < 1e-3;
            assert!(
                close(actual.0, expected.0)
                    && close(actual.1, expected.1)
                    && close(actual.2, expected.2)
                    && close(actual.3, expected.3)
                    && close(actual.4, expected.4)
                    && close(actual.5, expected.5)
                    && close(actual.6, expected.6),
                "{font_size} px at {scale}×, {line_height}/{cell_width}: {actual:?} != {expected:?}"
            );
            assert_eq!(
                cell.device_size(),
                size(expected.0 as u32, expected.1 as u32)
            );
        }
    }

    #[test]
    fn missing_tables_use_ghostty_fallbacks() {
        let face = FaceMetrics {
            line_gap: 100.,
            x_height: None,
            cap_height: None,
            underline: None,
            strikeout: None,
            ..jetbrains_mono()
        };
        // 10 px: face (1020 + 300 + 100) / 100 = 14.2 → 14; the gap splits around the face.
        let cell = face.cell(10., 1., 1., 1.);
        assert_eq!(cell.size, size(px(6.), px(14.)));
        // baseline_bottom = round(0.5 + 3 + 0.1) = 4.
        assert_eq!(cell.baseline, px(10.));
        // ex = 1020 × 0.75 × 0.75 = 573.75 units; underline thickness 0.86 → 1 px, one
        // thickness below the baseline; strikeout centred on half the x-height.
        assert_eq!(cell.underline, px(11.));
        assert_eq!(cell.underline_thickness, px(1.));
        assert_eq!(cell.strike, px(7.));
    }

    #[test]
    fn multipliers_round_like_ghostty() {
        assert_eq!(adjusted_cell_width(28., 1.05), 29.);
        assert_eq!(adjusted_cell_width(20., 1.05), 21.);
        assert_eq!(adjusted_cell_width(8., 0.8), 6.);
        assert_eq!(adjusted_cell_height(17., 17.16, 0.1, 1.2), (20., 2.));
        assert_eq!(adjusted_cell_height(17., 17.16, -0.5, 1.2), (20., 1.));
    }

    /// Both rasterizers' re-centring, checked on every platform.
    #[test]
    fn glyph_x_recentres_glyphs_as_ghostty() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
        // FreeType: whole pixels, and only into a wider cell.
        assert!(close(rasterizer_glyph_x(9., 7.8, false), 1.));
        assert!(close(rasterizer_glyph_x(8., 7.8, false), 0.));
        assert!(close(rasterizer_glyph_x(8., 8.4, false), 0.));
        // CoreText: the exact offset into a wider cell; only the subpixel part into a narrower.
        assert!(close(rasterizer_glyph_x(9., 7.8, true), 0.6));
        assert!(close(rasterizer_glyph_x(8., 7.8, true), 0.1));
        assert!(close(rasterizer_glyph_x(8., 8.4, true), -0.2));
        assert!(close(rasterizer_glyph_x(6., 9., true), -0.5));
        assert!(close(rasterizer_glyph_x(5., 9., true), 0.));
    }

    /// The PTY reports at most 100-pixel cells, so a cell measured larger is cut to 100 for
    /// painting too: a 72 px face at line height 2 measures 190 px tall.
    #[test]
    fn cells_past_the_pty_limit_are_cut_to_it() {
        let face = jetbrains_mono();
        let tall = face.cell(72., 1., 2., 1.);
        assert_eq!(tall.device_size().height, 100);
        assert_eq!(tall.size.height, px(100.));
        assert!(tall.baseline <= tall.size.height);
        assert!(tall.underline + tall.underline_thickness <= tall.size.height);
        assert!(tall.strike + tall.strike_thickness <= tall.size.height);
        // 72 px at 2× with cell width 2: 86.4 → 86 → 172 px wide, cut to 100.
        let wide = face.cell(72., 2., 1., 2.);
        assert_eq!(wide.device_size(), size(100, 100));
        assert_eq!(wide.size, size(px(50.), px(50.)));
        // The PTY gets exactly the painted cell, and the grid counts painted cells.
        let grid = tall.grid(size(px(1000.), px(500.)));
        assert_eq!(
            (grid.cols, grid.rows, grid.cell_width, grid.cell_height),
            (23, 5, 43, 100)
        );
        // Just under the limit is untouched: 50 px at line height 1 is 66 px tall.
        assert_eq!(face.cell(50., 1., 1., 1.).device_size().height, 66);
    }

    #[test]
    fn snapping_and_device_sizes() {
        let cell = jetbrains_mono().cell(13., 1.5, 1., 1.);
        // 13 × 1.5 = 19.5 → em 20 (FreeType): 11.7 → 12 × 26.4 → 26 device pixels.
        assert_eq!(cell.device_size(), size(12, 26));
        assert_eq!(cell.size, size(px(8.), px(26. / 1.5)));
        let close = |a: Point<Pixels>, x: f32, y: f32| {
            (f32::from(a.x) - x).abs() < 1e-4 && (f32::from(a.y) - y).abs() < 1e-4
        };
        assert!(close(cell.snap(point(px(10.1), px(3.4))), 10., 5. / 1.5));
        assert!(close(
            cell.cell_origin(point(px(1.), px(2.)), 2, 3),
            25.,
            2. + 52. / 1.5
        ));
    }
}
