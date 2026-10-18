//! Library screen: breadcrumbs, `# Title Time` listing of the browser's
//! current directory, with queued markers and known durations.

use std::collections::HashSet;
use std::path::PathBuf;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use sointty_core::{CueRange, QueueEntry};

use crate::browser::{Browser, BrowserItemKind};
use crate::display;
use crate::screens::{MetadataView, visible_window};
use crate::theme::Theme;
use crate::widgets;

/// Live-queue membership key: a track counts as queued when both path and
/// CUE range match a live entry.
pub fn queue_key(entry: &QueueEntry) -> (PathBuf, Option<CueRange>) {
    (entry.path.clone(), entry.cue_range)
}

/// Render the Library list into `area`.
pub fn render<M: MetadataView>(
    frame: &mut Frame<'_>,
    area: Rect,
    browser: &Browser,
    focused: bool,
    queued: &HashSet<(PathBuf, Option<CueRange>)>,
    meta: &M,
    theme: &Theme,
) {
    let block = widgets::panel("Library", Some(browser.items.len()), theme, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Breadcrumbs.
    lines.push(Line::from(Span::styled(
        widgets::truncate_cells(
            &browser.current_dir.display().to_string(),
            inner.width as usize,
        ),
        theme.faint_style(),
    )));
    // Column header and divider.
    if inner.height >= 3 {
        let width = inner.width as usize;
        let header = format!("  # {:<width$}", "Title", width = width.saturating_sub(10));
        let header = format!("{}{}", header, "Time");
        lines.push(Line::from(Span::styled(
            widgets::truncate_cells(&header, width),
            theme.dimmed(),
        )));
        lines.push(Line::from(Span::styled(
            "─".repeat(width),
            theme.border_style(),
        )));
    }
    let rows_height = inner.height as usize - lines.len().min(inner.height as usize);
    let (start, end) = visible_window(browser.items.len(), browser.selected, rows_height);
    for (index, item) in browser.items[start..end].iter().enumerate() {
        let index = start + index;
        let selected = focused && index == browser.selected;
        let width = inner.width as usize;
        match item.kind {
            BrowserItemKind::Dir => {
                let name = format!("{}/", item.name);
                let line = if selected {
                    Line::from(vec![
                        Span::styled("› ", theme.accent_style()),
                        Span::styled(
                            widgets::truncate_cells(&name, width.saturating_sub(2)),
                            theme.selected(),
                        ),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled("  ", theme.base()),
                        Span::styled(
                            widgets::truncate_cells(&name, width.saturating_sub(2)),
                            theme.strong(),
                        ),
                    ])
                };
                lines.push(line);
            }
            BrowserItemKind::Playlist => {
                let line = if selected {
                    Line::from(vec![
                        Span::styled("› ", theme.accent_style()),
                        Span::styled(
                            widgets::truncate_cells(&item.name, width.saturating_sub(2)),
                            theme.selected(),
                        ),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled("≡ ", theme.accent_style()),
                        Span::styled(
                            widgets::truncate_cells(&item.name, width.saturating_sub(2)),
                            theme.dimmed(),
                        ),
                    ])
                };
                lines.push(line);
            }
            BrowserItemKind::Audio => {
                let metadata = meta.metadata(&item.path);
                let (title, tag) =
                    display::display_title(&item.path, metadata.map(|meta| &meta.tags));
                let entry = QueueEntry::from(item.path.clone());
                let is_queued = queued.contains(&queue_key(&entry));
                let marker_style = if selected {
                    theme.accent_style()
                } else if is_queued {
                    theme.ok_style()
                } else {
                    theme.base()
                };
                let number = metadata
                    .and_then(|meta| meta.track_number)
                    .or_else(|| display::fallback_track_number(&item.path));
                let marker = format!(
                    "{}{:>3} ",
                    if selected {
                        "› "
                    } else if is_queued {
                        "• "
                    } else {
                        "  "
                    },
                    number.map(|number| number.to_string()).unwrap_or_default(),
                );
                let duration = metadata.and_then(|meta| meta.duration.map(crate::screens::clock));
                let row = widgets::track_row(
                    width,
                    &marker,
                    marker_style,
                    &title,
                    tag.as_deref(),
                    duration.as_deref(),
                    theme,
                );
                lines.push(if selected {
                    row.style(theme.selected())
                } else {
                    row
                });
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
