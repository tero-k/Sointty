//! Playlists screen: names panel with dirty markers plus a tracks panel
//! with multi-select checkboxes and the move-mode drop line.

use std::collections::HashSet;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::keymap::Focus;
use crate::screens::visible_window;
use crate::theme::Theme;
use crate::widgets;

/// One row in the names panel.
pub struct NameRow {
    pub name: String,
    pub count: usize,
    pub dirty: bool,
    /// True for the catalog's selected (default) playlist.
    pub catalog_selected: bool,
    /// Known total duration; `None` when any track duration is unknown.
    pub duration: Option<Duration>,
}

/// One row in the tracks panel.
pub struct TrackRow {
    pub number: Option<u32>,
    pub title: String,
    pub album: Option<String>,
    pub duration: Option<Duration>,
}

/// View data for one frame of the Playlists screen.
pub struct PlaylistsView<'a> {
    pub names: &'a [NameRow],
    pub name_sel: usize,
    pub tracks: &'a [TrackRow],
    pub track_cursor: usize,
    pub checked: &'a HashSet<usize>,
    pub lifted: &'a HashSet<usize>,
    /// Selected playlist name for the tracks panel title.
    pub tracks_title: &'a str,
    pub tracks_dirty: bool,
    pub focus: Focus,
    pub catalog_path: Option<&'a str>,
    /// Read-only protection from a malformed catalog file.
    pub catalog_error: Option<&'a str>,
    /// Move mode: the drop gap index (0..=visible rows after lifting).
    pub move_gap: Option<usize>,
}

/// Render the Playlists screen into `area` (two panels side by side when
/// wide; the focused panel only when narrow).
pub fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &PlaylistsView<'_>,
    theme: &Theme,
    narrow: bool,
) {
    let (names_area, tracks_area) = if narrow {
        // One active pane: Tab navigates between them.
        match view.focus {
            Focus::PlaylistNames => (area, Rect { width: 0, ..area }),
            _ => (Rect { width: 0, ..area }, area),
        }
    } else {
        crate::layout::split_columns(area, 30)
    };
    if names_area.width > 0 {
        render_names(frame, names_area, view, theme);
    }
    if tracks_area.width > 0 {
        render_tracks(frame, tracks_area, view, theme);
    }
}

