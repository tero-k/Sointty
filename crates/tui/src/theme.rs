//! Central color tokens and terminal capability detection for the TUI.
//!
//! Truecolor terminals get the exact design tokens; 256-color terminals get
//! the nearest xterm-256 preset. Under `NO_COLOR` no foreground/background
//! colors are emitted at all: focus, playing and selection are conveyed with
//! markers and text modifiers instead.

use ratatui::style::{Color, Modifier, Style};

/// Design tokens from the redesign mockup, in sRGB.
pub mod tokens {
    pub const BG: (u8, u8, u8) = (0x0D, 0x0F, 0x12);
    pub const FG: (u8, u8, u8) = (0xD5, 0xD8, 0xDD);
    pub const FG_STRONG: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);
    pub const DIM: (u8, u8, u8) = (0x8A, 0x91, 0x9C);
    pub const FAINT: (u8, u8, u8) = (0x76, 0x7D, 0x88);
    pub const BORDER: (u8, u8, u8) = (0x2B, 0x30, 0x38);
    pub const SEL_BG: (u8, u8, u8) = (0x22, 0x28, 0x33);
    pub const PLAYING_BG: (u8, u8, u8) = (0x1A, 0x1E, 0x25);
    pub const KEYCAP_BG: (u8, u8, u8) = (0x26, 0x2B, 0x33);
    pub const ACCENT: (u8, u8, u8) = (0xE5, 0xA8, 0x4B);
    pub const OK: (u8, u8, u8) = (0x6C, 0xC9, 0xA1);
}

/// Default accent, applied when no valid `#RRGGBB` override is configured.
pub const DEFAULT_ACCENT: &str = "#E5A84B";

/// Parse an exact `#RRGGBB` color. Anything else (wrong length, non-hex
/// digits, missing hash) is rejected so callers can surface feedback and
/// fall back to [`DEFAULT_ACCENT`] without touching the stored value.
pub fn parse_hex_color(text: &str) -> Option<(u8, u8, u8)> {
    let hex = text.strip_prefix('#')?;
    let digits: Vec<char> = hex.chars().collect();
    if digits.len() != 6 {
        return None;
    }
    let value = |index: usize| -> Option<u8> {
        let high = digits[index].to_digit(16)?;
        let low = digits[index + 1].to_digit(16)?;
        Some((high * 16 + low) as u8)
    };
    Some((value(0)?, value(2)?, value(4)?))
}

/// Color set for one frame of the UI. Under `no_color` every field is
/// `Color::Reset`, so painting any of them emits no color escape codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub fg_strong: Color,
    pub dim: Color,
    pub faint: Color,
    pub border: Color,
    pub sel_bg: Color,
    pub playing_bg: Color,
    pub keycap_bg: Color,
    pub accent: Color,
    pub ok: Color,
    /// True under `NO_COLOR`: distinguish focus/playing/selection with
    /// markers and modifiers, never with colors.
    pub no_color: bool,
}

/// Nearest xterm-256 preset for each token (precomputed for the exact hex
/// values above).
const INDEXED: [u8; 11] = [233, 253, 231, 245, 243, 236, 235, 234, 235, 179, 79];

