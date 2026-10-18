//! Now playing surface: the four-row panel on the Library screen and the
//! mini player used elsewhere. Verdicts come from the engine's `Playing`
//! events only; nothing here infers format from output state.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme::Theme;
use crate::widgets;

/// The truthful stream verdict; `Converted` carries its direction label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackBadge {
    /// No current audible stream.
    Idle,
    /// The actual stream plays through the OS/system mixer; never
    /// bit-perfect even if no app-side sample packer converted it.
    Shared,
    BitPerfect,
    Converted,
}

/// Everything the Now playing surface needs for one frame.
pub struct PlayerView<'a> {
    pub badge: PlaybackBadge,
    pub title: &'a str,
    /// `artist · album` when known.
    pub subtitle: Option<&'a str>,
    pub elapsed: Duration,
    /// `None` shows `--:--`, never a fabricated countdown.
    pub total: Option<Duration>,
    pub paused: bool,
    /// Error/stall/underrun or plain status text.
    pub status: &'a str,
    pub source: Option<&'a str>,
    pub output: Option<&'a str>,
    pub buffers: Option<&'a str>,
    pub path: Option<&'a str>,
}

impl PlayerView<'_> {
    fn badge_spans(&self, theme: &Theme) -> (Span<'static>, Span<'static>) {
        match self.badge {
            PlaybackBadge::Idle => (
                Span::styled("○ ", theme.faint_style()),
                Span::styled("idle", theme.faint_style()),
            ),
            PlaybackBadge::BitPerfect => (
                Span::styled("● ", theme.ok_style()),
                Span::styled("bit-perfect", theme.ok_style()),
            ),
            PlaybackBadge::Shared => (
                Span::styled("● ", theme.accent_style()),
                Span::styled("shared", theme.accent_style()),
            ),
            PlaybackBadge::Converted => (
                Span::styled("● ", theme.accent_style()),
                Span::styled("converted", theme.accent_style()),
            ),
        }
    }
}

/// The wide four-row panel for the Library home screen.
pub fn render_panel(frame: &mut Frame<'_>, area: Rect, view: &PlayerView<'_>, theme: &Theme) {
    let block = widgets::panel("Now playing", None, theme, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let (left, right) = crate::layout::split_columns(inner, 55);
    let width = left.width as usize;

    let (dot, verdict) = view.badge_spans(theme);
    let title = if view.title.is_empty() {
        "nothing playing"
    } else {
        view.title
    };
    let title_budget = width.saturating_sub(
        4 + widgets::text_cells(verdict.content.as_ref()) + if view.paused { 8 } else { 0 },
    );
    let mut title_spans = vec![
        dot,
        Span::styled(widgets::truncate_cells(title, title_budget), theme.strong()),
        Span::raw("  "),
        verdict,
    ];
    if view.paused {
        title_spans.push(Span::styled("  paused", theme.dimmed()));
    }
    let total = view
        .total
        .map(crate::screens::clock)
        .unwrap_or_else(|| "--:--".to_owned());
    let position = format!("{} / {}", crate::screens::clock(view.elapsed), total);
    let bar_width = width.saturating_sub(crate::widgets::text_cells(&position) + 3);
    let ratio = view.total.and_then(|total| {
        (total.as_secs_f64() > 0.0).then(|| view.elapsed.as_secs_f64() / total.as_secs_f64())
    });
    let mut progress_spans = Vec::new();
    let mut bar = widgets::progress_line(ratio, bar_width, theme);
    progress_spans.append(&mut bar.spans);
    progress_spans.push(Span::raw(" "));
    progress_spans.push(Span::styled(position, theme.dimmed()));

    let subtitle = Line::from(Span::styled(
        widgets::truncate_cells(view.subtitle.unwrap_or(""), width),
        theme.dimmed(),
    ));
    let status = Line::from(Span::styled(
        widgets::truncate_cells(view.status, width),
        theme.faint_style(),
    ));
    let mut left_lines = vec![
        Line::from(title_spans),
        subtitle,
        Line::from(progress_spans),
        status,
    ];
    left_lines.truncate(inner.height as usize);
    frame.render_widget(
        Paragraph::new(left_lines),
        Rect {
            width: left.width,
            ..left
        },
    );

    let label = |text: &str| Span::styled(format!("{text}: "), theme.faint_style());
    let value = |text: Option<&str>| {
        Span::styled(
            widgets::truncate_cells(
                text.unwrap_or("unknown"),
                right.width.saturating_sub(9) as usize,
            ),
            theme.dimmed(),
        )
    };
    let mut right_lines = vec![
        Line::from(vec![label("Source"), value(view.source)]),
        Line::from(vec![label("Output"), value(view.output)]),
        Line::from(vec![label("Buffer"), value(view.buffers)]),
        Line::from(vec![label("Path"), value(view.path)]),
    ];
    right_lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(right_lines), right);
}

/// The one-line mini player used outside the Library screen.
pub fn render_mini(frame: &mut Frame<'_>, area: Rect, view: &PlayerView<'_>, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let (dot, verdict) = view.badge_spans(theme);
    let total = view
        .total
        .map(crate::screens::clock)
        .unwrap_or_else(|| "--:--".to_owned());
    let position = format!("{}/{}", crate::screens::clock(view.elapsed), total);
    let status = if view.paused {
        Some("paused")
    } else if view.status.starts_with("error")
        || matches!(view.status, "stalled" | "underrun" | "reconfiguring device")
    {
        Some(view.status)
    } else {
        None
    };
    let tail_width = crate::widgets::text_cells(&position)
        + crate::widgets::text_cells(verdict.content.as_ref())
        + status.map(crate::widgets::text_cells).unwrap_or(0)
        + if status.is_some() { 6 } else { 4 };
    let title = if view.title.is_empty() {
        "nothing playing"
    } else {
        view.title
    };
    let mut spans = vec![
        dot,
        Span::styled(
            widgets::truncate_cells(title, width.saturating_sub(tail_width)),
            theme.strong(),
        ),
        Span::raw("  "),
        Span::styled(position, theme.dimmed()),
        Span::raw("  "),
        verdict,
    ];
    if let Some(status) = status {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(status.to_owned(), theme.accent_style()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme.base()), area);
}
