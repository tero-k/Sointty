//! Filesystem browser for the TUI. Uses only `std::fs` — no database or
//! library index is required for browsing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Audio file extensions shown in the browser.
const AUDIO_EXTS: &[&str] = &[
    "flac", "wav", "aif", "aiff", "mp3", "m4a", "aac", "ogg", "opus",
];

/// Playlist file extensions expandable in the browser.
const PLAYLIST_EXTS: &[&str] = &["m3u", "m3u8", "pls", "xspf", "cue"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserItemKind {
    Dir,
    Audio,
    Playlist,
}

#[derive(Debug, Clone)]
pub struct BrowserItem {
    pub name: String,
    pub path: PathBuf,
    pub kind: BrowserItemKind,
}

fn file_kind(path: &Path) -> Option<BrowserItemKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if AUDIO_EXTS.contains(&ext.as_str()) {
        Some(BrowserItemKind::Audio)
    } else if PLAYLIST_EXTS.contains(&ext.as_str()) {
        Some(BrowserItemKind::Playlist)
    } else {
        None
    }
}

fn sort_by_name(items: &mut [BrowserItem]) {
    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
}

/// Lists `dir`: subdirectories first, then audio and playlist files, each
/// group sorted case-insensitively by name. Uses only `std::fs`.
pub fn list_dir(dir: &Path) -> io::Result<Vec<BrowserItem>> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            dirs.push(BrowserItem {
                name,
                path,
                kind: BrowserItemKind::Dir,
            });
        } else if file_type.is_file()
            && let Some(kind) = file_kind(&path)
        {
            files.push(BrowserItem { name, path, kind });
        }
    }
    sort_by_name(&mut dirs);
    sort_by_name(&mut files);
    dirs.extend(files);
    Ok(dirs)
}

/// Browser panel state: current directory, its items, and the selection.
pub struct Browser {
    pub current_dir: PathBuf,
    pub items: Vec<BrowserItem>,
    pub selected: usize,
}

impl Browser {
    pub fn open(dir: PathBuf) -> io::Result<Browser> {
        let items = list_dir(&dir)?;
        Ok(Browser {
            current_dir: dir,
            items,
            selected: 0,
        })
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.items.len() - 1);
    }

    pub fn descend(&mut self, dir: PathBuf) -> io::Result<()> {
        let items = list_dir(&dir)?;
        self.current_dir = dir;
        self.items = items;
        self.selected = 0;
        Ok(())
    }

    pub fn ascend(&mut self) -> io::Result<()> {
        let Some(parent) = self.current_dir.parent().map(Path::to_path_buf) else {
            return Ok(());
        };
        if parent == self.current_dir {
            return Ok(());
        }
        self.descend(parent)
    }

    pub fn selected_item(&self) -> Option<&BrowserItem> {
        self.items.get(self.selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_test_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("sointty-browser-{tag}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn list_dir_groups_dirs_first_and_filters_extensions() {
        let dir = temp_test_dir("group");
        fs::create_dir_all(dir.join("subdir")).unwrap();
        fs::write(dir.join("a.flac"), b"").unwrap();
        fs::write(dir.join("b.MP3"), b"").unwrap();
        fs::write(dir.join("c.txt"), b"").unwrap();
        fs::write(dir.join("d.m3u8"), b"").unwrap();

        // No database file is involved; std::fs only.
        let items = list_dir(&dir).unwrap();
        let summary: Vec<(BrowserItemKind, &str)> = items
            .iter()
            .map(|item| (item.kind, item.name.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                (BrowserItemKind::Dir, "subdir"),
                (BrowserItemKind::Audio, "a.flac"),
                (BrowserItemKind::Audio, "b.MP3"),
                (BrowserItemKind::Playlist, "d.m3u8"),
            ]
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_dir_sorts_case_insensitively_within_groups() {
        let dir = temp_test_dir("sort");
        fs::create_dir_all(dir.join("Zeta")).unwrap();
        fs::create_dir_all(dir.join("alpha")).unwrap();
        fs::write(dir.join("B.mp3"), b"").unwrap();
        fs::write(dir.join("a.flac"), b"").unwrap();
        fs::write(dir.join("C.WAV"), b"").unwrap();

        let items = list_dir(&dir).unwrap();
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Zeta", "a.flac", "B.mp3", "C.WAV"]);

        fs::remove_dir_all(&dir).unwrap();
    }
}
