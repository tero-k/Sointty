//! Queue screen: audible row, grouped pending rows, and a bounded UI-only
//! past-row history. Grouping and row building are pure functions.

use std::collections::HashSet;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use sointty_core::{QueueItem, TrackId};

use crate::display;
use crate::screens::{MetadataView, visible_window};
use crate::theme::Theme;
use crate::widgets;

/// At most this many past rows are kept for display.
pub const PAST_LIMIT: usize = 200;
/// Collapsed album groups show this many rows before a `… N more` stub.
pub const GROUP_VISIBLE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Past,
    Audible,
    Pending,
}

/// One rendered queue row.
#[derive(Debug, Clone, PartialEq)]
pub enum QueueRow {
    /// Album/artist group header (shared title suffix).
    Header { label: String },
    Track {
        id: TrackId,
        section: Section,
        title: String,
        tag: Option<String>,
        duration: Option<Duration>,
    },
    /// Collapsed remainder of a group.
    More { count: usize },
}

/// A stable group identity: the lowercase album+artist pair.
fn group_key<M: MetadataView>(item: &QueueItem, meta: &M) -> Option<String> {
    let metadata = meta.metadata(&item.entry.path)?;
    let album = metadata
        .tags
        .album
        .as_ref()
        .filter(|album| !album.is_empty())?;
    let artist = metadata.tags.artist.clone().unwrap_or_default();
    Some(format!(
        "{}|{}",
        album.to_lowercase(),
        artist.to_lowercase()
    ))
}

/// A bounded CUE span's duration comes from CD frames (75 Hz), never the
/// whole image file. An open-ended CUE span stays unknown.
pub fn entry_duration<M: MetadataView>(item: &QueueItem, meta: &M) -> Option<Duration> {
    match item.entry.cue_range {
        Some(range) => range
            .end_cd
            .map(|end| Duration::from_millis(end.saturating_sub(range.start_cd) * 1_000 / 75)),
        None => meta
            .metadata(&item.entry.path)
            .and_then(|meta| meta.duration),
    }
}

/// Build display rows for the queue. Adjacent pending entries with the same
/// album/artist collapse under one header; a group shows the first
/// [`GROUP_VISIBLE`] rows plus a `… N more` stub unless the selected row
/// would be hidden, in which case the whole group expands.
pub fn build_rows<M: MetadataView>(
    audible: Option<&QueueItem>,
    pending: &[QueueItem],
    past: &[QueueItem],
    meta: &M,
    selected_id: Option<TrackId>,
) -> Vec<QueueRow> {
    let track_row = |item: &QueueItem, section: Section| {
        let metadata = meta.metadata(&item.entry.path);
        let (title, tag) =
            display::display_entry_title(&item.entry, metadata.map(|meta| &meta.tags));
        let tag = match (crate::screens::codec_label(&item.entry.path), tag) {
            (Some(codec), Some(tag)) => Some(format!("{codec} · {tag}")),
            (Some(codec), None) => Some(codec.to_owned()),
            (None, tag) => tag,
        };
        QueueRow::Track {
            id: item.id,
            section,
            title,
            tag,
            duration: entry_duration(item, meta),
        }
    };
    let mut rows: Vec<QueueRow> = Vec::new();
    for item in past {
        rows.push(track_row(item, Section::Past));
    }
    if let Some(item) = audible {
        rows.push(track_row(item, Section::Audible));
    }
    // Group adjacent pending entries by album/artist.
    let mut index = 0;
    while index < pending.len() {
        let key = group_key(&pending[index], meta);
        let mut end = index + 1;
        if let Some(key) = &key {
            while end < pending.len() {
                match group_key(&pending[end], meta) {
                    Some(other) if &other == key => end += 1,
                    _ => break,
                }
            }
        }
        let group_len = end - index;
        if key.is_some() && group_len > 1 {
            let metadata = meta.metadata(&pending[index].entry.path);
            let album = metadata
                .and_then(|meta| meta.tags.album.clone())
                .unwrap_or_default();
            let artist = metadata.and_then(|meta| meta.tags.artist.clone());
            let label = match artist {
                Some(artist) => format!("{album} · {artist}"),
                None => album,
            };
            rows.push(QueueRow::Header { label });
            // Expand when the selected row is inside the hidden tail.
            let selected_inside = pending[index..end]
                .iter()
                .position(|item| Some(item.id) == selected_id);
            let collapse = group_len > GROUP_VISIBLE
                && selected_inside.is_none_or(|position| position < GROUP_VISIBLE);
            let visible = if collapse { GROUP_VISIBLE } else { group_len };
            for item in &pending[index..index + visible] {
                rows.push(track_row(item, Section::Pending));
            }
            if collapse {
                rows.push(QueueRow::More {
                    count: group_len - visible,
                });
            }
        } else {
            for item in &pending[index..end] {
                rows.push(track_row(item, Section::Pending));
            }
        }
        index = end;
    }
    rows
}

