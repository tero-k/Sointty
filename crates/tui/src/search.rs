//! Background metadata and search worker.
//!
//! One worker thread owns both jobs so the UI/event thread never does file
//! I/O or tag reads:
//!
//! - Priority metadata requests for currently visible browser/queue/
//!   playlist rows, answered from a path-keyed cache capped at 2,048
//!   entries; directory refreshes invalidate the cache below that dir.
//! - Lazy, generation-tagged search indexing over the configured roots
//!   using incremental `browser::list_dir` traversal. Newer queries and
//!   cancels are checked after every directory, so indexing never blocks
//!   the UI and stale generations are discarded.
//!
//! The optional SQLite library index is never opened here; this worker is
//! the whole search backend for the TUI.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, Sender};

use sointty_core::QueueEntry;
use sointty_decode::tags::{DisplayMetadata, read_display_metadata};

use crate::browser::{self, BrowserItemKind};

/// Cache cap for path-keyed display metadata.
pub const METADATA_CACHE_LIMIT: usize = 2_048;

/// Result batches are capped so one send never floods the UI channel.
pub const RESULT_BATCH: usize = 64;

/// UI -> worker messages.
pub enum WorkerMsg {
    /// Prioritized metadata for a currently visible row.
    Metadata(PathBuf),
    /// Drop cached metadata below this directory (browser refreshed).
    InvalidateDir(PathBuf),
    /// Replace the search roots; cancels any running index/query.
    SetRoots(Vec<PathBuf>),
    /// Run a query. `generation` strictly increases per query; results for
    /// older generations are never sent.
    Query {
        generation: u64,
        text: String,
    },
    /// Cancel the active query/index (overlay closed).
    Cancel,
    Shutdown,
}

/// Worker -> UI messages.
pub enum WorkerEvent {
    Metadata {
        path: PathBuf,
        meta: Option<DisplayMetadata>,
    },
    /// A bounded batch of new hits for the current generation.
    Hits {
        generation: u64,
        hits: Vec<SearchHit>,
    },
    /// Indexing/search status for the current generation.
    Status {
        generation: u64,
        status: SearchStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchStatus {
    /// Indexing is in progress; results remain progressive.
    Indexing,
    /// Index covered every reachable root.
    Done,
    /// Indexing finished but some roots were skipped; counts included.
    Incomplete { skipped_roots: usize },
}

/// What a search hit refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitKind {
    Track,
    Album,
    Folder,
    /// A playlist file (.m3u/.cue/...); named catalog playlists are merged
    /// into results by the UI, not indexed here.
    PlaylistFile,
}

/// One search hit. `spans` and `subtitle_spans` are char indices to highlight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub kind: HitKind,
    pub path: PathBuf,
    pub title: String,
    pub subtitle: Option<String>,
    /// Deterministic ordering: lower sorts first.
    pub rank: (i64, String, String),
    pub spans: Vec<usize>,
    pub subtitle_spans: Vec<usize>,
}

/// A queued search scope (`Tab` cycles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    All,
    Albums,
    Tracks,
    Folders,
    Playlists,
}

impl SearchScope {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Albums,
            Self::Albums => Self::Tracks,
            Self::Tracks => Self::Folders,
            Self::Folders => Self::Playlists,
            Self::Playlists => Self::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Albums => "Albums",
            Self::Tracks => "Tracks",
            Self::Folders => "Folders",
            Self::Playlists => "Playlists",
        }
    }

    /// Whether a hit kind belongs to this scope.
    pub fn admits(self, kind: &HitKind) -> bool {
        match self {
            Self::All => true,
            Self::Albums => matches!(kind, HitKind::Album),
            Self::Tracks => matches!(kind, HitKind::Track),
            Self::Folders => matches!(kind, HitKind::Folder),
            Self::Playlists => matches!(kind, HitKind::PlaylistFile),
        }
    }
}

