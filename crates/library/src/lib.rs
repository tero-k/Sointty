//! Opt-in library index backed by SQLite (rusqlite, bundled).
//!
//! The index is opened lazily and only when the user enables index
//! operations; filesystem browsing never depends on this crate. A missing
//! or corrupt database is reported and recreated rather than failing, so a
//! broken index can never block playback or browsing.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, OptionalExtension};

const SCHEMA_VERSION: i32 = 1;
const BATCH_SIZE: usize = 500;

/// Lowercase audio file extensions indexed by [`Library::scan`].
const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "wav", "aif", "aiff", "mp3", "m4a", "aac", "ogg", "opus", "dsf", "dff", "ape", "wv",
    "tak", "mpc",
];

fn log(message: &str) {
    eprintln!("sointty: library index: {message}");
}

/// SQLite-backed library index. Owns the connection; opened lazily via
/// [`Library::open`].
pub struct Library {
    conn: Connection,
}

impl Library {
    /// Open or create the index at `db_path`.
    ///
    /// A missing database is created on demand. A corrupt database is
    /// reported via the log and recreated. This function never panics and
    /// never blocks browsing: failures arrive as a typed [`LibraryError`].
    pub fn open(db_path: &Path) -> Result<Self, LibraryError> {
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        match Self::open_existing(db_path) {
            Ok(library) => Ok(library),
            Err(error) => {
                log(&format!(
                    "index at {} is missing or corrupt ({error}); recreating",
                    db_path.display()
                ));
                // Drop any stale sidecar files before recreating.
                for suffix in ["-wal", "-shm", "-journal"] {
                    let _ = std::fs::remove_file(format!("{}{suffix}", db_path.display()));
                }
                let _ = std::fs::remove_file(db_path);
                Self::open_existing(db_path)
            }
        }
    }