/// `N tracks` and `H:MM:SS left` summary. Durations of pending entries come
/// from the metadata cache; the current track contributes its remaining
/// time. Any unknown duration yields `time unknown`.
pub fn summary<M: MetadataView>(
    audible: Option<&QueueItem>,
    pending: &[QueueItem],
    meta: &M,
    current_remaining: Option<Duration>,
) -> (usize, String) {
    let count = pending.len() + usize::from(audible.is_some());
    let mut total = Duration::ZERO;
    if audible.is_some() {
        match current_remaining {
            Some(remaining) => total += remaining,
            None => return (count, "time unknown".to_owned()),
        }
    }
    for item in pending {
        match entry_duration(item, meta) {
            Some(duration) => total += duration,
            None => return (count, "time unknown".to_owned()),
        }
    }
    (count, format!("{} left", crate::screens::clock(total)))
}

/// Render inputs for the Queue panel.
pub struct QueueView<'a> {
    pub rows: &'a [QueueRow],
    pub selected_row: Option<usize>,
    pub focused: bool,
    pub shuffle: bool,
    pub track_count: usize,
    pub time_left: &'a str,
}

/// Render the queue into `area`.
pub fn render(frame: &mut Frame<'_>, area: Rect, view: &QueueView<'_>, theme: &Theme) {
    let QueueView {
        rows,
        selected_row,
        focused,
        shuffle,
        track_count,
        time_left,
    } = *view;
    let title = if shuffle {
        format!("Queue · {track_count} tracks · {time_left} · shuffle")
    } else {
        format!("Queue · {track_count} tracks · {time_left}")
    };
    let block = widgets::panel(&title, None, theme, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let width = inner.width as usize;
    let selected_row = selected_row.filter(|index| *index < rows.len());
    let (start, end) = visible_window(rows.len(), selected_row.unwrap_or(0), inner.height as usize);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (offset, row) in rows[start..end].iter().enumerate() {
        let index = start + offset;
        let selected = focused && Some(index) == selected_row;
        let line = match row {
            QueueRow::Header { label } => Line::from(Span::styled(
                widgets::truncate_cells(&format!("  {label}"), width),
                theme.dimmed(),
            )),
            QueueRow::More { count } => Line::from(Span::styled(
                format!("    … {count} more from this album"),
                theme.faint_style(),
            )),
            QueueRow::Track {
                section,
                title,
                tag,
                duration,
                ..
            } => {
                let (marker, marker_style) = match section {
                    Section::Audible => ("▶ ".to_owned(), theme.playing_marker()),
                    Section::Past => ("✓ ".to_owned(), theme.faint_style()),
                    Section::Pending if selected => ("› ".to_owned(), theme.accent_style()),
                    Section::Pending => ("  ".to_owned(), theme.base()),
                };
                let time = duration.map(crate::screens::clock);
                let mut line = widgets::track_row(
                    width,
                    &marker,
                    marker_style,
                    title,
                    tag.as_deref(),
                    time.as_deref(),
                    theme,
                );
                if *section == Section::Past {
                    line = Line::from(
                        line.spans
                            .into_iter()
                            .map(|span| {
                                Span::styled(span.content.into_owned(), theme.faint_style())
                            })
                            .collect::<Vec<_>>(),
                    );
                } else if selected {
                    line = line.style(theme.selected());
                }
                line
            }
        };
        lines.push(line);
    }
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "(queue empty: Enter plays the selected track, a queues it)",
            theme.faint_style(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Selection reconciliation: find the rendered row for a track ID.
pub fn row_of(rows: &[QueueRow], id: TrackId) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row, QueueRow::Track { id: row_id, .. } if *row_id == id))
}