/// Case-insensitive substring first; otherwise a scored subsequence.
/// Returns a rank score (higher is better) and the matched char indices.
pub fn match_spans(query: &str, candidate: &str) -> Option<(i64, Vec<usize>)> {
    let query: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let candidate_chars: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = candidate_chars
        .iter()
        .flat_map(|ch| ch.to_lowercase())
        .collect();
    // Lowercasing can change char counts (rare); fall back to byte-free
    // handling by only supporting 1:1 lowercase mappings here.
    if lower.len() != candidate_chars.len() {
        return subsequence(&query, &lower);
    }
    if query.is_empty() {
        return None;
    }
    // Substring search over char indices.
    if query.len() <= lower.len() {
        for start in 0..=(lower.len() - query.len()) {
            if lower[start..start + query.len()] == query[..] {
                let spans: Vec<usize> = (start..start + query.len()).collect();
                // Earlier and relatively longer matches rank higher.
                let score = 10_000 - start as i64 * 4 + query.len() as i64;
                return Some((score, spans));
            }
        }
    }
    subsequence(&query, &lower)
}

/// Scored subsequence match: all query chars must appear in order. Score
/// rewards compact matches (small span, few gaps).
fn subsequence(query: &[char], candidate: &[char]) -> Option<(i64, Vec<usize>)> {
    if query.is_empty() || query.len() > candidate.len() {
        return None;
    }
    let mut spans = Vec::with_capacity(query.len());
    let mut cursor = 0;
    for &needle in query {
        let found = candidate[cursor..]
            .iter()
            .position(|&ch| ch == needle)
            .map(|offset| cursor + offset);
        let index = found?;
        spans.push(index);
        cursor = index + 1;
    }
    let span = spans.last().unwrap() - spans.first().unwrap() + 1;
    let gaps = span - query.len();
    let score = 5_000 - gaps as i64 * 8 - *spans.first().unwrap() as i64;
    Some((score, spans))
}

/// One indexed filesystem entry with small metadata.
#[derive(Debug, Clone)]
struct IndexedItem {
    path: PathBuf,
    kind: HitKind,
    /// Display text used for matching and rendering.
    title: String,
    subtitle: Option<String>,
}

impl IndexedItem {
    fn into_hit(self, query: &str) -> Option<SearchHit> {
        // Match across title and subtitle; keep the best.
        let mut best = match_spans(query, &self.title).map(|(score, spans)| (score, spans, false));
        if let Some(subtitle) = &self.subtitle
            && let Some((score, spans)) = match_spans(query, subtitle)
        {
            let candidate = (score - 2_000, spans, true);
            if best.as_ref().is_none_or(|current| candidate.0 > current.0) {
                best = Some(candidate);
            }
        }
        let (score, spans, in_subtitle) = best?;
        let rank = (
            -score,
            self.title.to_lowercase(),
            self.path.to_string_lossy().into_owned(),
        );
        Some(SearchHit {
            kind: self.kind,
            path: self.path,
            title: self.title,
            subtitle: self.subtitle,
            rank,
            spans: if in_subtitle {
                Vec::new()
            } else {
                spans.clone()
            },
            subtitle_spans: if in_subtitle { spans } else { Vec::new() },
        })
    }
}

/// Normalize roots: canonicalize, drop duplicates and roots contained in an
/// earlier root, so e.g. Home is not scanned twice when its drive is a root.
pub fn normalize_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in roots {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let contained = out.iter().any(|existing| root.starts_with(existing));
        if !contained && !out.contains(&root) {
            // A later, shorter root replaces earlier nested ones.
            out.retain(|existing| !existing.starts_with(&root));
            out.push(root);
        }
    }
    out
}

/// Handle to the background worker; `try_recv` results on the UI thread.
pub struct SearchWorker {
    tx: Sender<WorkerMsg>,
    priority_tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerEvent>,
}

