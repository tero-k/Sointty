//! Settings screen: sections on the left, rows on the right. All values are
//! prepared by the app; this module only lays them out.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::keymap::Focus;
use crate::screens::visible_window;
use crate::theme::Theme;
use crate::widgets;

/// One settings row in the active section.
pub struct SettingsRowView {
    pub label: String,
    pub value: String,
    /// Explanatory note (e.g. fidelity implications); dim.
    pub note: Option<String>,
    /// Conversion or mismatch warning; accent.
    pub warning: Option<String>,
}

/// View data for one frame of the Settings screen.
pub struct SettingsView<'a> {
    pub sections: &'a [&'a str],
    pub section_sel: usize,
    pub rows: &'a [SettingsRowView],
    pub row_sel: usize,
    pub focus: Focus,
    pub config_path: Option<&'a str>,
    /// Startup config parse error; a failed later save keeps this visible.
    pub config_error: Option<&'a str>,
    /// Output section: decoder source → negotiated output → device.
    pub signal_path: Option<&'a str>,
}

/// Render the Settings screen; narrow terminals show only the focused pane.
pub fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &SettingsView<'_>,
    theme: &Theme,
    narrow: bool,
) {
    let (sections_area, rows_area) = if narrow {
        match view.focus {
            Focus::SettingsSections => (area, Rect { width: 0, ..area }),
            _ => (Rect { width: 0, ..area }, area),
        }
    } else {
        crate::layout::split_columns(area, 24)
    };
    if sections_area.width > 0 {
        render_sections(frame, sections_area, view, theme);
    }
    if rows_area.width > 0 {
        render_rows(frame, rows_area, view, theme);
    }
}

fn render_sections(frame: &mut Frame<'_>, area: Rect, view: &SettingsView<'_>, theme: &Theme) {
    let focused = view.focus == Focus::SettingsSections;
    let block = widgets::panel("Settings", None, theme, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let width = inner.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, section) in view.sections.iter().enumerate() {
        let selected = index == view.section_sel;
        let marker = if selected && focused {
            "› "
        } else if selected {
            "* "
        } else {
            "  "
        };
        let line = Line::from(vec![
            Span::styled(
                marker,
                if selected {
                    theme.accent_style()
                } else {
                    theme.base()
                },
            ),
            Span::styled(
                widgets::truncate_cells(section, width.saturating_sub(2)),
                if selected {
                    theme.strong()
                } else {
                    theme.base()
                },
            ),
        ]);
        lines.push(if selected && focused {
            line.style(theme.selected())
        } else {
            line
        });
    }
    if let Some(path) = view.config_path {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            widgets::truncate_cells(path, width),
            theme.faint_style(),
        )));
    }
    if let Some(error) = view.config_error {
        lines.push(Line::from(Span::styled(
            widgets::truncate_cells(&format!("config not loaded: {error}"), width),
            theme.accent_style(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_rows(frame: &mut Frame<'_>, area: Rect, view: &SettingsView<'_>, theme: &Theme) {
    let focused = view.focus == Focus::SettingsRows;
    let section = view
        .sections
        .get(view.section_sel)
        .copied()
        .unwrap_or("Settings");
    let block = widgets::panel(section, None, theme, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let width = inner.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    if view.section_sel == 0 {
        lines.push(Line::from(Span::styled(
            widgets::truncate_cells(view.signal_path.unwrap_or("No active signal path"), width),
            theme.faint_style(),
        )));
    }
    let rows_height = (inner.height as usize).saturating_sub(lines.len());
    let (start, end) = visible_window(view.rows.len(), view.row_sel, rows_height);
    for (offset, row) in view.rows[start..end].iter().enumerate() {
        let index = start + offset;
        let selected = focused && index == view.row_sel;
        let marker = if selected { "› " } else { "  " };
        let value_budget = width.saturating_sub(widgets::text_cells(&row.label) + 4);
        let line = Line::from(vec![
            Span::styled(
                marker,
                if selected {
                    theme.accent_style()
                } else {
                    theme.base()
                },
            ),
            Span::styled(
                widgets::truncate_cells(&row.label, width.saturating_sub(4)),
                if selected {
                    theme.strong()
                } else {
                    theme.base()
                },
            ),
            Span::styled("  ", theme.base()),
            Span::styled(
                widgets::truncate_cells(&row.value, value_budget),
                theme.dimmed(),
            ),
        ]);
        lines.push(if selected {
            line.style(theme.selected())
        } else {
            line
        });
        if let Some(warning) = &row.warning {
            lines.push(Line::from(Span::styled(
                widgets::truncate_cells(&format!("    {warning}"), width),
                theme.accent_style(),
            )));
        }
        if let Some(note) = &row.note {
            lines.push(Line::from(Span::styled(
                widgets::truncate_cells(&format!("    {note}"), width),
                theme.faint_style(),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