    fn open_existing(db_path: &Path) -> Result<Self, LibraryError> {
        let conn = Connection::open(db_path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Reading user_version forces a parse of the database header, which
        // surfaces "file is not a database" for corrupt files.
        let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            if version != 0 {
                return Err(LibraryError::UnexpectedSchema(version));
            }
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS tracks(
                    path     TEXT PRIMARY KEY,
                    title    TEXT,
                    artist   TEXT,
                    album    TEXT,
                    size     INTEGER NOT NULL,
                    mtime    INTEGER NOT NULL,
                    available INTEGER NOT NULL DEFAULT 1
                );
                PRAGMA user_version = 1;",
            )?;
        }
        Ok(Self { conn })
    }

    /// Recursively scan `roots` for audio files, reading tags via
    /// `sointty_decode::tags::read_tags` and upserting rows keyed by
    /// canonical path with a `(size, mtime)` identity for incremental
    /// refresh. Commits in bounded batches of [`BATCH_SIZE`] files.
    ///
    /// Unreadable directories (e.g. removed mounts) are skipped, and files
    /// whose tags cannot be read still get indexed with the file stem as
    /// fallback title.
    pub fn scan(&mut self, roots: &[PathBuf]) -> Result<ScanStats, LibraryError> {
        let mut files = Vec::new();
        for root in roots {
            collect_audio_files(root, &mut files);
        }
        files.sort();

        let mut stats = ScanStats::default();
        let mut batch = Vec::with_capacity(BATCH_SIZE);
        for file in files {
            batch.push(file);
            if batch.len() >= BATCH_SIZE {
                self.process_batch(&mut batch, &mut stats)?;
            }
        }
        if !batch.is_empty() {
            self.process_batch(&mut batch, &mut stats)?;
        }
        Ok(stats)
    }

    fn process_batch(
        &mut self,
        batch: &mut Vec<PathBuf>,
        stats: &mut ScanStats,
    ) -> Result<(), LibraryError> {
        let mut paths = std::mem::take(batch);
        let tx = self.conn.transaction()?;
        {
            let mut existing = tx.prepare("SELECT size, mtime FROM tracks WHERE path = ?1")?;
            let mut upsert = tx.prepare(
                "INSERT INTO tracks(path, title, artist, album, size, mtime, available)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)
                 ON CONFLICT(path) DO UPDATE SET
                     title = excluded.title,
                     artist = excluded.artist,
                     album = excluded.album,
                     size = excluded.size,
                     mtime = excluded.mtime,
                     available = 1",
            )?;
            for file in &paths {
                stats.scanned += 1;
                let Ok(metadata) = std::fs::metadata(file) else {
                    continue;
                };
                let size = metadata.len() as i64;
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs() as i64)
                    .unwrap_or(0);
                let key = canonical_key(file);
                let unchanged: Option<(i64, i64)> = existing
                    .query_row([&key], |row| Ok((row.get(0)?, row.get(1)?)))
                    .optional()?;
                if unchanged == Some((size, mtime)) {
                    stats.skipped_unchanged += 1;
                    continue;
                }
                let tags = sointty_decode::tags::read_tags(file);
                let title = tags
                    .as_ref()
                    .and_then(|tags| tags.title.clone())
                    .or_else(|| {
                        file.file_stem()
                            .and_then(|stem| stem.to_str())
                            .map(str::to_owned)
                    });
                let artist = tags.as_ref().and_then(|tags| tags.artist.clone());
                let album = tags.as_ref().and_then(|tags| tags.album.clone());
                upsert.execute(rusqlite::params![key, title, artist, album, size, mtime])?;
                stats.inserted_or_updated += 1;
            }
        }
        tx.commit()?;
        paths.clear();
        *batch = paths;
        Ok(())
    }

    /// Mark entries whose file no longer exists as unavailable (rows are
    /// never deleted). Returns the number of rows marked unavailable.
    pub fn mark_unavailable_missing(&mut self) -> Result<u64, LibraryError> {
        let paths: Vec<String> = self
            .conn
            .prepare("SELECT path FROM tracks")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let mut marked = 0;
        let tx = self.conn.transaction()?;
        {
            let mut update = tx.prepare("UPDATE tracks SET available = 0 WHERE path = ?1")?;
            for path in paths {
                if !Path::new(&path).exists() {
                    update.execute([&path])?;
                    marked += 1;
                }
            }
        }
        tx.commit()?;
        Ok(marked)
    }

    /// Case-insensitive substring search over title, artist and album.
    pub fn search(&self, query: &str) -> Result<Vec<LibraryTrack>, LibraryError> {
        let pattern = format!("%{}%", escape_like(query));
        let mut stmt = self.conn.prepare(
            "SELECT path, title, artist, album, available FROM tracks
             WHERE available = 1 AND (
                 title LIKE ?1 ESCAPE '\\' OR
                 artist LIKE ?1 ESCAPE '\\' OR
                 album LIKE ?1 ESCAPE '\\')
             ORDER BY path",
        )?;
        let tracks = stmt
            .query_map([&pattern], row_to_track)?
            .collect::<Result<_, _>>()?;
        Ok(tracks)
    }

    /// List up to `limit` available tracks, ordered by path.
    pub fn tracks(&self, limit: usize) -> Result<Vec<LibraryTrack>, LibraryError> {
        let mut stmt = self.conn.prepare(
            "SELECT path, title, artist, album, available FROM tracks
             WHERE available = 1 ORDER BY path LIMIT ?1",
        )?;
        let tracks = stmt
            .query_map([limit as i64], row_to_track)?
            .collect::<Result<_, _>>()?;
        Ok(tracks)
    }
}

fn row_to_track(row: &rusqlite::Row<'_>) -> rusqlite::Result<LibraryTrack> {
    Ok(LibraryTrack {
        path: PathBuf::from(row.get::<_, String>(0)?),
        title: row.get(1)?,
        artist: row.get(2)?,
        album: row.get(3)?,
        available: row.get::<_, i64>(4)? != 0,
    })
}

fn canonical_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn is_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| AUDIO_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

fn collect_audio_files(dir: &Path, out: &mut Vec<PathBuf>) {
    // Unreadable directories (removed mounts, permissions) are skipped so a
    // scan always makes progress and never blocks browsing.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_audio_files(&path, out);
        } else if file_type.is_file() && is_audio_path(&path) {
            out.push(path);
        }
    }
}

fn escape_like(query: &str) -> String {
    query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// One indexed track as returned by [`Library::search`] and [`Library::tracks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryTrack {
    pub path: PathBuf,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub available: bool,
}

/// Aggregate counters for [`Library::scan`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanStats {
    pub scanned: u64,
    pub inserted_or_updated: u64,
    pub skipped_unchanged: u64,
}

/// Errors from the library index. All failures are typed; the index never
/// panics and never blocks playback or browsing.
#[derive(Debug)]
pub enum LibraryError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    /// The database reports a schema version this build does not understand.
    UnexpectedSchema(i32),
}