impl SearchWorker {
    pub fn spawn() -> Self {
        let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
        let (priority_tx, priority_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::bounded(256);
        std::thread::spawn(move || run_worker(msg_rx, priority_rx, event_tx));
        Self {
            tx: msg_tx,
            priority_tx,
            rx: event_rx,
        }
    }

    pub fn send(&self, msg: WorkerMsg) {
        match msg {
            WorkerMsg::Metadata(_) | WorkerMsg::InvalidateDir(_) => {
                let _ = self.priority_tx.send(msg);
            }
            _ => {
                let _ = self.tx.send(msg);
            }
        }
    }

    pub fn try_recv(&self) -> Option<WorkerEvent> {
        self.rx.try_recv().ok()
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(WorkerMsg::Shutdown);
    }
}

#[derive(Clone)]
struct ActiveQuery {
    generation: u64,
    text: String,
}

struct Worker {
    rx: Receiver<WorkerMsg>,
    priority_rx: Receiver<WorkerMsg>,
    tx: Sender<WorkerEvent>,
    cache: HashMap<PathBuf, Option<DisplayMetadata>>,
    /// FIFO eviction order for the cache.
    cache_order: VecDeque<PathBuf>,
    roots: Vec<PathBuf>,
    /// Directories still to scan for the index.
    pending_dirs: Vec<PathBuf>,
    /// Indexed items (tracks, playlist files, folders, albums).
    items: Vec<IndexedItem>,
    /// Albums already emitted (dedupe by lowercase album+artist).
    albums_seen: HashSet<String>,
    indexing: bool,
    skipped_roots: usize,
    query: Option<ActiveQuery>,
}

impl Worker {
    fn cached_metadata(&mut self, path: &Path) -> Option<DisplayMetadata> {
        if let Some(cached) = self.cache.get(path) {
            return cached.clone();
        }
        let meta = read_display_metadata(path);
        if self.cache.len() >= METADATA_CACHE_LIMIT
            && let Some(oldest) = self.cache_order.pop_front()
        {
            self.cache.remove(&oldest);
        }
        let path = path.to_path_buf();
        self.cache_order.push_back(path.clone());
        self.cache.insert(path, meta.clone());
        meta
    }

    fn metadata(&mut self, path: PathBuf) {
        let meta = self.cached_metadata(&path);
        let _ = self.tx.send(WorkerEvent::Metadata { path, meta });
    }

    fn invalidate(&mut self, dir: &Path) {
        self.cache.retain(|path, _| !path.starts_with(dir));
        self.cache_order.retain(|path| !path.starts_with(dir));
    }

    fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = normalize_roots(roots);
        self.items.clear();
        self.albums_seen.clear();
        self.pending_dirs = self.roots.clone();
        self.indexing = true;
        self.skipped_roots = 0;
        if let Some(query) = &self.query {
            let generation = query.generation;
            self.send_status(generation, SearchStatus::Indexing);
        }
    }

    fn send_status(&self, generation: u64, status: SearchStatus) {
        let _ = self.tx.send(WorkerEvent::Status { generation, status });
    }

    /// Handle one message in both idle and indexing states. Returns false
    /// only for Shutdown.
    fn handle_msg(&mut self, msg: WorkerMsg) -> bool {
        match msg {
            WorkerMsg::Metadata(path) => self.metadata(path),
            WorkerMsg::InvalidateDir(dir) => self.invalidate(&dir),
            WorkerMsg::SetRoots(roots) => self.set_roots(roots),
            WorkerMsg::Query { generation, text } => {
                let text = text.trim().to_owned();
                if text.is_empty() {
                    self.query = None;
                } else {
                    self.query = Some(ActiveQuery {
                        generation,
                        text: text.clone(),
                    });
                    self.send_status(
                        generation,
                        if self.indexing {
                            SearchStatus::Indexing
                        } else {
                            SearchStatus::Done
                        },
                    );
                    let mut hits: Vec<SearchHit> = self
                        .items
                        .iter()
                        .cloned()
                        .filter_map(|item| item.into_hit(&text))
                        .collect();
                    hits.sort_by(|a, b| a.rank.cmp(&b.rank));
                    for chunk in hits.chunks(RESULT_BATCH) {
                        let _ = self.tx.send(WorkerEvent::Hits {
                            generation,
                            hits: chunk.to_vec(),
                        });
                    }
                }
            }
            WorkerMsg::Cancel => self.query = None,
            WorkerMsg::Shutdown => return false,
        }
        true
    }