fn render_names(frame: &mut Frame<'_>, area: Rect, view: &PlaylistsView<'_>, theme: &Theme) {
    let focused = view.focus == Focus::PlaylistNames;
    let dirty_count = view.names.iter().filter(|row| row.dirty).count();
    let title = if dirty_count > 0 {
        format!("Playlists · {dirty_count} unsaved")
    } else {
        "Playlists".to_owned()
    };
    let block = widgets::panel(&title, None, theme, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let width = inner.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(if focused { "› " } else { "  " }, theme.accent_style()),
        Span::styled("+ New playlist", theme.accent_style()),
    ]));
    let (start, end) = visible_window(
        view.names.len(),
        view.name_sel,
        (inner.height as usize).saturating_sub(3),
    );
    for (offset, row) in view.names[start..end].iter().enumerate() {
        let index = start + offset;
        let selected = focused && index == view.name_sel;
        let marker = if selected { "› " } else { "  " };
        let mut spans = vec![Span::styled(
            marker,
            if selected {
                theme.accent_style()
            } else {
                theme.base()
            },
        )];
        let name = widgets::truncate_cells(&row.name, width.saturating_sub(12));
        spans.push(Span::styled(name, theme.strong()));
        if row.catalog_selected {
            spans.push(Span::styled(" *", theme.faint_style()));
        }
        if row.dirty {
            spans.push(Span::styled(" ●", theme.accent_style()));
        }
        spans.push(Span::styled(format!(" ({})", row.count), theme.dimmed()));
        if let Some(duration) = row.duration {
            spans.push(Span::styled(
                format!(" {}", crate::screens::clock(duration)),
                theme.faint_style(),
            ));
        }
        let line = if selected {
            Line::from(spans).style(theme.selected())
        } else {
            Line::from(spans)
        };
        lines.push(line);
    }
    // Catalog path / read-only notice.
    if let Some(error) = view.catalog_error {
        lines.push(Line::from(Span::styled(
            widgets::truncate_cells(&format!("read-only: {error}"), width),
            theme.accent_style(),
        )));
    } else if let Some(path) = view.catalog_path {
        lines.push(Line::from(Span::styled(
            widgets::truncate_cells(path, width),
            theme.faint_style(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_tracks(frame: &mut Frame<'_>, area: Rect, view: &PlaylistsView<'_>, theme: &Theme) {
    let focused = view.focus == Focus::PlaylistTracks;
    let title = if view.tracks_dirty {
        format!("{} ●", view.tracks_title)
    } else {
        view.tracks_title.to_owned()
    };
    let block = widgets::panel(&title, Some(view.tracks.len()), theme, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let width = inner.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let moving = view.move_gap.is_some();
    if moving {
        lines.push(Line::from(Span::styled(
            " Move: ↑↓ choose drop position · Enter drop · Esc cancel",
            theme.accent_style().add_modifier(Modifier::BOLD),
        )));
    }
    // Header.
    if !moving || inner.height > 2 {
        lines.push(Line::from(Span::styled(
            widgets::truncate_cells("   [ ] # Title · Album · Time", width),
            theme.dimmed(),
        )));
    }
    let rows_height = (inner.height as usize).saturating_sub(lines.len() + usize::from(moving));
    let drop_before = view.move_gap.map(|gap| {
        view.tracks
            .iter()
            .enumerate()
            .filter(|(index, _)| !view.lifted.contains(index))
            .nth(gap)
            .map(|(index, _)| index)
            .unwrap_or(view.tracks.len())
    });
    let focus_row = drop_before
        .map(|index| index.min(view.tracks.len().saturating_sub(1)))
        .unwrap_or(view.track_cursor);
    let (start, end) = visible_window(view.tracks.len(), focus_row, rows_height);
    let mut remaining_before = (0..start)
        .filter(|index| !view.lifted.contains(index))
        .count();
    for (offset, row) in view.tracks[start..end].iter().enumerate() {
        let index = start + offset;
        // Gap is indexed after removing lifted rows, not by original row.
        if !view.lifted.contains(&index) && view.move_gap == Some(remaining_before) {
            lines.push(Line::from(Span::styled(
                "  ─ drop here ─",
                theme.accent_style(),
            )));
        }
        if !view.lifted.contains(&index) {
            remaining_before += 1;
        }
        let selected = focused && !moving && index == view.track_cursor;
        let checkbox = if view.checked.contains(&index) { "[x]" } else { "[ ]" };
        let number = row
            .number
            .map(|number| format!("{number:>2}"))
            .unwrap_or_else(|| "  ".to_owned());
        let duration = row
            .duration
            .map(crate::screens::clock)
            .unwrap_or_else(|| "--:--".to_owned());
        let title_budget = width.saturating_sub(3 + 4 + 3 + widgets::text_cells(&duration) + 2);
        let title = widgets::truncate_cells(&row.title, title_budget);
        let base = if view.lifted.contains(&index) {
            theme.faint_style().add_modifier(Modifier::ITALIC)
        } else {
            theme.base()
        };
        let mut spans = vec![
            Span::styled(
                if selected { "› " } else { "  " },
                if selected { theme.accent_style() } else { base },
            ),
            Span::styled(format!("{checkbox} "), base),
            Span::styled(format!("{number} "), base),
            Span::styled(title, base),
        ];
        if let Some(album) = &row.album {
            spans.push(Span::styled(
                widgets::truncate_cells(
                    &format!(" · {album}"),
                    width.saturating_sub(3 + 4 + 3 + widgets::text_cells(&duration) + 2),
                ),
                theme.dimmed(),
            ));
        }
        let used: usize = spans
            .iter()
            .map(|span| widgets::text_cells(&span.content))
            .sum();
        spans.push(Span::raw(" ".repeat(
            width.saturating_sub(used + widgets::text_cells(&duration)),
        )));
        spans.push(Span::styled(duration, theme.dimmed()));
        let line = if selected {
            Line::from(spans).style(theme.selected())
        } else {
            Line::from(spans)
        };
        lines.push(line);
    }
    if moving && end == view.tracks.len() && view.move_gap == Some(remaining_before) {
        lines.push(Line::from(Span::styled(
            "  ─ drop here ─",
            theme.accent_style(),
        )));
    }
    if view.tracks.is_empty() {
        lines.push(Line::from(Span::styled(
            "(empty: p from Library or Search adds tracks)",
            theme.faint_style(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
