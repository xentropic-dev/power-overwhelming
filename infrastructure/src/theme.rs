//! The built-in terminal catalog and the colors shared by every Power surface.

use anyhow::Result;
use ghostty_vt::{
    Terminal,
    style::{Palette, RgbColor},
    terminal::ColorScheme,
};
use serde::Deserialize;
use std::{collections::BTreeMap, sync::LazyLock};

pub const DEFAULT: &str = "VS Code Dark";
pub const DEFAULT_LIGHT: &str = "VS Code Light";

pub struct Scheme {
    pub name: String,
    pub aliases: Vec<String>,
    pub colors: Colors,
    /// Packed 0xRRGGBBAA, to retain translucent selections from the source scheme.
    pub selection_background: u32,
    pub selection_foreground: Option<u32>,
}

pub struct Colors {
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub indexed: [u32; 256],
    pub accent: u32,
    pub muted: u32,
    pub surface: u32,
    pub raised: u32,
    pub border: u32,
    pub link: u32,
    pub error: u32,
}

impl Colors {
    fn new(foreground: u32, background: u32, cursor: u32, indexed: [u32; 256]) -> Self {
        let accent = accessible(indexed[4], foreground, background);
        Self {
            foreground,
            background,
            cursor,
            indexed,
            accent,
            muted: accessible(blend(foreground, background, 30), foreground, background),
            surface: blend(background, foreground, 8),
            raised: blend(background, foreground, 17),
            border: blend(background, foreground, 28),
            link: accessible(indexed[12], foreground, background),
            error: accessible(indexed[1], foreground, background),
        }
    }

    pub fn readable(&self, color: u32, background: u32) -> u32 {
        accessible(color, self.contrast(background), background)
    }

    pub fn contrast(&self, background: u32) -> u32 {
        [self.foreground, self.background, 0x000000, 0xffffff]
            .into_iter()
            .find(|color| contrast_ratio(*color, background) >= 4.5)
            .unwrap_or(self.foreground)
    }
}

impl Scheme {
    pub fn is_light(&self) -> bool {
        luminance(self.colors.background) > luminance(self.colors.foreground)
    }

    pub(crate) fn terminal_mode(&self) -> ColorScheme {
        if self.is_light() {
            ColorScheme::Light
        } else {
            ColorScheme::Dark
        }
    }

    pub fn apply_to(&self, terminal: &mut Terminal) -> Result<()> {
        let colors = &self.colors;
        terminal
            .set_default_fg_color(Some(ghostty_rgb(colors.foreground)))?
            .set_default_bg_color(Some(ghostty_rgb(colors.background)))?
            .set_default_cursor_color(Some(ghostty_rgb(colors.cursor)))?
            .set_default_color_palette(Some(Palette(colors.indexed.map(ghostty_rgb))))?
            .set_color_scheme(Some(self.terminal_mode()));
        Ok(())
    }

    /// Terminal applications can override specific colors without changing Power's UI theme.
    pub fn terminal_ink(&self, terminal: &Terminal) -> (u32, u32, u32) {
        let effective = |value: ghostty_vt::error::Result<Option<RgbColor>>, default| {
            value.ok().flatten().map_or(default, from_ghostty)
        };
        (
            effective(terminal.fg_color(), self.colors.foreground),
            effective(terminal.bg_color(), self.colors.background),
            effective(terminal.cursor_color(), self.colors.cursor),
        )
    }
}

#[derive(Deserialize)]
struct Entry {
    metadata: Metadata,
    colors: Definition,
}

#[derive(Deserialize)]
struct Metadata {
    name: String,
    aliases: Vec<String>,
}

#[derive(Deserialize)]
struct Definition {
    foreground: String,
    background: String,
    ansi: [String; 8],
    brights: [String; 8],
    indexed: BTreeMap<u8, String>,
    cursor_bg: Option<String>,
    selection_bg: Option<String>,
    selection_fg: Option<String>,
}

impl From<Entry> for Scheme {
    fn from(entry: Entry) -> Self {
        let Definition {
            foreground,
            background,
            ansi,
            brights,
            indexed,
            cursor_bg,
            selection_bg,
            selection_fg,
        } = entry.colors;
        let mut palette = Palette::default().0.map(from_ghostty);
        for (index, color) in ansi.into_iter().chain(brights).enumerate() {
            palette[index] = rgb(&color);
        }
        for (index, color) in indexed {
            palette[usize::from(index)] = rgb(&color);
        }
        let foreground = rgb(&foreground);
        let background = rgb(&background);
        let colors = Colors::new(
            foreground,
            background,
            cursor_bg.as_deref().map_or(foreground, rgb),
            palette,
        );
        let selection_background = selection_bg
            .as_deref()
            .map_or_else(|| (blend(background, colors.accent, 24) << 8) | 0xff, rgba);
        Self {
            name: entry.metadata.name,
            aliases: entry.metadata.aliases,
            colors,
            selection_background,
            selection_foreground: selection_fg
                .as_deref()
                .map(rgba)
                .filter(|color| color & 0xff != 0),
        }
    }
}

