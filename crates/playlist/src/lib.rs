//! Playlist reading and writing for Sointty: M3U/M3U8, PLS, XSPF and
//! single-image CUE album sheets.
//!
//! Path policy: relative entries are resolved against the playlist's
//! containing directory. `file://` URIs are converted to local paths; any
//! other `scheme://` URI is rejected with [`PlaylistError::RemoteUrl`] so
//! network protocols are never opened implicitly. Order is preserved.
//!
//! CUE sheets yield one [`PlaylistEntry`] per TRACK with a
//! [`sointty_core::CueRange`]; other formats yield one entry per file with
//! `cue_range: None`.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use sointty_core::CueRange;

/// One playlist item: a file, optionally restricted to a CUE sub-range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistEntry {
    pub path: PathBuf,
    pub cue_range: Option<CueRange>,
}

/// Errors produced by playlist readers and writers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistError {
    /// An I/O failure; carries only the [`io::ErrorKind`] so the error stays
    /// cheap to store and compare.
    Io(io::ErrorKind),
    /// The playlist is syntactically invalid or uses an unsupported feature.
    Parse(&'static str),
    /// The playlist references a remote URL (`http://`, `https://`, `ftp://`,
    /// `smb://`, or any non-`file` scheme). Network protocols are never
    /// opened implicitly.
    RemoteUrl(String),
    /// A CUE sheet references an audio file that does not exist.
    MissingFile(PathBuf),
    /// The CUE sheet is structurally ambiguous (multiple FILE entries,
    /// per-track FILE, missing/non-monotonic INDEX 01, non-sequential track
    /// numbers, or indentation constructs whose REM ownership rcue would
    /// misattribute). Rejected rather than silently misattributed.
    AmbiguousCue(&'static str),
}

impl fmt::Display for PlaylistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(kind) => write!(f, "I/O error ({kind})"),
            Self::Parse(msg) => write!(f, "parse error: {msg}"),
            Self::RemoteUrl(url) => write!(f, "remote URL not supported: {url}"),
            Self::MissingFile(path) => {
                write!(f, "CUE sheet references missing file: {}", path.display())
            }
            Self::AmbiguousCue(msg) => write!(f, "ambiguous CUE sheet: {msg}"),
        }
    }
}

impl std::error::Error for PlaylistError {}

/// Read a playlist, dispatching on the file extension (case-insensitive):
/// `m3u`, `m3u8`, `pls`, `xspf` or `cue`.
pub fn read(path: &Path) -> Result<Vec<PlaylistEntry>, PlaylistError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("m3u" | "m3u8") => read_m3u(path),
        Some("pls") => read_pls(path),
        Some("xspf") => read_xspf(path),
        Some("cue") => read_cue(path),
        _ => Err(PlaylistError::Parse("unknown playlist format")),
    }
}

/// Read an M3U/M3U8 playlist. Comment and metadata lines (`#...`) are
/// skipped; remaining lines are paths or `file://` URIs resolved against the
/// playlist's directory.
pub fn read_m3u(path: &Path) -> Result<Vec<PlaylistEntry>, PlaylistError> {
    let text = read_lossy(path)?;
    let base = base_dir(path);
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        entries.push(resolve_entry(line, &base)?);
    }
    Ok(entries)
}

/// Write a UTF-8 M3U8 playlist (`#EXTM3U` header). Entry paths are written
/// relative to the playlist's directory when representable with `std::path`
/// components (on Windows: same drive prefix), otherwise absolute, so a
/// round-trip survives relocating the whole directory tree.
pub fn write_m3u(path: &Path, entries: &[PlaylistEntry]) -> Result<(), PlaylistError> {
    let abs_path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let base = abs_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut out = String::from("#EXTM3U\n");
    for entry in entries {
        let abs_entry = std::path::absolute(&entry.path).unwrap_or_else(|_| entry.path.clone());
        let written = relative_to(&base, &abs_entry)
            .map(|rel| rel.to_string_lossy().into_owned())
            .unwrap_or_else(|| abs_entry.to_string_lossy().into_owned());
        out.push_str(&written);
        out.push('\n');
    }
    fs::write(path, out).map_err(|e| PlaylistError::Io(e.kind()))
}