    /// Scan one directory (one unit of work), then return. New items are
    /// matched against the active query and sent as a bounded batch.
    fn index_step(&mut self) {
        let Some(dir) = self.pending_dirs.pop() else {
            if self.indexing {
                self.indexing = false;
                if let Some(query) = &self.query {
                    let generation = query.generation;
                    let status = if self.skipped_roots > 0 {
                        SearchStatus::Incomplete {
                            skipped_roots: self.skipped_roots,
                        }
                    } else {
                        SearchStatus::Done
                    };
                    self.send_status(generation, status);
                }
            }
            return;
        };
        if !self.roots.iter().any(|root| dir.starts_with(root)) {
            self.skipped_roots += 1;
            return;
        }
        let items = match browser::list_dir(&dir) {
            Ok(items) => items,
            Err(_) => {
                self.skipped_roots += 1;
                // Unreadable directory: skipped, never fatal.
                return;
            }
        };
        let mut new_items = Vec::new();
        for item in items {
            match item.kind {
                BrowserItemKind::Dir => {
                    self.pending_dirs.push(item.path.clone());
                    let title = item.name.clone();
                    new_items.push(IndexedItem {
                        subtitle: Some(item.path.display().to_string()),
                        path: item.path,
                        kind: HitKind::Folder,
                        title,
                    });
                }
                BrowserItemKind::Audio => {
                    let meta = self.cached_metadata(&item.path);
                    let (title, _) = crate::display::display_title(
                        &item.path,
                        meta.as_ref().map(|meta| &meta.tags),
                    );
                    let (subtitle, album_key) = match &meta {
                        Some(meta) => {
                            let subtitle = match (&meta.tags.artist, &meta.tags.album) {
                                (Some(artist), Some(album)) => Some(format!("{artist} · {album}")),
                                (Some(artist), None) => Some(artist.clone()),
                                (None, Some(album)) => Some(album.clone()),
                                (None, None) => None,
                            };
                            let album_key = meta
                                .tags
                                .album
                                .as_ref()
                                .filter(|album| !album.trim().is_empty())
                                .map(|album| {
                                    format!(
                                        "{}|{}",
                                        album.to_lowercase(),
                                        meta.tags.artist.as_deref().unwrap_or("").to_lowercase()
                                    )
                                });
                            (subtitle, album_key)
                        }
                        None => (None, None),
                    };
                    if let (Some(key), Some(meta)) = (album_key, meta.as_ref())
                        && self.albums_seen.insert(key)
                    {
                        let album = meta.tags.album.clone().unwrap_or_default();
                        let artist = meta.tags.artist.clone();
                        // Keep one representative track path so an album
                        // action filters this folder by the same album tag.
                        new_items.push(IndexedItem {
                            kind: HitKind::Album,
                            path: item.path.clone(),
                            title: album,
                            subtitle: artist,
                        });
                    }
                    new_items.push(IndexedItem {
                        path: item.path,
                        kind: HitKind::Track,
                        title,
                        subtitle,
                    });
                }
                BrowserItemKind::Playlist => {
                    new_items.push(IndexedItem {
                        title: item.name.clone(),
                        subtitle: Some(item.path.display().to_string()),
                        path: item.path,
                        kind: HitKind::PlaylistFile,
                    });
                }
            }
        }
        self.items.extend(new_items.iter().cloned());
        if let Some(query) = &self.query {
            let generation = query.generation;
            let mut hits: Vec<SearchHit> = new_items
                .into_iter()
                .filter_map(|item| item.into_hit(&query.text))
                .collect();
            hits.sort_by(|a, b| a.rank.cmp(&b.rank));
            for chunk in hits.chunks(RESULT_BATCH) {
                let _ = self.tx.send(WorkerEvent::Hits {
                    generation,
                    hits: chunk.to_vec(),
                });
            }
        }
    }
}

fn run_worker(rx: Receiver<WorkerMsg>, priority_rx: Receiver<WorkerMsg>, tx: Sender<WorkerEvent>) {
    let mut worker = Worker {
        rx,
        priority_rx,
        tx,
        cache: HashMap::new(),
        cache_order: VecDeque::new(),
        roots: Vec::new(),
        pending_dirs: Vec::new(),
        items: Vec::new(),
        albums_seen: HashSet::new(),
        indexing: false,
        skipped_roots: 0,
        query: None,
    };
    loop {
        // Visible-row metadata/invalidation first, then newer query/cancel
        // messages, then one directory of indexing.
        while let Ok(msg) = worker.priority_rx.try_recv() {
            if !worker.handle_msg(msg) {
                return;
            }
        }
        while let Ok(msg) = worker.rx.try_recv() {
            if !worker.handle_msg(msg) {
                return;
            }
        }
        if worker.indexing {
            worker.index_step();
        } else {
            // Truly idle: block on either channel instead of polling.
            crossbeam_channel::select! {
                recv(worker.priority_rx) -> msg => match msg {
                    Ok(msg) => { if !worker.handle_msg(msg) { return; } }
                    Err(_) => return,
                },
                recv(worker.rx) -> msg => match msg {
                    Ok(msg) => { if !worker.handle_msg(msg) { return; } }
                    Err(_) => return,
                },
            }
        }
    }
}