static SCHEMES: LazyLock<Vec<Scheme>> = LazyLock::new(|| {
    [
        include_str!("../themes/vscode.json"),
        include_str!("../themes/wezterm.json"),
    ]
    .into_iter()
    .flat_map(|source| serde_json::from_str::<Vec<Entry>>(source).expect("valid bundled schemes"))
    .map(Scheme::from)
    .collect()
});

pub fn schemes() -> &'static [Scheme] {
    &SCHEMES
}

pub fn find(name: &str) -> Option<&'static Scheme> {
    let name = name.trim();
    schemes()
        .iter()
        .find(|scheme| scheme.name == name)
        .or_else(|| {
            schemes()
                .iter()
                .find(|scheme| scheme.name.eq_ignore_ascii_case(name))
        })
        .or_else(|| {
            schemes().iter().find(|scheme| {
                scheme
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(name))
            })
        })
}

pub fn default_scheme() -> &'static Scheme {
    &SCHEMES[0]
}

pub fn blend(from: u32, to: u32, percent: u32) -> u32 {
    let [_, r, g, b] = from.to_be_bytes();
    let [_, other_r, other_g, other_b] = to.to_be_bytes();
    let mix = |from: u8, to: u8| {
        ((u32::from(from) * (100 - percent) + u32::from(to) * percent) / 100) as u8
    };
    u32::from_be_bytes([0, mix(r, other_r), mix(g, other_g), mix(b, other_b)])
}

fn accessible(color: u32, foreground: u32, background: u32) -> u32 {
    (0..=10)
        .map(|step| blend(color, foreground, step * 10))
        .find(|color| contrast_ratio(*color, background) >= 4.5)
        .unwrap_or(foreground)
}

fn contrast_ratio(a: u32, b: u32) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn luminance(color: u32) -> f64 {
    let [_, r, g, b] = color.to_be_bytes();
    let linear = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

fn rgb(color: &str) -> u32 {
    assert_eq!(color.len(), 7, "expected RGB in bundled color {color}");
    u32::from_str_radix(&color[1..], 16).expect("valid bundled RGB")
}

fn rgba(color: &str) -> u32 {
    if let Some(hex) = color.strip_prefix('#') {
        let value = u32::from_str_radix(hex, 16).expect("valid bundled RGB(A)");
        return if hex.len() == 6 {
            (value << 8) | 0xff
        } else {
            value
        };
    }
    // WezTerm also serializes translucent selection colors as CSS percentages.
    let channels: Vec<u8> = color
        .strip_prefix("rgba(")
        .and_then(|s| s.strip_suffix(')'))
        .expect("valid bundled rgba()")
        .split_ascii_whitespace()
        .map(|value| {
            let percent = value
                .strip_suffix('%')
                .expect("percentage")
                .parse::<f64>()
                .expect("number");
            (percent * 255.0 / 100.0).round() as u8
        })
        .collect();
    u32::from_be_bytes(channels.try_into().expect("four rgba() channels"))
}

fn ghostty_rgb(color: u32) -> RgbColor {
    let [_, r, g, b] = color.to_be_bytes();
    RgbColor { r, g, b }
}

fn from_ghostty(color: RgbColor) -> u32 {
    u32::from_be_bytes([0, color.r, color.g, color.b])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostty_vt::terminal::Options;

    #[test]
    fn all_bundled_palettes_load_and_have_distinct_names() {
        let names: std::collections::BTreeSet<_> =
            schemes().iter().map(|scheme| &scheme.name).collect();
        assert_eq!(names.len(), 1003);
        assert_eq!(default_scheme().name, DEFAULT);
        assert!(find(DEFAULT_LIGHT).unwrap().is_light());
        assert_eq!(find("Dark 2026").unwrap().name, DEFAULT);
    }

    #[test]
    fn palette_updates_preserve_explicit_osc_overrides_and_update_indexed_defaults() {
        let first = default_scheme();
        let second = find("Catppuccin Latte").unwrap();
        let mut terminal = Terminal::new(Options::default()).unwrap();
        first.apply_to(&mut terminal).unwrap();
        terminal.vt_write(b"\x1b]10;#123456\x07");
        second.apply_to(&mut terminal).unwrap();
        let (foreground, background, _) = second.terminal_ink(&terminal);
        assert_eq!(foreground, 0x123456);
        assert_eq!(background, second.colors.background);
        assert_eq!(
            terminal.color_palette().unwrap().0.map(from_ghostty),
            second.colors.indexed
        );
        terminal.vt_write(b"\x1b]110\x07");
        assert_eq!(second.terminal_ink(&terminal).0, second.colors.foreground);
    }
}
