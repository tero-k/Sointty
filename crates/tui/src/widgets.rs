//! Small shared render helpers for the redesign surfaces. These are plain
//! functions producing ratatui lines/blocks, not a widget framework.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

use crate::theme::Theme;

/// Estimated terminal cell width of one character. Conservative: common
/// wide ranges (CJK, fullwidth, wide emoji planes) count as 2 cells,
/// control characters as 0, everything else as 1. Never panics; truncation
/// based on this estimate never exceeds the cell budget.
fn char_cells(ch: char) -> usize {
    let cp = ch as u32;
    if cp < 0x20 || (0x7F..0xA0).contains(&cp) {
        return 0;
    }
    let wide = matches!(cp,
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x20000..=0x2FFFD
            | 0x30000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

/// Estimated terminal cell width of a string.
pub fn text_cells(text: &str) -> usize {
    text.chars().map(char_cells).sum()
}

/// Truncate to at most `max_cells` terminal cells, appending `…` when text
/// was cut. The result never exceeds `max_cells` by the estimate above.
pub fn truncate_cells(text: &str, max_cells: usize) -> String {
    if text_cells(text) <= max_cells {
        return text.to_owned();
    }
    if max_cells == 0 {
        return String::new();
    }
    let budget = max_cells.saturating_sub(1); // room for the ellipsis
    let mut out = String::new();
    let mut cells = 0;
    for ch in text.chars() {
        let width = char_cells(ch);
        if cells + width > budget {
            break;
        }
        out.push(ch);
        cells += width;
    }
    out.push('…');
    out
}

/// Rounded panel with an inset title, an optional ` (count)` suffix, and an
/// accent border when focused.
pub fn panel(title: &str, count: Option<usize>, theme: &Theme, focused: bool) -> Block<'static> {
    let title = match count {
        Some(count) => format!(" {title} ({count}) "),
        None => format!(" {title} "),
    };
    let border_style = if focused {
        theme.accent_style()
    } else {
        theme.border_style()
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(Span::styled(title, theme.strong()))
        .style(Style::default().bg(theme.bg))
}

/// One footer line of `key label` pairs in keycap style. Pairs that would
/// overflow the width are dropped from the end of the visible set; `?` (all
/// keys) is kept whenever it fits.
pub fn keycap_line(pairs: &[(&str, &str)], theme: &Theme, width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (key, label) in pairs {
        let needed = text_cells(key) + text_cells(label) + 4; // spaces around keycap
        if used + needed > width && !spans.is_empty() {
            break;
        }
        spans.push(Span::styled(format!(" {key} "), theme.keycap()));
        spans.push(Span::styled(format!(" {label}  "), theme.dimmed()));
        used += needed;
    }
    Line::from(spans)
}

/// Render the keycap footer into `area` (one row; extra rows stay blank).
pub fn render_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    pairs: &[(&str, &str)],
    theme: &Theme,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let line = keycap_line(pairs, theme, area.width as usize);
    frame.render_widget(
        ratatui::widgets::Paragraph::new(line).style(theme.base()),
        Rect { height: 1, ..area },
    );
}

/// A progress bar line: elapsed `━` in accent, remainder `─` in faint,
/// filling exactly `width` cells. `ratio` is `None` for unknown duration.
pub fn progress_line(ratio: Option<f64>, width: usize, theme: &Theme) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let filled = match ratio {
        Some(ratio) => ((ratio.clamp(0.0, 1.0) * width as f64).round() as usize).min(width),
        None => 0,
    };
    Line::from(vec![
        Span::styled("━".repeat(filled), theme.accent_style()),
        Span::styled("─".repeat(width - filled), theme.faint_style()),
    ])
}