/// The pending IDs in a group of adjacent same-album entries, for tests and
/// the jump target lookup.
pub fn pending_ids(rows: &[QueueRow]) -> HashSet<TrackId> {
    rows.iter()
        .filter_map(|row| match row {
            QueueRow::Track {
                id,
                section: Section::Pending,
                ..
            } => Some(*id),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sointty_core::QueueEntry;
    use sointty_decode::tags::DisplayMetadata;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    struct FakeMeta(HashMap<PathBuf, DisplayMetadata>);

    impl MetadataView for FakeMeta {
        fn metadata(&self, path: &Path) -> Option<&DisplayMetadata> {
            self.0.get(path)
        }
    }

    fn item(id: TrackId, name: &str) -> QueueItem {
        QueueItem {
            id,
            entry: QueueEntry::from(PathBuf::from(format!("/music/{name}"))),
        }
    }

    fn tagged(name: &str, album: &str, artist: &str, seconds: u64) -> (PathBuf, DisplayMetadata) {
        (
            PathBuf::from(format!("/music/{name}")),
            DisplayMetadata {
                tags: sointty_core::TrackTags {
                    title: Some(name.to_owned()),
                    artist: Some(artist.to_owned()),
                    album: Some(album.to_owned()),
                },
                duration: Some(Duration::from_secs(seconds)),
                ..DisplayMetadata::default()
            },
        )
    }

    #[test]
    fn adjacent_album_tracks_group_under_one_header() {
        let meta = FakeMeta(HashMap::from([
            tagged("a.flac", "Album", "Artist", 60),
            tagged("b.flac", "Album", "Artist", 60),
            tagged("c.flac", "Other", "Artist", 60),
        ]));
        let pending = vec![item(1, "a.flac"), item(2, "b.flac"), item(3, "c.flac")];
        let rows = build_rows(None, &pending, &[], &meta, None);
        assert!(matches!(&rows[0], QueueRow::Header { label } if label == "Album · Artist"));
        assert!(matches!(rows[1], QueueRow::Track { id: 1, .. }));
        assert!(matches!(rows[2], QueueRow::Track { id: 2, .. }));
        assert!(matches!(rows[3], QueueRow::Track { id: 3, .. }));
    }

    #[test]
    fn long_groups_collapse_and_expand_around_selection() {
        let mut map = HashMap::new();
        let mut pending = Vec::new();
        for index in 0..12 {
            let name = format!("t{index:02}.flac");
            map.insert(
                tagged(&name, "Album", "Artist", 60).0,
                tagged(&name, "Album", "Artist", 60).1,
            );
            pending.push(item(index as TrackId + 1, &name));
        }
        let meta = FakeMeta(map);
        let rows = build_rows(None, &pending, &[], &meta, None);
        let more = rows.iter().find(|row| matches!(row, QueueRow::More { .. }));
        assert!(
            matches!(more, Some(QueueRow::More { count: 4 })),
            "{rows:?}"
        );
        // Selecting a hidden row expands the group.
        let rows = build_rows(None, &pending, &[], &meta, Some(10));
        assert!(
            !rows.iter().any(|row| matches!(row, QueueRow::More { .. })),
            "{rows:?}"
        );
    }

    #[test]
    fn summary_counts_and_unknown_time() {
        let meta = FakeMeta(HashMap::from([tagged("a.flac", "Album", "Artist", 60)]));
        let audible = item(1, "a.flac");
        let pending = vec![item(2, "missing.flac")];
        let (count, left) = summary(
            Some(&audible),
            &pending,
            &meta,
            Some(Duration::from_secs(30)),
        );
        assert_eq!(count, 2);
        assert_eq!(
            left, "time unknown",
            "one unknown pending duration hides the total"
        );
        let (count, left) = summary(Some(&audible), &[], &meta, Some(Duration::from_secs(90)));
        assert_eq!(count, 1);
        assert_eq!(left, "01:30 left");
        let (count, left) = summary(None, &[], &meta, None);
        assert_eq!(count, 0);
        assert_eq!(left, "00:00 left");
    }

    #[test]
    fn cue_span_duration_uses_cd_frames_not_whole_image() {
        use sointty_core::CueRange;
        let path = PathBuf::from("/music/image.wav");
        let meta = FakeMeta(HashMap::from([(
            path.clone(),
            DisplayMetadata {
                duration: Some(Duration::from_secs(3_600)),
                ..DisplayMetadata::default()
            },
        )]));
        let bounded = QueueItem {
            id: 1,
            entry: QueueEntry {
                path: path.clone(),
                cue_range: Some(CueRange {
                    start_cd: 75,
                    end_cd: Some(225),
                }),
            },
        };
        let open_ended = QueueItem {
            id: 2,
            entry: QueueEntry {
                path,
                cue_range: Some(CueRange {
                    start_cd: 225,
                    end_cd: None,
                }),
            },
        };
        assert_eq!(
            entry_duration(&bounded, &meta),
            Some(Duration::from_secs(2))
        );
        assert_eq!(entry_duration(&open_ended, &meta), None);
        assert_eq!(
            summary(None, &[bounded], &meta, None),
            (1, "00:02 left".to_owned())
        );
        assert_eq!(
            summary(None, &[open_ended], &meta, None),
            (1, "time unknown".to_owned())
        );
    }
}
