//! Display-title helpers: turn paths, tags and queue entries into clean
//! titles plus optional dim tags for the UI. Pure presentation only.

use std::path::Path;

use sointty_core::{CueRange, QueueEntry, TrackTags};

/// CD frames per second; CUE ranges are stored in 75 Hz CD frames.
const CD_FRAMES_PER_SECOND: u64 = 75;

/// Removes a leading 1-3 digit track number followed by ` - `, `-` or `.`
/// and an optional space, returning the remaining text.
fn strip_track_prefix(stem: &str) -> &str {
    let bytes = stem.as_bytes();
    let mut digits = 0;
    while digits < bytes.len() && digits < 3 && bytes[digits].is_ascii_digit() {
        digits += 1;
    }
    if digits == 0 || digits == bytes.len() {
        return stem;
    }
    let rest = &stem[digits..];
    let rest = if let Some(after) = rest.strip_prefix(" - ") {
        after
    } else if let Some(after) = rest.strip_prefix('-').or_else(|| rest.strip_prefix('.')) {
        after
    } else {
        return stem;
    };
    rest.trim_start_matches(' ')
}

/// Derives a fallback track number from a filename prefix such as
/// `01 - Track.flac`; only valid when no tagged track number is known.
pub fn fallback_track_number(path: &Path) -> Option<u32> {
    let stem = path.file_stem()?.to_str()?;
    let bytes = stem.as_bytes();
    let mut digits = 0;
    while digits < bytes.len() && digits < 3 && bytes[digits].is_ascii_digit() {
        digits += 1;
    }
    if digits == 0 || digits == bytes.len() {
        return None;
    }
    let rest = &stem[digits..];
    let stripped = rest
        .strip_prefix(" - ")
        .or_else(|| rest.strip_prefix('-'))
        .or_else(|| rest.strip_prefix('.'))?;
    if stripped.trim_start_matches(' ').is_empty() {
        return None;
    }
    stem[..digits].parse().ok()
}

/// Splits a terminal balanced parenthesized group (`(Live)`) into a dim tag.
/// Only the final parenthesized group at the very end of the string becomes
/// the tag; earlier parentheses stay in the title.
fn split_terminal_tag(title: &str) -> (String, Option<String>) {
    let trimmed = title.trim_end();
    if !trimmed.ends_with(')') {
        return (title.to_owned(), None);
    }
    let mut depth = 0_u32;
    let mut open_idx = None;
    for (idx, ch) in trimmed.char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    open_idx = Some(idx);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(open_idx) = open_idx else {
        return (title.to_owned(), None);
    };
    // Require the group to be a true suffix preceded by whitespace, so
    // `Track(2)` style names are left alone.
    let preceded = &trimmed[..open_idx];
    let before = preceded.trim_end();
    if before.is_empty() || !preceded.ends_with(char::is_whitespace) {
        return (title.to_owned(), None);
    }
    let inner = trimmed[open_idx + 1..trimmed.len() - 1].trim();
    if inner.is_empty() {
        return (title.to_owned(), None);
    }
    (before.trim_end().to_owned(), Some(inner.to_owned()))
}