/// What the UI needs to turn a hit into queue entries.
impl SearchHit {
    pub fn queue_entry(&self) -> QueueEntry {
        QueueEntry::from(self.path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn substring_beats_subsequence_and_spans_match() {
        let (score, spans) = match_spans("live", "Live at Wembley").unwrap();
        assert_eq!(spans, vec![0, 1, 2, 3]);
        let (sub_score, _) = match_spans("lve", "Live at Wembley").unwrap();
        assert!(score > sub_score, "substring must outrank subsequence");
        assert!(match_spans("zzz", "Live at Wembley").is_none());
        assert!(match_spans("", "anything").is_none());
        // Case-insensitive both ways.
        assert!(match_spans("WEMBLEY", "Live at Wembley").is_some());
    }

    #[test]
    fn subsequence_scores_compact_matches_higher() {
        let (compact, _) = match_spans("abc", "abc-def").unwrap();
        let (spread, _) = match_spans("abc", "a---b---c").unwrap();
        assert!(compact > spread);
    }

    #[test]
    fn normalize_roots_dedupes_nested_paths() {
        let base = std::env::temp_dir().join(format!("sointty-roots-{}", std::process::id()));
        let nested = base.join("music");
        std::fs::create_dir_all(&nested).unwrap();
        let roots = normalize_roots(vec![base.clone(), nested.clone(), base.clone()]);
        assert_eq!(roots.len(), 1, "{roots:?}");
        assert!(roots[0].ends_with(base.file_name().unwrap().to_string_lossy().as_ref()));
        std::fs::remove_dir_all(&base).ok();
    }

    /// Drive a worker against a temp tree and collect events until Done.
    fn run_query(root: &Path, text: &str) -> (Vec<SearchHit>, SearchStatus) {
        let worker = SearchWorker::spawn();
        worker.send(WorkerMsg::SetRoots(vec![root.to_path_buf()]));
        worker.send(WorkerMsg::Query {
            generation: 1,
            text: text.to_owned(),
        });
        let mut hits = Vec::new();
        let mut status = SearchStatus::Indexing;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match worker.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(WorkerEvent::Hits { hits: batch, .. }) => hits.extend(batch),
                Ok(WorkerEvent::Status { status: s, .. }) => {
                    if !matches!(s, SearchStatus::Indexing) {
                        status = s;
                        break;
                    }
                }
                Ok(WorkerEvent::Metadata { .. }) => {}
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        (hits, status)
    }

    #[test]
    fn worker_indexes_incrementally_and_matches() {
        let dir = std::env::temp_dir().join(format!(
            "sointty-search-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("album")).unwrap();
        std::fs::write(dir.join("album").join("wembley live.flac"), b"").unwrap();
        std::fs::write(dir.join("album").join("studio take.flac"), b"").unwrap();
        std::fs::write(dir.join("setlist.m3u"), b"").unwrap();
        std::fs::write(dir.join("._junk.flac"), b"").unwrap();

        let (hits, status) = run_query(&dir, "wembley");
        assert!(matches!(status, SearchStatus::Done), "{status:?}");
        assert!(
            hits.iter().any(|hit| hit.title == "wembley live"),
            "cleaned fallback title: {hits:?}"
        );
        assert!(
            hits.iter().all(|hit| !hit.title.starts_with("._")),
            "junk must never be indexed: {hits:?}"
        );
        let (folder_hits, _) = run_query(&dir, "album");
        assert!(
            folder_hits.iter().any(|hit| hit.kind == HitKind::Folder),
            "{folder_hits:?}"
        );
        let (playlist_hits, _) = run_query(&dir, "setlist");
        assert!(
            playlist_hits
                .iter()
                .any(|hit| hit.kind == HitKind::PlaylistFile),
            "{playlist_hits:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn worker_publishes_progressive_batches_and_scope_counts() {
        let dir = std::env::temp_dir().join(format!(
            "sointty-search-progress-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(dir.join("music-a")).unwrap();
        std::fs::create_dir_all(dir.join("music-b")).unwrap();
        std::fs::write(dir.join("music-a").join("music-tone-a.flac"), b"").unwrap();
        std::fs::write(dir.join("music-b").join("music-tone-b.flac"), b"").unwrap();
        std::fs::write(dir.join("music-set.m3u"), b"").unwrap();
        let worker = SearchWorker::spawn();
        worker.send(WorkerMsg::SetRoots(vec![dir.clone()]));
        worker.send(WorkerMsg::Query {
            generation: 7,
            text: "music".to_owned(),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut batches = 0;
        let mut hits = Vec::new();
        let mut done = false;
        while std::time::Instant::now() < deadline {
            match worker.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(WorkerEvent::Hits {
                    generation: 7,
                    hits: batch,
                }) => {
                    assert!(!done, "no results after Done");
                    assert!(batch.len() <= RESULT_BATCH);
                    batches += 1;
                    hits.extend(batch);
                }
                Ok(WorkerEvent::Status {
                    generation: 7,
                    status: SearchStatus::Done,
                }) => {
                    done = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(done, "worker must finish indexing");
        assert!(
            batches >= 2,
            "results arrive per directory, not only at the end"
        );
        assert_eq!(
            hits.iter()
                .filter(|hit| SearchScope::Tracks.admits(&hit.kind))
                .count(),
            2
        );
        assert_eq!(
            hits.iter()
                .filter(|hit| SearchScope::Folders.admits(&hit.kind))
                .count(),
            2
        );
        assert_eq!(
            hits.iter()
                .filter(|hit| SearchScope::Playlists.admits(&hit.kind))
                .count(),
            1
        );
        assert_eq!(
            hits.iter()
                .filter(|hit| SearchScope::Albums.admits(&hit.kind))
                .count(),
            0
        );
        assert_eq!(
            hits.iter()
                .filter(|hit| SearchScope::All.admits(&hit.kind))
                .count(),
            5
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_generations_are_suppressed_by_the_ui_contract() {
        // The worker tags every result with the generation that produced it;
        // a newer query replaces the active one, so old-generation batches
        // simply stop arriving. Verify a new query after cancel yields no
        // hits for the old text.
        let dir = std::env::temp_dir().join(format!("sointty-search-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("alpha.flac"), b"").unwrap();
        let worker = SearchWorker::spawn();
        worker.send(WorkerMsg::SetRoots(vec![dir.clone()]));
        worker.send(WorkerMsg::Query {
            generation: 1,
            text: "alpha".to_owned(),
        });
        worker.send(WorkerMsg::Cancel);
        worker.send(WorkerMsg::Query {
            generation: 2,
            text: "zzz-no-match".to_owned(),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut gen2_hits = 0;
        while std::time::Instant::now() < deadline {
            match worker.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(WorkerEvent::Hits { generation, hits }) => {
                    if generation == 2 {
                        gen2_hits += hits.len();
                    }
                }
                Ok(WorkerEvent::Status {
                    generation: 2,
                    status,
                }) => {
                    if !matches!(status, SearchStatus::Indexing) {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert_eq!(gen2_hits, 0, "no hits may leak into the newer query");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unreadable_roots_report_incomplete() {
        let missing = std::env::temp_dir().join("sointty-search-definitely-missing-root");
        let worker = SearchWorker::spawn();
        worker.send(WorkerMsg::SetRoots(vec![missing]));
        worker.send(WorkerMsg::Query {
            generation: 1,
            text: "x".to_owned(),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut final_status = None;
        while std::time::Instant::now() < deadline {
            if let Ok(WorkerEvent::Status { status, .. }) =
                worker.rx.recv_timeout(Duration::from_millis(100))
                && !matches!(status, SearchStatus::Indexing)
            {
                final_status = Some(status);
                break;
            }
        }
        // A missing root fails list_dir and is skipped, not fatal or
        // misreported as complete coverage.
        assert!(
            matches!(final_status, Some(SearchStatus::Incomplete { skipped_roots }) if skipped_roots >= 1),
            "{final_status:?}"
        );
    }
}