impl Theme {
    /// Build a theme from explicit capabilities. `accent` is the optional
    /// configured `#RRGGBB`; an invalid value falls back to the default and
    /// returns a warning string for the status line. The invalid configured
    /// value itself is never modified here.
    pub fn build(
        truecolor: bool,
        no_color: bool,
        accent: Option<&str>,
    ) -> (Theme, Option<String>) {
        let (accent_rgb, warning) = match accent {
            Some(text) => match parse_hex_color(text) {
                Some(rgb) => (rgb, None),
                None => (
                    tokens::ACCENT,
                    Some(format!(
                        "invalid accent {text:?} (want #RRGGBB); using {DEFAULT_ACCENT}"
                    )),
                ),
            },
            None => (tokens::ACCENT, None),
        };
        if no_color {
            let theme = Theme {
                bg: Color::Reset,
                fg: Color::Reset,
                fg_strong: Color::Reset,
                dim: Color::Reset,
                faint: Color::Reset,
                border: Color::Reset,
                sel_bg: Color::Reset,
                playing_bg: Color::Reset,
                keycap_bg: Color::Reset,
                accent: Color::Reset,
                ok: Color::Reset,
                no_color: true,
            };
            return (theme, warning);
        }
        let palette: [(u8, u8, u8); 11] = [
            tokens::BG,
            tokens::FG,
            tokens::FG_STRONG,
            tokens::DIM,
            tokens::FAINT,
            tokens::BORDER,
            tokens::SEL_BG,
            tokens::PLAYING_BG,
            tokens::KEYCAP_BG,
            accent_rgb,
            tokens::OK,
        ];
        let color = |index: usize| {
            if truecolor {
                let (r, g, b) = palette[index];
                Color::Rgb(r, g, b)
            } else if index == 9 && accent.is_some() && warning.is_none() {
                // A configured accent has no preset: quantize to the nearest
                // of the 240-entry color cube/grayscale space.
                let (r, g, b) = palette[index];
                nearest_indexed(r, g, b)
            } else {
                Color::Indexed(INDEXED[index])
            }
        };
        let theme = Theme {
            bg: color(0),
            fg: color(1),
            fg_strong: color(2),
            dim: color(3),
            faint: color(4),
            border: color(5),
            sel_bg: color(6),
            playing_bg: color(7),
            keycap_bg: color(8),
            accent: color(9),
            ok: color(10),
            no_color: false,
        };
        (theme, warning)
    }

    /// Detect capabilities from the process environment.
    pub fn detect(accent: Option<&str>) -> (Theme, Option<String>) {
        let truecolor = std::env::var("COLORTERM")
            .map(|value| {
                let value = value.to_ascii_lowercase();
                value.contains("truecolor") || value.contains("24bit")
            })
            .unwrap_or(false);
        let no_color = std::env::var_os("NO_COLOR")
            .is_some_and(|value| !value.is_empty());
        Self::build(truecolor, no_color, accent)
    }

    /// Base body text.
    pub fn base(&self) -> Style {
        Style::default().fg(self.fg).bg(self.bg)
    }

    /// Primary titles and values.
    pub fn strong(&self) -> Style {
        Style::default().fg(self.fg_strong).bg(self.bg)
    }

    /// Secondary labels.
    pub fn dimmed(&self) -> Style {
        Style::default().fg(self.dim).bg(self.bg)
    }

    /// Least-emphasis hints.
    pub fn faint_style(&self) -> Style {
        Style::default().fg(self.faint).bg(self.bg)
    }

    /// Accent (selection markers, playing row, active focus).
    pub fn accent_style(&self) -> Style {
        Style::default().fg(self.accent).bg(self.bg)
    }

    /// Success/bit-perfect marker.
    pub fn ok_style(&self) -> Style {
        Style::default().fg(self.ok).bg(self.bg)
    }

    /// Panel border.
    pub fn border_style(&self) -> Style {
        Style::default().fg(self.border).bg(self.bg)
    }

    /// Selected row. Without colors, reverse video carries the selection.
    pub fn selected(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(self.fg_strong).bg(self.sel_bg)
        }
    }

    /// The currently audible row. Without colors, bold carries it.
    pub fn playing(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.fg_strong).bg(self.playing_bg)
        }
    }

    /// Accent-colored playing marker (`▶`); bold without colors.
    pub fn playing_marker(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(self.accent)
                .bg(self.playing_bg)
                .add_modifier(Modifier::BOLD)
        }
    }

    /// A footer keycap like `q` in `q quit`.
    pub fn keycap(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.fg).bg(self.keycap_bg)
        }
    }
}