/// Splits a path into a clean display title and an optional dim tag.
///
/// Prefers a nonempty tagged title, otherwise the filename stem with a
/// leading track-number prefix removed. A terminal parenthesized suffix
/// such as `(Live)` is split into the dim tag for both sources.
pub fn display_title(path: &Path, tags: Option<&TrackTags>) -> (String, Option<String>) {
    let tagged = tags
        .and_then(|tags| tags.title.as_deref())
        .filter(|title| !title.trim().is_empty());
    let base = tagged.map(str::to_owned).unwrap_or_else(|| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let (title, tag) = if tagged.is_some() {
        split_terminal_tag(&base)
    } else {
        let stripped = strip_track_prefix(&base);
        let stripped = if stripped.is_empty() { &base } else { stripped };
        split_terminal_tag(stripped)
    };
    (title, tag)
}

/// Formats CD frames as `mm:ss`.
fn format_cd(frames: u64) -> String {
    let total_seconds = frames / CD_FRAMES_PER_SECOND;
    format!("{:02}:{:02}", total_seconds / 60, total_seconds % 60)
}

/// Builds the dim tag for a CUE range: `CUE mm:ss–mm:ss` or `CUE mm:ss–end`.
fn cue_range_label(range: CueRange) -> String {
    let start = format_cd(range.start_cd);
    match range.end_cd {
        Some(end_cd) => format!("CUE {start}\u{2013}{}", format_cd(end_cd)),
        None => format!("CUE {start}\u{2013}end"),
    }
}

/// Like [`display_title`] but for queue entries; appends a distinct dim
/// `CUE mm:ss–mm:ss` (or `CUE mm:ss–end`) range tag for CUE-bounded entries.
pub fn display_entry_title(entry: &QueueEntry, tags: Option<&TrackTags>) -> (String, Option<String>) {
    let (title, tag) = display_title(&entry.path, tags);
    let cue_tag = entry.cue_range.map(cue_range_label);
    match (tag, cue_tag) {
        (Some(tag), Some(cue_tag)) => (title, Some(format!("{tag} \u{00B7} {cue_tag}"))),
        (Some(tag), None) => (title, Some(tag)),
        (None, cue_tag) => (title, cue_tag),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from("/music").join(name)
    }

    fn tags(title: &str) -> TrackTags {
        TrackTags {
            title: Some(title.to_owned()),
            artist: None,
            album: None,
        }
    }

    #[test]
    fn spaced_dash_prefix_is_removed() {
        assert_eq!(display_title(&path("01 - Track.flac"), None), ("Track".into(), None));
    }

    #[test]
    fn dash_prefix_is_removed() {
        assert_eq!(display_title(&path("01-Track.flac"), None), ("Track".into(), None));
    }

    #[test]
    fn dot_prefix_is_removed() {
        assert_eq!(display_title(&path("01.Track.flac"), None), ("Track".into(), None));
    }

    #[test]
    fn plain_stem_is_kept() {
        assert_eq!(display_title(&path("Song.flac"), None), ("Song".into(), None));
    }

    #[test]
    fn unicode_titles_survive() {
        assert_eq!(
            display_title(&path("曲 名前 (Live).flac"), None),
            ("曲 名前".into(), Some("Live".into()))
        );
        assert_eq!(
            display_title(&path("Song.flac"), Some(&tags("München Übung"))),
            ("München Übung".into(), None)
        );
    }

    #[test]
    fn no_extension_still_works() {
        assert_eq!(display_title(&path("Song"), None), ("Song".into(), None));
    }

    #[test]
    fn terminal_parens_split_into_tag() {
        assert_eq!(
            display_title(&path("Track (Live).flac"), None),
            ("Track".into(), Some("Live".into()))
        );
    }

    #[test]
    fn multiword_terminal_parens_keep_inner_text() {
        assert_eq!(
            display_title(&path("Track (1986 Sound City Session).flac"), None),
            ("Track".into(), Some("1986 Sound City Session".into()))
        );
    }

    #[test]
    fn earlier_parens_stay_in_title() {
        assert_eq!(
            display_title(&path("A (B) (C).flac"), None),
            ("A (B)".into(), Some("C".into()))
        );
    }

    #[test]
    fn tagged_title_wins_and_splits_its_own_parens() {
        assert_eq!(
            display_title(&path("01 - File.flac"), Some(&tags("Song (Remaster)"))),
            ("Song".into(), Some("Remaster".into()))
        );
    }

    #[test]
    fn empty_tagged_title_falls_back_to_filename() {
        assert_eq!(display_title(&path("Song.flac"), Some(&tags("  "))), ("Song".into(), None));
    }

    #[test]
    fn cue_ranges_from_same_image_differ() {
        let image = path("image.flac");
        let first = QueueEntry {
            path: image.clone(),
            cue_range: Some(CueRange { start_cd: 0, end_cd: Some(75 * 60) }),
        };
        let second = QueueEntry {
            path: image,
            cue_range: Some(CueRange { start_cd: 75 * 60, end_cd: None }),
        };
        assert_eq!(display_entry_title(&first, None), ("image".into(), Some("CUE 00:00\u{2013}01:00".into())));
        assert_eq!(display_entry_title(&second, None), ("image".into(), Some("CUE 01:00\u{2013}end".into())));
    }

    #[test]
    fn cue_tag_combines_with_existing_tag() {
        let entry = QueueEntry {
            path: path("Track (Live).flac"),
            cue_range: Some(CueRange { start_cd: 0, end_cd: Some(75 * 90) }),
        };
        assert_eq!(
            display_entry_title(&entry, None),
            ("Track".into(), Some("Live \u{00B7} CUE 00:00\u{2013}01:30".into()))
        );
    }

    #[test]
    fn entry_without_cue_matches_display_title() {
        let entry = QueueEntry::from(path("01 - Track.flac"));
        assert_eq!(display_entry_title(&entry, None), ("Track".into(), None));
    }

    #[test]
    fn fallback_track_number_from_prefix_only() {
        assert_eq!(fallback_track_number(&path("01 - Track.flac")), Some(1));
        assert_eq!(fallback_track_number(&path("12-Track.flac")), Some(12));
        assert_eq!(fallback_track_number(&path("Song.flac")), None);
        assert_eq!(fallback_track_number(&path("1234 - Track.flac")), None);
    }
}