/// One track row: marker, title, optional dim tag, and a right-aligned
/// duration (`--:--` when unknown). Title and tag are truncated so the row
/// fits `width` cells.
pub fn track_row(
    width: usize,
    marker: &str,
    marker_style: Style,
    title: &str,
    tag: Option<&str>,
    time: Option<&str>,
    theme: &Theme,
) -> Line<'static> {
    let time = time.unwrap_or("--:--");
    let time_cells = text_cells(time);
    let marker_cells = text_cells(marker);
    let mut spans = vec![Span::styled(marker.to_owned(), marker_style)];
    // The title gets first claim on the space left after marker and time.
    let title_budget = width.saturating_sub(marker_cells + time_cells + 2);
    let title = truncate_cells(title, title_budget);
    let title_cells = text_cells(&title);
    spans.push(Span::styled(title, theme.base()));
    // The tag takes whatever the title left behind (or nothing).
    let mut used = marker_cells + title_cells;
    if let Some(tag) = tag {
        let tag_budget = width.saturating_sub(used + time_cells + 3);
        if tag_budget >= 5 {
            let rendered = truncate_cells(&format!(" ({tag})"), tag_budget);
            used += text_cells(&rendered);
            spans.push(Span::styled(rendered, theme.dimmed()));
        }
    }
    let padding = width.saturating_sub(used + time_cells);
    spans.push(Span::raw(" ".repeat(padding)));
    spans.push(Span::styled(time.to_owned(), theme.dimmed()));
    Line::from(spans)
}

/// A one-line signal path: `segment → segment → segment` with dim arrows.
pub fn signal_path_line<'a>(segments: &[(&'a str, Style)], theme: &Theme) -> Line<'a> {
    let mut spans = Vec::with_capacity(segments.len() * 2);
    for (index, (text, style)) in segments.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" → ", theme.faint_style()));
        }
        spans.push(Span::styled((*text).to_owned(), *style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn theme() -> Theme {
        Theme::build(true, false, None).0
    }

    #[test]
    fn truncation_respects_cell_budget() {
        assert_eq!(truncate_cells("hello", 10), "hello");
        assert_eq!(truncate_cells("hello world", 8), "hello w…");
        assert_eq!(truncate_cells("hello", 0), "");
        assert_eq!(truncate_cells("hello", 1), "…");
        // Wide characters count as two cells and are never split.
        assert_eq!(truncate_cells("日本語のタイトル", 8), "日本語…");
        assert!(text_cells("日本語のタイトル") > 8);
        // Control characters consume no cells, so the string fits as-is.
        assert_eq!(text_cells("a\u{7}b"), 2);
        assert_eq!(truncate_cells("a\u{7}b", 2), "a\u{7}b");
    }

    #[test]
    fn keycap_line_drops_overflowing_pairs() {
        let theme = theme();
        let pairs = [("space", "pause"), ("n", "next"), ("s", "stop"), ("?", "all keys")];
        let line = keycap_line(&pairs, &theme, 18);
        let text: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("space"), "{text}");
        assert!(!text.contains("all keys"), "{text}");
        let full = keycap_line(&pairs, &theme, 200);
        let text: String = full.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("all keys"), "{text}");
    }

    #[test]
    fn progress_line_fills_and_clamps() {
        let theme = theme();
        let half = progress_line(Some(0.5), 10, &theme);
        let text: String = half.spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text_cells(&text), 10);
        assert!(text.starts_with("━━━━━"), "{text}");
        let over = progress_line(Some(2.0), 6, &theme);
        let text: String = over.spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text_cells(&text), 6);
        let unknown = progress_line(None, 6, &theme);
        let text: String = unknown.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.chars().all(|ch| ch == '─'), "{text}");
        assert_eq!(progress_line(Some(0.5), 0, &theme).spans.len(), 0);
    }

    #[test]
    fn track_row_fits_width_with_unknown_time() {
        let theme = theme();
        let row = track_row(
            30,
            "› ",
            theme.accent_style(),
            "A Very Long Track Title That Overflows",
            Some("Live"),
            None,
            &theme,
        );
        let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text_cells(&text) <= 30, "{text}");
        assert!(text.ends_with("--:--"), "{text}");
    }
}
