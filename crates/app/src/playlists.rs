//! Persistent named playlists in `<data dir>/sointty/playlists.toml`.
//!
//! The file keeps a selected index plus an ordered list of named playlists;
//! each entry stores an absolute path and an optional CUE range in CD
//! frames. Serialization types are local: core types carry no serde.
//!
//! Atomicity: writes go to a unique same-directory temporary file followed
//! by a rename, so a crash mid-write never leaves a torn catalog. A
//! malformed existing file is never overwritten: it loads read-only and any
//! save attempt fails until the user repairs or removes the file.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::path::{Path, PathBuf};

use sointty_core::{CueRange, QueueEntry};
use sointty_tui::{PlaylistCatalog, SavedPlaylist};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CatalogFile {
    #[serde(default)]
    selected: usize,
    #[serde(default)]
    playlists: Vec<PlaylistFile>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PlaylistFile {
    name: String,
    #[serde(default)]
    entries: Vec<EntryFile>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct EntryFile {
    path: PathBuf,
    #[serde(default)]
    cue_range: Option<CueRangeFile>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct CueRangeFile {
    start_cd: u64,
    end_cd: Option<u64>,
}

impl From<&QueueEntry> for EntryFile {
    fn from(entry: &QueueEntry) -> Self {
        Self {
            path: entry.path.clone(),
            cue_range: entry.cue_range.map(|range| CueRangeFile {
                start_cd: range.start_cd,
                end_cd: range.end_cd,
            }),
        }
    }
}

impl From<EntryFile> for QueueEntry {
    fn from(entry: EntryFile) -> Self {
        Self {
            path: entry.path,
            cue_range: entry.cue_range.map(|range| CueRange {
                start_cd: range.start_cd,
                end_cd: range.end_cd,
            }),
        }
    }
}

/// Where the catalog lives; `None` when the platform has no data dir.
pub fn catalog_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "sointty", "sointty")
        .map(|dirs| dirs.data_dir().join("playlists.toml"))
}

/// A loaded catalog plus whether it is writable. A missing file yields an
/// empty writable catalog; a malformed file loads as an empty *read-only*
/// catalog so an accidental save can never destroy user data.
pub struct LoadedCatalog {
    pub catalog: PlaylistCatalog,
    /// Full path and parse/read error for malformed files. Edit actions are
    /// disabled while this is set.
    pub read_only_error: Option<String>,
}

pub fn load() -> LoadedCatalog {
    let Some(path) = catalog_path() else {
        return LoadedCatalog {
            catalog: PlaylistCatalog::default(),
            read_only_error: Some("no platform data directory available".to_owned()),
        };
    };
    load_from(&path)
}

fn load_from(path: &Path) -> LoadedCatalog {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LoadedCatalog {
                catalog: PlaylistCatalog::default(),
                read_only_error: None,
            };
        }
        Err(error) => {
            return LoadedCatalog {
                catalog: PlaylistCatalog::default(),
                read_only_error: Some(format!("{}: {error}", path.display())),
            };
        }
    };
    match toml::from_str::<CatalogFile>(&text) {
        Ok(file) if file.playlists.iter().flat_map(|list| &list.entries)
            .all(|entry| entry.path.is_absolute()) => LoadedCatalog {
            catalog: from_file(file),
            read_only_error: None,
        },
        Ok(_) => LoadedCatalog {
            catalog: PlaylistCatalog::default(),
            read_only_error: Some(format!("{}: entries must have absolute paths", path.display())),
        },
        Err(error) => LoadedCatalog {
            catalog: PlaylistCatalog::default(),
            read_only_error: Some(format!("{}: {error}", path.display())),
        },
    }
}

/// Persist the catalog atomically. Returns an error (and writes nothing)
/// when the existing file is malformed: never clobber user data.
pub fn save(catalog: &PlaylistCatalog) -> Result<(), String> {
    let Some(path) = catalog_path() else {
        return Err("no platform data directory available".to_owned());
    };
    save_to(&path, catalog)
}