impl fmt::Display for LibraryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Sql(error) => write!(f, "database error: {error}"),
            Self::UnexpectedSchema(version) => {
                write!(f, "unsupported index schema version {version}")
            }
        }
    }
}

impl std::error::Error for LibraryError {}

impl From<std::io::Error> for LibraryError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for LibraryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sointty-library-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Minimal PCM WAV writer replicating the fixture byte pattern used by
    /// the decode crate's tests (see crates/decode/src/lib.rs).
    fn wav_fixture() -> Vec<u8> {
        let samples: [i16; 8] = [0, 1, -1, i16::MAX, i16::MIN, 2, -2, 3];
        let data_len = (samples.len() * 2) as u32;
        let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&44_100_u32.to_le_bytes());
        bytes.extend_from_slice(&(44_100_u32 * 2 * 2).to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn write_wav(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, wav_fixture()).unwrap();
        path
    }

    #[test]
    fn open_creates_schema_lazily() {
        let dir = temp_dir("open");
        let db_path = dir.join("index.db");
        assert!(!db_path.exists());
        let library = Library::open(&db_path).unwrap();
        assert!(db_path.exists());
        let tables: Vec<String> = library
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(tables.iter().any(|table| table == "tracks"));
    }

    #[test]
    fn scan_and_search() {
        let dir = temp_dir("scan");
        let music = dir.join("music");
        std::fs::create_dir_all(&music).unwrap();
        let alpha = write_wav(&music, "Alpha Song.wav");
        let beta = write_wav(&music, "Beta Song.wav");
        // Non-audio files are ignored.
        std::fs::write(music.join("notes.txt"), b"hello").unwrap();
        let db_path = dir.join("index.db");
        let mut library = Library::open(&db_path).unwrap();

        let stats = library.scan(&[music.clone()]).unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.inserted_or_updated, 2);
        assert_eq!(stats.skipped_unchanged, 0);

        // Untagged WAVs fall back to the file stem as title.
        let hits = library.search("alpha").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title.as_deref(), Some("Alpha Song"));
        assert_eq!(canonical_key(&alpha), hits[0].path.to_string_lossy());
        assert_eq!(library.search("song").unwrap().len(), 2);
        assert_eq!(library.tracks(10).unwrap().len(), 2);

        // Rescan with unchanged files: everything is skipped.
        let stats = library.scan(&[music.clone()]).unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.inserted_or_updated, 0);
        assert_eq!(stats.skipped_unchanged, 2);

        // Touch one file (content rewrite changes size and mtime) so its
        // (size, mtime) identity no longer matches: only it is rescanned.
        std::fs::write(&beta, [wav_fixture(), vec![0]].concat()).unwrap();
        let stats = library.scan(&[music.clone()]).unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.inserted_or_updated, 1);
        assert_eq!(stats.skipped_unchanged, 1);

        // Scanning a removed mount must not error.
        let stats = library.scan(&[dir.join("does-not-exist")]).unwrap();
        assert_eq!(stats.scanned, 0);
    }

    #[test]
    fn missing_files_marked_unavailable() {
        let dir = temp_dir("missing");
        let first = write_wav(&dir, "First.wav");
        write_wav(&dir, "Second.wav");
        let db_path = dir.join("index.db");
        let mut library = Library::open(&db_path).unwrap();
        library.scan(&[dir.clone()]).unwrap();

        std::fs::remove_file(&first).unwrap();
        let marked = library.mark_unavailable_missing().unwrap();
        assert_eq!(marked, 1);

        // Row is still present, flagged unavailable; search hides it.
        let all = library.tracks(10).unwrap();
        assert_eq!(all.len(), 1);
        let hits = library.search("first").unwrap();
        assert_eq!(hits.len(), 0);
        // Second scan does not resurrect it; unchanged files are skipped.
        let stats = library.scan(&[dir.clone()]).unwrap();
        assert_eq!(stats.inserted_or_updated, 0);
        assert_eq!(stats.skipped_unchanged, 1);
    }

    #[test]
    fn corrupt_db_recovers() {
        let dir = temp_dir("corrupt");
        let db_path = dir.join("index.db");
        std::fs::write(&db_path, b"this is not a sqlite database at all").unwrap();
        // Must not panic; open either recreates or returns a typed error.
        let library = match Library::open(&db_path) {
            Ok(library) => library,
            Err(error) => {
                eprintln!("corrupt_db_recovers: open returned typed error: {error}");
                return;
            }
        };
        let tables: Vec<String> = library
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(tables.iter().any(|table| table == "tracks"));
    }
}