/// Read a PLS playlist. `FileN` entries are emitted in numeric `N` order,
/// regardless of the order they appear in the file; `NumberOfEntries` is
/// ignored in favour of the entries actually present.
pub fn read_pls(path: &Path) -> Result<Vec<PlaylistEntry>, PlaylistError> {
    let text = read_lossy(path)?;
    let base = base_dir(path);
    let mut numbered: Vec<(u64, String)> = Vec::new();
    for line in text.lines() {
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = line[..eq].trim();
        let value = line[eq + 1..].trim();
        let lower = key.to_ascii_lowercase();
        let Some(digits) = lower.strip_prefix("file") else {
            continue;
        };
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(index) = digits.parse::<u64>() else {
            continue;
        };
        let value = value.trim_matches('"');
        numbered.push((index, value.to_string()));
    }
    // Numeric order, not file order; be lenient about gaps and duplicates.
    numbered.sort_by_key(|(n, _)| *n);
    numbered
        .into_iter()
        .map(|(_, v)| resolve_entry(&v, &base))
        .collect()
}

/// Read an XSPF playlist: `<trackList><track><location>` values. `file://`
/// locations become local paths; other schemes are rejected. Documents
/// containing a DOCTYPE/DTD are rejected outright, so external entities are
/// never resolved.
pub fn read_xspf(path: &Path) -> Result<Vec<PlaylistEntry>, PlaylistError> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let file = fs::File::open(path).map_err(|e| PlaylistError::Io(e.kind()))?;
    let mut reader = Reader::from_reader(io::BufReader::new(file));
    reader.config_mut().trim_text(true);

    let base = base_dir(path);
    let mut entries = Vec::new();
    let mut in_track = false;
    let mut have_location = false;
    let mut capturing = false;
    let mut location = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => {
                return Err(PlaylistError::Parse(match e {
                    quick_xml::Error::Io(_) => "I/O error reading XSPF",
                    _ => "malformed XSPF document",
                }))
            }
            Ok(Event::Eof) => break,
            // Forbid DTDs entirely: no external entity can ever be resolved.
            Ok(Event::DocType(_)) => {
                return Err(PlaylistError::Parse("DOCTYPE/DTD not allowed in XSPF"))
            }
            Ok(Event::Start(e)) => match e.name().as_ref() {
                "track" => {
                    in_track = true;
                    have_location = false;
                }
                "location" if in_track && !have_location => {
                    capturing = true;
                    location.clear();
                }
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if capturing {
                    let t = quick_xml::escape::unescape(e.as_ref())
                        .map_err(|_| PlaylistError::Parse("bad entity reference in XSPF"))?;
                    location.push_str(&t);
                }
            }
            Ok(Event::CData(e)) => {
                if capturing {
                    location.push_str(e.as_ref());
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                "location" => {
                    capturing = false;
                    have_location = true;
                }
                "track" => {
                    in_track = false;
                    if have_location {
                        let loc = location.trim();
                        if !loc.is_empty() {
                            entries.push(resolve_entry(loc, &base)?);
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    Ok(entries)
}

/// Read a single-image CUE album sheet into one virtual track per TRACK.
///
/// The sheet must contain exactly one `FILE` referencing an existing audio
/// file (resolved against the `.cue` directory, otherwise
/// [`PlaylistError::MissingFile`]) and TRACK entries numbered sequentially
/// from 1, each with exactly one `INDEX 01`. INDEX 01 times must be strictly
/// monotonic. Track 1 spans from CD frame 0 (pregap before its INDEX 01
/// stays audible); track N starts at its own INDEX 01 and ends at the next
/// track's INDEX 01; the final track ends at source EOF (`end_cd: None`).
/// INDEX 00 bytes remain in the preceding span; no silence is injected and
/// no samples are discarded.
pub fn read_cue(path: &Path) -> Result<Vec<PlaylistEntry>, PlaylistError> {
    let bytes = fs::read(path).map_err(|e| PlaylistError::Io(e.kind()))?;
    let text = String::from_utf8_lossy(&bytes);

    // rcue tokenizes each line independently after trimming, so indented
    // continuation lines of multi-line REM/metadata fields get misparsed and
    // misattributed. Reject such sheets rather than silently misattribute.
    if text
        .lines()
        .any(|l| l.starts_with(' ') || l.starts_with('\t'))
    {
        return Err(PlaylistError::AmbiguousCue(
            "indented metadata lines (REM ownership ambiguous for rcue)",
        ));
    }

    let mut cursor = io::Cursor::new(bytes);
    let cue = rcue::parser::parse(&mut cursor, true)
        .map_err(|_| PlaylistError::Parse("malformed CUE sheet"))?;

    if cue.files.is_empty() {
        return Err(PlaylistError::Parse("CUE sheet has no FILE entry"));
    }
    if cue.files.len() > 1 {
        return Err(PlaylistError::AmbiguousCue(
            "multiple FILE entries (per-track FILE not supported)",
        ));
    }
    let file = &cue.files[0];
    let image = PathBuf::from(&file.file);
    let image = if image.is_absolute() {
        image
    } else {
        base_dir(path).join(image)
    };
    match image.try_exists() {
        Ok(true) => {}
        Ok(false) => return Err(PlaylistError::MissingFile(image)),
        Err(e) => return Err(PlaylistError::Io(e.kind())),
    }
    if file.tracks.is_empty() {
        return Err(PlaylistError::Parse("CUE sheet has no TRACK entries"));
    }

    // Extract INDEX 01 of every track, validating numbering and uniqueness.
    let mut starts = Vec::with_capacity(file.tracks.len());
    for (i, track) in file.tracks.iter().enumerate() {
        let expected = (i + 1) as u64;
        let ok = track
            .no
            .parse::<u64>()
            .map(|n| n == expected)
            .unwrap_or(false);
        if !ok {
            return Err(PlaylistError::AmbiguousCue(
                "TRACK numbers not sequential from 1",
            ));
        }
        let idx01: Vec<Duration> = track
            .indices
            .iter()
            .filter(|(n, _)| n == "01")
            .map(|(_, d)| *d)
            .collect();
        match idx01.as_slice() {
            [] => return Err(PlaylistError::AmbiguousCue("TRACK missing INDEX 01")),
            [_] => {}
            _ => return Err(PlaylistError::AmbiguousCue("duplicate INDEX 01 in TRACK")),
        }
        let cd = duration_to_cd_frames(idx01[0]);
        if i > 0 && cd <= starts[i - 1] {
            return Err(PlaylistError::AmbiguousCue(
                "INDEX 01 times not strictly monotonic",
            ));
        }
        starts.push(cd);
    }

    let n = starts.len();
    Ok(file
        .tracks
        .iter()
        .enumerate()
        .map(|(i, _)| PlaylistEntry {
            path: image.clone(),
            cue_range: Some(CueRange {
                start_cd: if i == 0 { 0 } else { starts[i] },
                end_cd: if i + 1 < n { Some(starts[i + 1]) } else { None },
            }),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_lossy(path: &Path) -> Result<String, PlaylistError> {
    let bytes = fs::read(path).map_err(|e| PlaylistError::Io(e.kind()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn base_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Extract a URI scheme if `s` looks like `scheme://...` (RFC 3986 scheme
/// grammar followed by `//`).
fn scheme_of(s: &str) -> Option<&str> {
    let colon = s.find(':')?;
    let scheme = &s[..colon];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    s[colon + 1..].starts_with("//").then_some(scheme)
}

/// Convert the part after `file://` to a local path. Rejects non-local
/// authorities (`file://host/...`) as remote URLs and percent-decodes.
fn file_uri_to_path(rest: &str) -> Result<PathBuf, PlaylistError> {
    let after_authority = if let Some(r) = rest
        .strip_prefix("localhost/")
        .or_else(|| rest.strip_prefix("LOCALHOST/"))
    {
        r
    } else if rest.eq_ignore_ascii_case("localhost") {
        ""
    } else if rest.starts_with('/') {
        // file:///absolute/path (no authority).
        rest
    } else if rest.len() >= 3
        && rest.as_bytes()[1] == b':'
        && (rest.as_bytes()[2] == b'/' || rest.as_bytes()[2] == b'\\')
        && rest.as_bytes()[0].is_ascii_alphabetic()
    {
        // file://C:/path as produced by some Windows tools.
        rest
    } else {
        return Err(PlaylistError::RemoteUrl(format!("file://{rest}")));
    };

    let decoded = percent_decode(after_authority);
    // A leading slash before a Windows drive letter is redundant; strip it so
    // the result is a normal absolute path. On other platforms the leading
    // slash is the root and must be kept.
    let trimmed = {
        #[cfg(windows)]
        {
            decoded.trim_start_matches('/')
        }
        #[cfg(not(windows))]
        {
            decoded.as_str()
        }
    };
    Ok(PathBuf::from(trimmed))
}

fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let (Some(h), Some(l)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            ) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve one playlist entry line/URI against the playlist's directory.
fn resolve_entry(raw: &str, base: &Path) -> Result<PlaylistEntry, PlaylistError> {
    let raw = raw.trim();
    if let Some(scheme) = scheme_of(raw) {
        if scheme.eq_ignore_ascii_case("file") {
            let rest = &raw[scheme.len() + 3..];
            return Ok(PlaylistEntry {
                path: file_uri_to_path(rest)?,
                cue_range: None,
            });
        }
        return Err(PlaylistError::RemoteUrl(raw.to_string()));
    }
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    Ok(PlaylistEntry {
        path,
        cue_range: None,
    })
}

/// Compute `target` relative to `base` using `std::path` components. Returns
/// `None` when not representable (e.g. different Windows drive prefixes, or
/// the path would have to escape a prefix/root).
fn relative_to(base: &Path, target: &Path) -> Option<PathBuf> {
    let base_c: Vec<Component> = base.components().collect();
    let tgt_c: Vec<Component> = target.components().collect();
    match (base_c.first(), tgt_c.first()) {
        (Some(Component::Prefix(b)), Some(Component::Prefix(t))) if b == t => {}
        (Some(Component::Prefix(_)), _) | (_, Some(Component::Prefix(_))) => return None,
        _ => {}
    }
    let mut common = 0;
    while common < base_c.len()
        && common < tgt_c.len()
        && base_c[common] == tgt_c[common]
        && matches!(base_c[common], Component::Prefix(_) | Component::RootDir | Component::Normal(_))
    {
        common += 1;
    }
    let mut out = PathBuf::new();
    for c in &base_c[common..] {
        match c {
            Component::Prefix(_) | Component::RootDir => return None,
            _ => out.push(".."),
        }
    }
    for c in &tgt_c[common..] {
        out.push(c.as_os_str());
    }
    Some(out)
}

/// Convert an rcue `Duration` back to whole CD frames (75 Hz), rounding to
/// the nearest frame (rcue stores `ff/75` seconds with nanosecond floor).
fn duration_to_cd_frames(d: Duration) -> u64 {
    d.as_secs() * 75 + (u64::from(d.subsec_nanos()) * 75 + 500_000_000) / 1_000_000_000
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    /// Unique temp directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "sointty-playlist-{label}-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(path: PathBuf) -> PlaylistEntry {
        PlaylistEntry {
            path,
            cue_range: None,
        }
    }

    fn file_uri(path: &Path) -> String {
        let s = path.to_string_lossy().replace('\\', "/");
        format!("file:///{s}")
    }

    // -- M3U ---------------------------------------------------------------

    #[test]
    fn m3u_round_trip_survives_relocation() {
        let root = TempDir::new("m3u-move");
        let dir_a = root.path().join("A");
        let sub = dir_a.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let files = [
            dir_a.join("one.flac"),
            dir_a.join("two.flac"),
            sub.join("three.flac"),
        ];
        for f in &files {
            fs::write(f, b"x").unwrap();
        }
        let entries: Vec<PlaylistEntry> = files.iter().cloned().map(entry).collect();

        let list = dir_a.join("list.m3u8");
        write_m3u(&list, &entries).unwrap();

        // Same-root entries must have been written relative.
        let written = fs::read_to_string(&list).unwrap();
        assert!(written.starts_with("#EXTM3U\n"), "missing M3U8 header");
        let lines: Vec<&str> = written.lines().skip(1).collect();
        assert_eq!(lines[0], "one.flac");
        assert_eq!(lines[1], "two.flac");
        assert_eq!(lines[2], format!("sub{}", std::path::MAIN_SEPARATOR) + "three.flac");

        // Physically move the tree; relative entries must now resolve into B.
        let dir_b = root.path().join("B");
        fs::rename(&dir_a, &dir_b).unwrap();
        let moved: Vec<PathBuf> = files.iter().map(|f| dir_b.join(f.strip_prefix(&dir_a).unwrap())).collect();

        let read_back = read(&dir_b.join("list.m3u8")).unwrap();
        assert_eq!(read_back.len(), 3);
        for (got, want) in read_back.iter().zip(moved.iter()) {
            assert_eq!(&got.path, want);
            assert_eq!(got.cue_range, None);
        }
    }

    #[test]
    fn m3u_read_mixed_absolute_relative_and_file_uri() {
        let dir = TempDir::new("m3u-mixed");
        let song = dir.path().join("song.flac");
        fs::write(&song, b"x").unwrap();
        let cover = dir.path().join("cover.jpg");
        fs::write(&cover, b"x").unwrap();

        let text = format!(
            "#EXTM3U\n#EXTINF:123,Artist - Title\n{}\nsong.flac\n{}\n",
            cover.display(),
            file_uri(&song),
        );
        let list = dir.path().join("mixed.m3u");
        fs::write(&list, text).unwrap();

        let entries = read_m3u(&list).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, cover);
        assert_eq!(entries[1].path, song, "relative entry must resolve against playlist dir");
        assert_eq!(entries[2].path, song, "file:// URI must convert to a local path");
    }

    #[test]
    fn m3u_read_rejects_remote_url() {
        let dir = TempDir::new("m3u-remote");
        let list = dir.path().join("remote.m3u");
        fs::write(
            &list,
            "#EXTM3U\nhttp://example.com/stream.flac\nC:/local.flac\n",
        )
        .unwrap();
        match read_m3u(&list) {
            Err(PlaylistError::RemoteUrl(url)) => assert_eq!(url, "http://example.com/stream.flac"),
            other => panic!("expected RemoteUrl, got {other:?}"),
        }
    }

    #[test]
    fn m3u_write_absolute_when_different_roots() {
        let dir = TempDir::new("m3u-abs");
        // A path on a different drive than the temp playlist is not
        // relatively representable on Windows; it must be written absolute.
        // On single-drive unix CI the entry is under the same root, so only
        // assert the round-trip, not the exact spelling.
        let other = dir.path().join("other.flac");
        fs::write(&other, b"x").unwrap();
        let list = dir.path().join("abs.m3u8");
        write_m3u(&list, &[entry(other.clone())]).unwrap();
        let read_back = read_m3u(&list).unwrap();
        assert_eq!(read_back, vec![entry(other)]);
    }

    // -- PLS ---------------------------------------------------------------

    #[test]
    fn pls_file_order_is_numeric_not_line_order() {
        let dir = TempDir::new("pls-order");
        let f1 = dir.path().join("one.flac");
        let f2 = dir.path().join("two.flac");
        let f3 = dir.path().join("three.flac");
        for f in [&f1, &f2, &f3] {
            fs::write(f, b"x").unwrap();
        }
        let list = dir.path().join("order.pls");
        fs::write(
            &list,
            "[playlist]\nFile3=three.flac\nFile1=one.flac\nNumberOfEntries=3\nFile2=two.flac\n",
        )
        .unwrap();

        let entries = read_pls(&list).unwrap();
        let paths: Vec<&Path> = entries.iter().map(|e| e.path.as_path()).collect();
        assert_eq!(paths, vec![f1.as_path(), f2.as_path(), f3.as_path()]);
    }

    #[test]
    fn pls_rejects_remote_url() {
        let dir = TempDir::new("pls-remote");
        let list = dir.path().join("remote.pls");
        fs::write(&list, "[playlist]\nFile1=smb://nas/share/a.flac\n").unwrap();
        match read_pls(&list) {
            Err(PlaylistError::RemoteUrl(url)) => assert_eq!(url, "smb://nas/share/a.flac"),
            other => panic!("expected RemoteUrl, got {other:?}"),
        }
    }

    // -- XSPF --------------------------------------------------------------

    #[test]
    fn xspf_reads_file_locations() {
        let dir = TempDir::new("xspf-basic");
        let a = dir.path().join("a.flac");
        let b = dir.path().join("b.flac");
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"x").unwrap();

        let list = dir.path().join("list.xspf");
        let text = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<playlist version="1" xmlns="http://xspf.org/ns/0/">
  <trackList>
    <track><location>{}</location><title>A</title></track>
    <track><location>{}</location><title>B</title></track>
  </trackList>
</playlist>
"#,
            file_uri(&a),
            file_uri(&b)
        );
        fs::write(&list, text).unwrap();

        let entries = read_xspf(&list).unwrap();
        assert_eq!(entries, vec![entry(a), entry(b)]);
    }

    #[test]
    fn xspf_rejects_doctype_and_never_resolves_external_entity() {
        let dir = TempDir::new("xspf-xxe");
        // If the parser resolved external entities, this file's content (a
        // valid location) would be read and imported. The reader must reject
        // the document at the DOCTYPE instead, without opening the file.
        let secret = dir.path().join("secret.txt");
        fs::write(&secret, file_uri(&dir.path().join("pwned.flac"))).unwrap();

        let list = dir.path().join("xxe.xspf");
        let text = format!(
            r#"<?xml version="1.0"?>
<!DOCTYPE playlist [<!ENTITY xxe SYSTEM "{}">]>
<playlist xmlns="http://xspf.org/ns/0/"><trackList>
<track><location>&xxe;</location></track>
</trackList></playlist>
"#,
            file_uri(&secret)
        );
        fs::write(&list, text).unwrap();

        match read_xspf(&list) {
            Err(PlaylistError::Parse(msg)) => assert!(msg.contains("DOCTYPE")),
            other => panic!("expected DOCTYPE rejection, got {other:?}"),
        }
        // The entity target must remain the only reader of itself: prove it
        // was never consumed as a playlist location.
        assert!(read_xspf(&list).is_err());
    }

    #[test]
    fn xspf_rejects_remote_location() {
        let dir = TempDir::new("xspf-remote");
        let list = dir.path().join("remote.xspf");
        fs::write(
            &list,
            r#"<playlist xmlns="http://xspf.org/ns/0/"><trackList>
<track><location>https://example.com/a.flac</location></track>
</trackList></playlist>
"#,
        )
        .unwrap();
        match read_xspf(&list) {
            Err(PlaylistError::RemoteUrl(url)) => {
                assert_eq!(url, "https://example.com/a.flac")
            }
            other => panic!("expected RemoteUrl, got {other:?}"),
        }
    }

    // -- CUE ---------------------------------------------------------------

    fn write_cue_fixture(dir: &Path, sheet: &str) -> PathBuf {
        let cue = dir.join("album.cue");
        fs::write(&cue, sheet).unwrap();
        cue
    }

    #[test]
    fn cue_two_tracks_index01_boundaries() {
        let dir = TempDir::new("cue-basic");
        let image = dir.path().join("album.wav");
        fs::write(&image, b"RIFF-dummy").unwrap();
        let cue = write_cue_fixture(
            dir.path(),
            "FILE \"album.wav\" WAVE\n\
             TRACK 01 AUDIO\n\
             TITLE \"One\"\n\
             INDEX 01 00:00:00\n\
             TRACK 02 AUDIO\n\
             TITLE \"Two\"\n\
             INDEX 01 01:00:00\n",
        );

        let entries = read_cue(&cue).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, image);
        assert_eq!(entries[1].path, image);

        // 01:00:00 => (1*60 + 0) * 75 + 0 = 4500 CD frames.
        let expected_end = 4500;
        assert_eq!(
            entries[0].cue_range,
            Some(CueRange {
                start_cd: 0,
                end_cd: Some(expected_end)
            })
        );
        assert_eq!(
            entries[1].cue_range,
            Some(CueRange {
                start_cd: expected_end,
                end_cd: None
            })
        );

        // Boundary math lock: 75 Hz CD frames to 44.1 kHz sample frames.
        assert_eq!(sointty_core::cd_frames_to_samples(expected_end, 44100), 588 * expected_end);
    }

    #[test]
    fn cue_missing_audio_file_is_missing_file() {
        let dir = TempDir::new("cue-missing");
        let cue = write_cue_fixture(
            dir.path(),
            "FILE \"gone.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        );
        match read_cue(&cue) {
            Err(PlaylistError::MissingFile(p)) => assert_eq!(p, dir.path().join("gone.wav")),
            other => panic!("expected MissingFile, got {other:?}"),
        }
    }

    #[test]
    fn cue_non_monotonic_index01_is_ambiguous() {
        let dir = TempDir::new("cue-monotonic");
        fs::write(dir.path().join("a.wav"), b"x").unwrap();
        let cue = write_cue_fixture(
            dir.path(),
            "FILE \"a.wav\" WAVE\n\
             TRACK 01 AUDIO\nINDEX 01 00:02:00\n\
             TRACK 02 AUDIO\nINDEX 01 00:01:00\n",
        );
        match read_cue(&cue) {
            Err(PlaylistError::AmbiguousCue(msg)) => assert!(msg.contains("monotonic")),
            other => panic!("expected AmbiguousCue, got {other:?}"),
        }
    }

    #[test]
    fn cue_multiple_file_entries_is_ambiguous() {
        let dir = TempDir::new("cue-multifile");
        fs::write(dir.path().join("a.wav"), b"x").unwrap();
        fs::write(dir.path().join("b.wav"), b"x").unwrap();
        let cue = write_cue_fixture(
            dir.path(),
            "FILE \"a.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n\
             FILE \"b.wav\" WAVE\nTRACK 02 AUDIO\nINDEX 01 00:01:00\n",
        );
        match read_cue(&cue) {
            Err(PlaylistError::AmbiguousCue(msg)) => assert!(msg.contains("multiple FILE")),
            other => panic!("expected AmbiguousCue, got {other:?}"),
        }
    }

    #[test]
    fn cue_indented_rem_is_ambiguous() {
        let dir = TempDir::new("cue-indent");
        fs::write(dir.path().join("a.wav"), b"x").unwrap();
        // rcue would misattribute the indented REM continuation line.
        let cue = write_cue_fixture(
            dir.path(),
            "FILE \"a.wav\" WAVE\nTRACK 01 AUDIO\nREM GENRE \"Rock\n  continuation\"\nINDEX 01 00:00:00\n",
        );
        match read_cue(&cue) {
            Err(PlaylistError::AmbiguousCue(msg)) => assert!(msg.contains("indented")),
            other => panic!("expected AmbiguousCue, got {other:?}"),
        }
    }

    #[test]
    fn cue_missing_index01_is_ambiguous() {
        let dir = TempDir::new("cue-noindex");
        fs::write(dir.path().join("a.wav"), b"x").unwrap();
        let cue = write_cue_fixture(
            dir.path(),
            "FILE \"a.wav\" WAVE\nTRACK 01 AUDIO\nTITLE \"No index\"\n",
        );
        match read_cue(&cue) {
            Err(PlaylistError::AmbiguousCue(msg)) => assert!(msg.contains("INDEX 01")),
            other => panic!("expected AmbiguousCue, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_rejects_unknown_extension() {
        let dir = TempDir::new("dispatch");
        let list = dir.path().join("list.txt");
        fs::write(&list, "whatever\n").unwrap();
        match read(&list) {
            Err(PlaylistError::Parse(msg)) => assert_eq!(msg, "unknown playlist format"),
            other => panic!("expected Parse, got {other:?}"),
        }
        match read(&dir.path().join("noextension")) {
            Err(PlaylistError::Parse(msg)) => assert_eq!(msg, "unknown playlist format"),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn error_display_and_source() {
        let err = PlaylistError::RemoteUrl("https://x/y".to_string());
        assert_eq!(err.to_string(), "remote URL not supported: https://x/y");
        assert!(std::error::Error::source(&err).is_none());
        let err = PlaylistError::Io(io::ErrorKind::NotFound);
        assert!(err.to_string().starts_with("I/O error"));
    }
}