/// Save to an explicit path; separated from [`save`] for tests.
pub fn save_to(path: &Path, catalog: &PlaylistCatalog) -> Result<(), String> {
    if catalog.playlists.is_empty() {
        return Err("a catalog must contain at least one playlist".to_owned());
    }
    if catalog.playlists.iter().flat_map(|list| &list.entries)
        .any(|entry| !entry.path.is_absolute())
    {
        return Err("playlist entries must have absolute paths".to_owned());
    }
    match std::fs::read_to_string(path) {
        Ok(text) => {
            toml::from_str::<CatalogFile>(&text).map_err(|error| {
                format!("{}: {error}; repair the existing file first", path.display())
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", path.display())),
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    let file = to_file(catalog);
    let text = toml::to_string_pretty(&file).map_err(|error| error.to_string())?;
    let tmp = unique_temp_path(path);
    let written = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {error}", path.display()));
    }
    Ok(())
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

fn unique_temp_path(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".{}-{nanos}-{}.tmp", std::process::id(), NEXT_TEMP.fetch_add(1, Ordering::Relaxed)));
    path.with_file_name(name)
}
fn to_file(catalog: &PlaylistCatalog) -> CatalogFile {
    CatalogFile {
        selected: catalog.selected,
        playlists: catalog
            .playlists
            .iter()
            .map(|playlist| PlaylistFile {
                name: playlist.name.clone(),
                entries: playlist.entries.iter().map(EntryFile::from).collect(),
            })
            .collect(),
    }
}

fn from_file(file: CatalogFile) -> PlaylistCatalog {
    let playlists: Vec<SavedPlaylist> = file
        .playlists
        .into_iter()
        .map(|playlist| SavedPlaylist {
            name: playlist.name,
            entries: playlist.entries.into_iter().map(QueueEntry::from).collect(),
        })
        .collect();
    if playlists.is_empty() {
        return PlaylistCatalog::default();
    }
    let selected = file.selected.min(playlists.len() - 1);
    PlaylistCatalog { selected, playlists }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("sointty-playlists-{tag}-{}-{nanos}", std::process::id()))
            .join("playlists.toml")
    }

    fn entry(path: &Path, cue_range: Option<CueRange>) -> QueueEntry {
        QueueEntry {
            path: path.to_path_buf(),
            cue_range,
        }
    }

    #[test]
    fn save_reload_preserves_order_ranges_and_selection() {
        let path = temp_path("roundtrip");
        let image = path.parent().unwrap().join("music/a.flac");
        let other = path.parent().unwrap().join("music/b.mp3");
        let catalog = PlaylistCatalog {
            selected: 1,
            playlists: vec![
                SavedPlaylist {
                    name: "first".to_owned(),
                    entries: vec![
                        entry(&image, None),
                        // Duplicate path with a CUE range must survive.
                        entry(
                            &image,
                            Some(CueRange { start_cd: 0, end_cd: Some(150) }),
                        ),
                        entry(
                            &image,
                            Some(CueRange { start_cd: 150, end_cd: Some(300) }),
                        ),
                        // Final track of a CUE: open-ended range.
                        entry(&image, Some(CueRange { start_cd: 300, end_cd: None })),
                    ],
                },
                SavedPlaylist {
                    name: "second".to_owned(),
                    entries: vec![entry(&other, None)],
                },
            ],
        };
        save_to(&path, &catalog).unwrap();
        let LoadedCatalog { catalog: loaded, read_only_error } = load_from(&path);
        assert!(read_only_error.is_none());
        assert_eq!(loaded.selected, 1);
        assert_eq!(loaded.playlists.len(), 2);
        assert_eq!(loaded.playlists[0].name, "first");
        assert_eq!(loaded.playlists[0].entries, catalog.playlists[0].entries);
        assert_eq!(loaded.playlists[1].entries, catalog.playlists[1].entries);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn malformed_file_is_never_overwritten() {
        let path = temp_path("malformed");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "this is [not valid toml").unwrap();
        let catalog = PlaylistCatalog::default();
        let error = save_to(&path, &catalog).unwrap_err();
        assert!(error.contains("malformed"));
        // Untouched: the original bytes are still there.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "this is [not valid toml");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn selected_index_is_clamped_when_list_deleted() {
        let file: CatalogFile = toml::from_str(
            "selected = 5\n[[playlists]]\nname = \"only\"\n",
        )
        .unwrap();
        let catalog = from_file(file);
        assert_eq!(catalog.selected, 0);
    }
}
