//! Screen renderers for the redesign surfaces. Each is a function of plain
//! view data, so screens are testable without the full app and never do I/O.

pub mod library;
pub mod player;
pub mod playlists;
pub mod queue;
pub mod settings;

use std::path::Path;

use sointty_decode::tags::DisplayMetadata;

/// Read-only access to the metadata cache filled by the search worker.
pub trait MetadataView {
    /// Cached metadata for a path; `None` when unknown or unreadable.
    fn metadata(&self, path: &Path) -> Option<&DisplayMetadata>;
}

/// Compute the visible row window `[start, end)` for a list of `total`
/// rows shown in `height` rows with `selected` kept visible.
pub fn visible_window(total: usize, selected: usize, height: usize) -> (usize, usize) {
    if height == 0 || total == 0 {
        return (0, 0);
    }
    let selected = selected.min(total - 1);
    let start = if selected >= height {
        selected + 1 - height
    } else {
        0
    };
    (start, (start + height).min(total))
}

/// Format a duration as `M:SS` (or `H:MM:SS` past an hour).
pub fn clock(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3_600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3_600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

/// A recognized codec label from the file extension, never guessed from the
/// negotiated output format. Unknown extensions yield `None`.
pub fn codec_label(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "flac" => "FLAC",
        "wav" => "WAV",
        "aif" | "aiff" => "AIFF",
        "mp3" => "MP3",
        "m4a" | "aac" => "AAC",
        "ogg" => "Ogg Vorbis",
        "opus" => "Opus",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_window_keeps_selection_visible() {
        assert_eq!(visible_window(100, 0, 10), (0, 10));
        assert_eq!(visible_window(100, 9, 10), (0, 10));
        assert_eq!(visible_window(100, 10, 10), (1, 11));
        assert_eq!(visible_window(100, 99, 10), (90, 100));
        assert_eq!(visible_window(5, 4, 10), (0, 5));
        assert_eq!(visible_window(0, 0, 10), (0, 0));
        assert_eq!(visible_window(10, 3, 0), (0, 0));
    }

    #[test]
    fn codec_labels_come_from_extensions() {
        assert_eq!(codec_label(Path::new("/a/b.flac")), Some("FLAC"));
        assert_eq!(codec_label(Path::new("/a/b.FLAC")), Some("FLAC"));
        assert_eq!(codec_label(Path::new("/a/b.cue")), None);
        assert_eq!(codec_label(Path::new("/a/noext")), None);
    }
}