/// Nearest xterm-256 color for an arbitrary sRGB triple (color cube plus
/// grayscale ramp; the 16 ANSI colors are ignored, matching most terminals'
/// redefinition risk).
fn nearest_indexed(r: u8, g: u8, b: u8) -> Color {
    let cube_component = |value: u8| -> u8 {
        // Cube stops are 0, 95, 135, 175, 215, 255.
        if value < 48 {
            0
        } else {
            ((u16::from(value) - 35) / 40).min(5) as u8
        }
    };
    let to_cube = |component: u8| -> u16 {
        if component == 0 { 0 } else { 95 + 40 * u16::from(component - 1) }
    };
    let (cr, cg, cb) = (cube_component(r), cube_component(g), cube_component(b));
    let cube_dist = {
        let (dr, dg, db) = (
            i32::from(to_cube(cr)) - i32::from(r),
            i32::from(to_cube(cg)) - i32::from(g),
            i32::from(to_cube(cb)) - i32::from(b),
        );
        (dr * dr + dg * dg + db * db) as u32
    };
    let gray_level = {
        let average = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
        if average < 8 {
            0
        } else {
            ((average - 8) / 10).min(23) as u8
        }
    };
    let gray_value = 8 + 10 * i32::from(gray_level);
    let gray_dist = {
        let (dr, dg, db) = (
            gray_value - i32::from(r),
            gray_value - i32::from(g),
            gray_value - i32::from(b),
        );
        (dr * dr + dg * dg + db * db) as u32
    };
    if gray_dist < cube_dist {
        Color::Indexed(232 + gray_level)
    } else {
        Color::Indexed(16 + 36 * cr + 6 * cg + cb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_rrggbb_only() {
        assert_eq!(parse_hex_color("#E5A84B"), Some((0xE5, 0xA8, 0x4B)));
        assert_eq!(parse_hex_color("#000000"), Some((0, 0, 0)));
        assert_eq!(parse_hex_color("#ffffff"), Some((255, 255, 255)));
        assert_eq!(parse_hex_color("E5A84B"), None);
        assert_eq!(parse_hex_color("#E5A84"), None);
        assert_eq!(parse_hex_color("#E5A84BC"), None);
        assert_eq!(parse_hex_color("#GG0000"), None);
        assert_eq!(parse_hex_color(""), None);
        // Multibyte input must not panic or slice mid-char.
        assert_eq!(parse_hex_color("#€2345"), None);
    }

    #[test]
    fn truecolor_uses_exact_rgb() {
        let (theme, warning) = Theme::build(true, false, None);
        assert_eq!(warning, None);
        assert_eq!(theme.bg, Color::Rgb(0x0D, 0x0F, 0x12));
        assert_eq!(theme.accent, Color::Rgb(0xE5, 0xA8, 0x4B));
        assert_eq!(theme.ok, Color::Rgb(0x6C, 0xC9, 0xA1));
    }

    #[test]
    fn indexed_falls_back_to_presets() {
        let (theme, _) = Theme::build(false, false, None);
        assert_eq!(theme.bg, Color::Indexed(233));
        assert_eq!(theme.accent, Color::Indexed(179));
        assert_eq!(theme.ok, Color::Indexed(79));
    }

    #[test]
    fn invalid_accent_warns_and_keeps_default() {
        let (theme, warning) = Theme::build(true, false, Some("orange"));
        assert_eq!(theme.accent, Color::Rgb(0xE5, 0xA8, 0x4B));
        let warning = warning.expect("invalid accent must surface feedback");
        assert!(warning.contains("orange"), "{warning}");
        assert!(warning.contains(DEFAULT_ACCENT), "{warning}");
    }

    #[test]
    fn valid_accent_applies() {
        let (theme, warning) = Theme::build(true, false, Some("#FF0000"));
        assert_eq!(warning, None);
        assert_eq!(theme.accent, Color::Rgb(255, 0, 0));
    }

    #[test]
    fn no_color_emits_no_colors() {
        let (theme, _) = Theme::build(true, true, Some("#FF0000"));
        assert!(theme.no_color);
        for color in [
            theme.bg,
            theme.fg,
            theme.fg_strong,
            theme.dim,
            theme.faint,
            theme.border,
            theme.sel_bg,
            theme.playing_bg,
            theme.keycap_bg,
            theme.accent,
            theme.ok,
        ] {
            assert_eq!(color, Color::Reset);
        }
        // Distinction shifts to modifiers.
        assert!(theme.selected().add_modifier.contains(Modifier::REVERSED));
        assert!(theme.playing().add_modifier.contains(Modifier::BOLD));
    }
}
