//! Terminal UI for Sointty. The player engine owns playback and queue truth;
//! this crate owns only control-plane input, presentation, and UI-only state.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use sointty_core::{
    BufferConfig, DecodedSpec, DeviceFormat, DeviceId, OutputSpec, PlayerCommand, PlayerEvent,
    QueueEntry, QueueItem, TrackId, TrackTags,
};
use sointty_decode::tags::DisplayMetadata;

mod browser;
pub mod display;
pub mod keymap;
pub mod layout;
pub mod screens;
pub mod search;
pub mod theme;
pub mod widgets;

pub use browser::collect_audio_files;
use browser::{Browser, BrowserItemKind};
use keymap::{Action, Focus, ModeKind, Screen};
use screens::MetadataView;
use search::{HitKind, SearchHit, SearchScope, SearchStatus, SearchWorker, WorkerEvent, WorkerMsg};
use theme::Theme;

/// Lists selectable output devices as `(id, display name)` pairs; invoked
/// when Output settings opens so hot-plugged endpoints appear.
pub type DeviceProvider = Box<dyn FnMut() -> Vec<(DeviceId, String)>>;
/// Persists a device selection; errors stay visible in the status line.
pub type DeviceSaver = Box<dyn FnMut(&DeviceId) -> Result<(), String>>;
/// Resolves the platform default, persists `device = None`, and returns its
/// real ID. The UI selects that ID only after this hook succeeds.
pub type DeviceReset = Box<dyn FnMut() -> Result<DeviceId, String>>;
/// Persists an explicit non-bit-perfect compatibility choice.
pub type FloatCompatibilitySaver = Box<dyn FnMut(bool) -> Result<(), String>>;
/// Persists the last successfully browsed directory.
pub type BrowserDirSaver = Box<dyn FnMut(&Path) -> Result<(), String>>;
/// Atomically persists the whole named-playlist catalog on explicit save.
pub type PlaylistSaver = Box<dyn FnMut(&PlaylistCatalog) -> Result<(), String>>;
/// Persists DAC timing overrides; `None` clears a field back to Auto.
pub type TimingSaver = Box<dyn FnMut(Option<u32>, Option<u32>) -> Result<(), String>>;
/// Persists a validated `#RRGGBB` accent, or `None` to reset it.
pub type AccentSaver = Box<dyn FnMut(Option<&str>) -> Result<(), String>>;

/// A browsable mount root.
#[derive(Debug, Clone)]
pub struct Location {
    pub label: String,
    pub path: PathBuf,
    pub mapped_network: bool,
}

pub type LocationProvider = Box<dyn FnMut() -> Result<Vec<Location>, String>>;
pub type MapDrive = Box<dyn FnMut(char, &str) -> Result<PathBuf, String>>;
pub type DisconnectDrive = Box<dyn FnMut(char) -> Result<(), String>>;
pub type MountShare = Box<dyn FnMut(&str) -> Result<(), String>>;

const SEEK_SECONDS: u64 = 10;

/// A named persisted playlist. Its entries are independent of the live queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPlaylist {
    pub name: String,
    pub entries: Vec<QueueEntry>,
}

/// All named playlists plus the selected one. At least one list exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistCatalog {
    pub selected: usize,
    pub playlists: Vec<SavedPlaylist>,
}

impl Default for PlaylistCatalog {
    fn default() -> Self {
        Self {
            selected: 0,
            playlists: vec![SavedPlaylist {
                name: "Default".to_owned(),
                entries: Vec::new(),
            }],
        }
    }
}

impl PlaylistCatalog {
    fn selected_playlist(&self) -> &SavedPlaylist {
        &self.playlists[self.selected.min(self.playlists.len().saturating_sub(1))]
    }

    fn name_taken(&self, name: &str) -> bool {
        let needle = name.trim().to_lowercase();
        self.playlists
            .iter()
            .any(|playlist| playlist.name.to_lowercase() == needle)
    }
}

/// Everything `run` needs that is not a callback.
pub struct RunOptions {
    pub open_browser: bool,
    pub initial_browser_dir: Option<PathBuf>,
    pub home_dir: Option<PathBuf>,
    pub current_device: DeviceId,
    pub float_to_int: bool,
    pub int_to_float: bool,
    /// Persisted preference (not an active-playback claim); off by default.
    pub shared_playback: bool,
    /// DAC timing overrides from CLI/config (None = Auto).
    pub timing: (Option<u32>, Option<u32>),
    pub catalog: PlaylistCatalog,
    /// Full path plus parse error when the catalog file is malformed.
    pub catalog_error: Option<String>,
    pub catalog_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    /// Startup TOML error; strict savers leave the file untouched.
    pub config_error: Option<String>,
    /// Optional validated-or-invalid `#RRGGBB` from config; the theme
    /// falls back on invalid values and reports feedback without saving.
    pub accent: Option<String>,
    /// Filesystem search roots supplied by the app's location provider.
    pub search_roots: Vec<PathBuf>,
    /// Location-provider failure; fallback roots still work but coverage
    /// must not be represented as complete.
    pub search_coverage_error: Option<String>,
}

/// Callbacks into the app layer; OS-specific actions are `None` where the
/// platform does not support them.
pub struct Hooks {
    pub device_provider: DeviceProvider,
    pub on_device_selected: DeviceSaver,
    pub on_device_reset: DeviceReset,
    pub on_float_to_int_selected: FloatCompatibilitySaver,
    pub on_int_to_float_selected: FloatCompatibilitySaver,
    pub on_shared_playback_selected: FloatCompatibilitySaver,
    pub on_browser_dir: BrowserDirSaver,
    pub on_catalog: PlaylistSaver,
    pub on_timing: TimingSaver,
    pub on_accent: AccentSaver,
    pub locations: LocationProvider,
    pub map_drive: Option<MapDrive>,
    pub disconnect_drive: Option<DisconnectDrive>,
    pub mount_share: Option<MountShare>,
}

/// Playback and settings values mirrored from the player and persistent
/// config. Queue snapshots and `Playing` events remain the only truth for
/// audible/pending order and the active output verdict.
pub struct TuiState {
    pub status: String,
    pub frame: u64,
    pub rate_hz: u32,
    total_frames: Option<u64>,
    pub paused: bool,
    pub tags: TrackTags,
    current_track: TrackId,
    device: DeviceId,
    device_names: Vec<(DeviceId, String)>,
    float_to_int: bool,
    int_to_float: bool,
    converted: bool,
    /// Persisted access preference. The active stream is reported only by
    /// `Playing.shared_playback`, never inferred from this value.
    shared_playback: bool,
    shared_active: bool,
    output_spec: Option<OutputSpec>,
    source_spec: Option<DecodedSpec>,
    buffers: Option<BufferConfig>,
    timing: (Option<u32>, Option<u32>),
    audible: Option<QueueItem>,
    pending: Vec<QueueItem>,
    shuffle: bool,
    catalog: PlaylistCatalog,
    catalog_error: Option<String>,
    catalog_path: Option<PathBuf>,
    config_path: Option<PathBuf>,
    config_error: Option<String>,
    accent: Option<String>,
    last_browser_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    search_roots: Vec<PathBuf>,
    search_coverage_error: Option<String>,
}

impl TuiState {
    fn from_options(options: RunOptions) -> Self {
        Self {
            status: "idle".to_owned(),
            frame: 0,
            rate_hz: 44_100,
            total_frames: None,
            paused: false,
            tags: TrackTags::default(),
            current_track: 0,
            device: options.current_device,
            device_names: Vec::new(),
            float_to_int: options.float_to_int,
            int_to_float: options.int_to_float,
            converted: false,
            shared_playback: options.shared_playback,
            shared_active: false,
            output_spec: None,
            source_spec: None,
            buffers: None,
            timing: options.timing,
            audible: None,
            pending: Vec::new(),
            shuffle: false,
            catalog: options.catalog,
            catalog_error: options.catalog_error,
            catalog_path: options.catalog_path,
            config_path: options.config_path,
            config_error: options.config_error,
            accent: options.accent,
            last_browser_dir: options.initial_browser_dir,
            home_dir: options.home_dir,
            search_roots: options.search_roots,
            search_coverage_error: options.search_coverage_error,
        }
    }

    fn device_display(&self) -> &str {
        self.device_names
            .iter()
            .find(|(id, _)| *id == self.device)
            .map(|(_, name)| name.as_str())
            .unwrap_or(&self.device)
    }
}

#[derive(Debug)]
enum InputAction {
    LocationPath,
    MapDriveLetter,
    MapDriveShare(char),
    MountShare,
    PlaylistCreate,
    PlaylistRename,
    PlaylistImportPath,
    PlaylistImportName(PathBuf),
    TimingPeriod,
    TimingBuffer(u32),
    Accent,
}

struct Prompt {
    label: String,
    buffer: String,
    kind: PromptKind,
    origin: Box<Mode>,
}

enum PromptKind {
    Text(InputAction),
    QuitConfirm,
}

struct LocationsOverlay {
    locations: Vec<Location>,
    selected: usize,
}

struct PlaylistPicker {
    entries: Vec<QueueEntry>,
    selected: usize,
}

#[derive(Clone)]
enum SearchItem {
    Worker(SearchHit),
    Catalog {
        index: usize,
        rank: (i64, String, String),
        spans: Vec<usize>,
    },
}

impl SearchItem {
    fn rank(&self) -> &(i64, String, String) {
        match self {
            Self::Worker(hit) => &hit.rank,
            Self::Catalog { rank, .. } => rank,
        }
    }

    fn kind(&self) -> HitKind {
        match self {
            Self::Worker(hit) => hit.kind.clone(),
            Self::Catalog { .. } => HitKind::PlaylistFile,
        }
    }
}

struct SearchOverlay {
    query: String,
    scope: SearchScope,
    generation: u64,
    results: Vec<SearchItem>,
    visible: Vec<usize>,
    selected: usize,
    status: SearchStatus,
}

impl SearchOverlay {
    fn refresh_visible(&mut self) {
        self.results.sort_by(|a, b| a.rank().cmp(b.rank()));
        self.visible = self
            .results
            .iter()
            .enumerate()
            .filter_map(|(index, item)| self.scope.admits(&item.kind()).then_some(index))
            .collect();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
    }

    fn selected_item(&self) -> Option<&SearchItem> {
        self.visible
            .get(self.selected)
            .and_then(|&index| self.results.get(index))
    }

    fn display_rows(&self) -> Vec<SearchDisplayRow> {
        const GROUPS: &[(HitKind, &str)] = &[
            (HitKind::Album, "Albums"),
            (HitKind::Track, "Tracks"),
            (HitKind::Folder, "Folders"),
            (HitKind::PlaylistFile, "Playlists"),
        ];
        let mut rows = Vec::new();
        for (kind, label) in GROUPS {
            let members: Vec<usize> = self
                .visible
                .iter()
                .copied()
                .filter(|&index| self.results[index].kind() == *kind)
                .collect();
            if members.is_empty() {
                continue;
            }
            rows.push(SearchDisplayRow::Header(label, members.len()));
            let selected = self.visible.get(self.selected).copied();
            let selected_in_tail = members.iter().skip(5).any(|&index| Some(index) == selected);
            let shown = if members.len() > 5 && !selected_in_tail {
                5
            } else {
                members.len()
            };
            for index in members.iter().take(shown) {
                rows.push(SearchDisplayRow::Item(*index));
            }
            if shown < members.len() {
                rows.push(SearchDisplayRow::More(members.len() - shown));
            }
        }
        rows
    }
}

enum SearchDisplayRow {
    Header(&'static str, usize),
    Item(usize),
    More(usize),
}

enum MoveOverlay {
    Playlist {
        indices: Vec<usize>,
        gap: usize,
    },
    Queue {
        id: TrackId,
        target: Option<TrackId>,
    },
}

#[derive(Clone)]
enum ActionItem {
    EnterPath,
    RefreshLocations,
    MapDrive,
    MountShare,
    Disconnect(char),
    PlaylistCreate,
    PlaylistRename,
    PlaylistDelete,
    PlaylistImport,
    QueueCurrentFolder,
    Quit,
}

struct ActionsOverlay {
    items: Vec<(String, ActionItem)>,
    selected: usize,
    origin: Box<Mode>,
}

enum Mode {
    Normal,
    Search(SearchOverlay),
    Move(MoveOverlay),
    Prompt(Prompt),
    Help,
    Locations(LocationsOverlay),
    PlaylistPicker(PlaylistPicker),
    Actions(ActionsOverlay),
}

impl Mode {
    fn kind(&self) -> ModeKind {
        match self {
            Self::Normal => ModeKind::Normal,
            Self::Search(_) => ModeKind::Search,
            Self::Move(_) => ModeKind::Move,
            Self::Prompt(Prompt {
                kind: PromptKind::QuitConfirm,
                ..
            }) => ModeKind::QuitConfirm,
            Self::Prompt(_) => ModeKind::Prompt,
            Self::Help => ModeKind::Help,
            Self::Locations(_) => ModeKind::Locations,
            Self::PlaylistPicker(_) => ModeKind::PlaylistPicker,
            Self::Actions(_) => ModeKind::Actions,
        }
    }
}

enum OsAction {
    MapDrive(char, String),
    MountShare(String),
}

enum CollectTarget {
    Enqueue { play_first: bool },
    OpenPicker,
}

struct CollectOp {
    generation: u64,
    target: CollectTarget,
    rx: Receiver<io::Result<Vec<QueueEntry>>>,
}

struct App {
    state: TuiState,
    theme: Theme,
    screen: Screen,
    focus: Focus,
    mode: Mode,
    help_scroll: usize,
    browser: Option<Browser>,
    commands: Sender<PlayerCommand>,
    hooks: Hooks,
    pending_os: Option<OsAction>,
    worker: SearchWorker,
    metadata: HashMap<PathBuf, Option<DisplayMetadata>>,
    metadata_order: VecDeque<PathBuf>,
    metadata_requested: HashSet<PathBuf>,
    search_generation: u64,
    past: VecDeque<QueueItem>,
    queue_rows: Vec<screens::queue::QueueRow>,
    pending_duration_total: Option<Duration>,
    queue_selected: Option<TrackId>,
    playlist_name_sel: usize,
    playlist_track_sel: usize,
    playlist_names_cache: Vec<screens::playlists::NameRow>,
    playlist_tracks_cache: Vec<screens::playlists::TrackRow>,
    playlist_selected: HashSet<usize>,
    playlist_anchor: usize,
    dirty_playlists: HashSet<usize>,
    catalog_dirty: bool,
    deleted_name: Option<String>,
    settings_section: usize,
    settings_row: usize,
    settings_rows_cache: Vec<screens::settings::SettingsRowView>,
    device_candidates: Vec<(DeviceId, String)>,
    device_candidate_sel: usize,
    collect_generation: u64,
    collect_op: Option<CollectOp>,
}

impl MetadataView for App {
    fn metadata(&self, path: &Path) -> Option<&DisplayMetadata> {
        self.metadata.get(path).and_then(Option::as_ref)
    }
}

impl App {
    fn new(options: RunOptions, hooks: Hooks, commands: Sender<PlayerCommand>) -> Self {
        let open_browser = options.open_browser;
        let (theme, warning) = Theme::detect(options.accent.as_deref());
        let mut app = Self {
            state: TuiState::from_options(options),
            theme,
            screen: Screen::Library,
            focus: Focus::Library,
            mode: Mode::Normal,
            help_scroll: 0,
            browser: None,
            commands,
            hooks,
            pending_os: None,
            worker: SearchWorker::spawn(),
            metadata: HashMap::new(),
            metadata_order: VecDeque::new(),
            metadata_requested: HashSet::new(),
            search_generation: 0,
            past: VecDeque::new(),
            queue_rows: Vec::new(),
            pending_duration_total: Some(Duration::ZERO),
            queue_selected: None,
            playlist_name_sel: 0,
            playlist_track_sel: 0,
            playlist_names_cache: Vec::new(),
            playlist_tracks_cache: Vec::new(),
            playlist_selected: HashSet::new(),
            playlist_anchor: 0,
            dirty_playlists: HashSet::new(),
            catalog_dirty: false,
            deleted_name: None,
            settings_section: 0,
            settings_row: 0,
            settings_rows_cache: Vec::new(),
            device_candidates: Vec::new(),
            device_candidate_sel: 0,
            collect_generation: 0,
            collect_op: None,
        };
        app.state.device_names = (app.hooks.device_provider)();
        app.playlist_name_sel = app.state.catalog.selected;
        app.rebuild_playlist_rows();
        app.rebuild_settings_rows();
        if open_browser {
            app.open_browser_startup();
        }
        if let Some(warning) = warning {
            app.state.status = warning;
        }
        if let Some(error) = &app.state.search_coverage_error {
            app.state.status = format!("search coverage incomplete: {error}");
        }
        app
    }

    /// Saved directory, then Home, then platform root. Never use cwd as
    /// the default browser location.
    fn open_browser_startup(&mut self) {
        let mut candidates = Vec::new();
        if let Some(saved) = &self.state.last_browser_dir {
            candidates.push(saved.clone());
        }
        if let Some(home) = &self.state.home_dir {
            candidates.push(home.clone());
        }
        #[cfg(not(windows))]
        candidates.push(PathBuf::from("/"));
        #[cfg(windows)]
        for letter in b'A'..=b'Z' {
            candidates.push(PathBuf::from(format!("{}:\\", letter as char)));
        }
        for path in candidates {
            match Browser::open(path.clone()) {
                Ok(browser) => {
                    self.browser = Some(browser);
                    return;
                }
                Err(error) => {
                    if Some(&path) == self.state.last_browser_dir.as_ref() {
                        self.state.status = format!(
                            "saved folder {} is unreadable ({error}); falling back",
                            path.display()
                        );
                    }
                }
            }
        }
        self.state.status = "no readable folder found; press l to enter a location".to_owned();
    }

    fn remember_browser_dir(&mut self, dir: &Path) {
        self.state.last_browser_dir = Some(dir.to_path_buf());
        if let Err(error) = (self.hooks.on_browser_dir)(dir) {
            self.state.status = format!(
                "folder saved for this session only; next launch may not restore it: {error}"
            );
        } else {
            self.state.config_error = None;
        }
    }

    fn open_browser(&mut self, path: PathBuf) -> bool {
        match Browser::open(path.clone()) {
            Ok(browser) => {
                self.browser = Some(browser);
                self.screen = Screen::Library;
                self.focus = Focus::Library;
                self.mode = Mode::Normal;
                self.invalidate_dir(&path);
                self.remember_browser_dir(&path);
                true
            }
            Err(error) => {
                self.state.status = format!("cannot open {}: {error}", path.display());
                false
            }
        }
    }

    fn invalidate_dir(&mut self, dir: &Path) {
        self.worker
            .send(WorkerMsg::InvalidateDir(dir.to_path_buf()));
        self.metadata.retain(|path, _| !path.starts_with(dir));
        self.metadata_order.retain(|path| !path.starts_with(dir));
        self.metadata_requested
            .retain(|path| !path.starts_with(dir));
    }

    fn prime_metadata(&mut self, visible_height: usize) {
        let mut paths = Vec::new();
        match self.screen {
            Screen::Library => {
                if self.focus == Focus::Library
                    && let Some(browser) = &self.browser
                {
                    let (start, end) = screens::visible_window(
                        browser.items.len(),
                        browser.selected,
                        visible_height,
                    );
                    for item in &browser.items[start..end] {
                        if item.kind == BrowserItemKind::Audio {
                            paths.push(item.path.clone());
                        }
                    }
                }
                for item in self
                    .state
                    .audible
                    .iter()
                    .chain(self.state.pending.iter())
                    .take(visible_height)
                {
                    paths.push(item.entry.path.clone());
                }
            }
            Screen::Playlists => {
                if let Some(playlist) = self.state.catalog.playlists.get(self.playlist_name_sel) {
                    let (start, end) = screens::visible_window(
                        playlist.entries.len(),
                        self.playlist_track_sel,
                        visible_height,
                    );
                    for entry in &playlist.entries[start..end] {
                        paths.push(entry.path.clone());
                    }
                }
            }
            Screen::Settings => {}
        }
        for path in paths {
            if !self.metadata.contains_key(&path) && self.metadata_requested.insert(path.clone()) {
                self.worker.send(WorkerMsg::Metadata(path));
            }
        }
    }

    /// Rebuild presentation rows only when catalog or metadata changes.
    /// Progress ticks reuse these rows and render only the visible window.
    fn rebuild_playlist_rows(&mut self) {
        self.playlist_names_cache = self
            .state
            .catalog
            .playlists
            .iter()
            .enumerate()
            .map(|(index, playlist)| {
                let mut total = Duration::ZERO;
                let mut known = true;
                for entry in &playlist.entries {
                    let duration = match entry.cue_range {
                        Some(range) => range.end_cd.map(|end| {
                            Duration::from_millis(end.saturating_sub(range.start_cd) * 1_000 / 75)
                        }),
                        None => self
                            .metadata
                            .get(&entry.path)
                            .and_then(Option::as_ref)
                            .and_then(|meta| meta.duration),
                    };
                    if let Some(duration) = duration {
                        total += duration;
                    } else {
                        known = false;
                    }
                }
                screens::playlists::NameRow {
                    name: playlist.name.clone(),
                    count: playlist.entries.len(),
                    dirty: self.dirty_playlists.contains(&index),
                    catalog_selected: index == self.state.catalog.selected,
                    duration: known.then_some(total),
                }
            })
            .collect();
        self.playlist_tracks_cache = self
            .state
            .catalog
            .playlists
            .get(self.playlist_name_sel)
            .map(|list| {
                list.entries
                    .iter()
                    .map(|entry| {
                        let meta = self.metadata.get(&entry.path).and_then(Option::as_ref);
                        let (title, tag) =
                            display::display_entry_title(entry, meta.map(|meta| &meta.tags));
                        let title = if let Some(tag) = tag {
                            format!("{title} ({tag})")
                        } else {
                            title
                        };
                        let duration = match entry.cue_range {
                            Some(range) => range.end_cd.map(|end| {
                                Duration::from_millis(
                                    end.saturating_sub(range.start_cd) * 1_000 / 75,
                                )
                            }),
                            None => meta.and_then(|meta| meta.duration),
                        };
                        screens::playlists::TrackRow {
                            number: meta
                                .and_then(|meta| meta.track_number)
                                .or_else(|| display::fallback_track_number(&entry.path)),
                            title,
                            album: meta.and_then(|meta| meta.tags.album.clone()),
                            duration,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
    }

    fn poll_worker(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self.worker.try_recv() {
            match event {
                WorkerEvent::Metadata { path, meta } => {
                    if !self.metadata.contains_key(&path) {
                        if self.metadata.len() >= search::METADATA_CACHE_LIMIT
                            && let Some(oldest) = self.metadata_order.pop_front()
                        {
                            self.metadata.remove(&oldest);
                            self.metadata_requested.remove(&oldest);
                        }
                        self.metadata_order.push_back(path.clone());
                    }
                    self.metadata.insert(path, meta);
                    self.rebuild_queue_rows();
                    self.rebuild_playlist_rows();
                    changed = true;
                }
                WorkerEvent::Hits { generation, hits } => {
                    if let Mode::Search(search) = &mut self.mode
                        && search.generation == generation
                    {
                        search
                            .results
                            .extend(hits.into_iter().map(SearchItem::Worker));
                        search.refresh_visible();
                        changed = true;
                    }
                }
                WorkerEvent::Status { generation, status } => {
                    if let Mode::Search(search) = &mut self.mode
                        && search.generation == generation
                    {
                        search.status = status;
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    fn start_collect(&mut self, dir: PathBuf, target: CollectTarget) {
        self.collect_generation = self.collect_generation.wrapping_add(1);
        let generation = self.collect_generation;
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let result = collect_audio_files(&dir)
                .map(|paths| paths.into_iter().map(QueueEntry::from).collect::<Vec<_>>());
            let _ = tx.send(result);
        });
        self.collect_op = Some(CollectOp {
            generation,
            target,
            rx,
        });
        self.state.status = "collecting folder…".to_owned();
    }

    fn start_album_collect(&mut self, path: PathBuf, target: CollectTarget) {
        self.collect_generation = self.collect_generation.wrapping_add(1);
        let generation = self.collect_generation;
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let result = (|| -> io::Result<Vec<QueueEntry>> {
                let parent = path
                    .parent()
                    .ok_or_else(|| io::Error::other("no parent folder"))?;
                let selected_album = sointty_decode::tags::read_display_metadata(&path)
                    .and_then(|meta| meta.tags.album)
                    .filter(|album| !album.trim().is_empty());
                let mut entries = Vec::new();
                for item in browser::list_dir(parent)? {
                    if item.kind != BrowserItemKind::Audio {
                        continue;
                    }
                    let same_album = match &selected_album {
                        Some(album) => sointty_decode::tags::read_display_metadata(&item.path)
                            .and_then(|meta| meta.tags.album)
                            .is_some_and(|candidate| candidate == *album),
                        None => true,
                    };
                    if same_album {
                        entries.push(QueueEntry::from(item.path));
                    }
                }
                Ok(entries)
            })();
            let _ = tx.send(result);
        });
        self.collect_op = Some(CollectOp {
            generation,
            target,
            rx,
        });
        self.state.status = "collecting album…".to_owned();
    }

    fn poll_collect(&mut self) -> bool {
        let Some(op) = self.collect_op.as_ref() else {
            return false;
        };
        let Ok(result) = op.rx.try_recv() else {
            return false;
        };
        let op = self.collect_op.take().expect("op still exists");
        if op.generation != self.collect_generation {
            return false; // stale/canceled operation
        }
        match result {
            Ok(entries) if entries.is_empty() => {
                self.state.status = "no audio files found".to_owned();
            }
            Ok(entries) => match op.target {
                CollectTarget::Enqueue { play_first } => {
                    if play_first {
                        self.play_entries(entries);
                    } else {
                        self.enqueue_entries(entries);
                    }
                }
                CollectTarget::OpenPicker => {
                    self.mode = Mode::PlaylistPicker(PlaylistPicker {
                        entries,
                        selected: 0,
                    });
                }
            },
            Err(error) => self.state.status = format!("nothing added: {error}"),
        }
        true
    }

    fn enqueue_entries(&mut self, entries: Vec<QueueEntry>) {
        if entries.is_empty() {
            self.state.status = "no tracks to queue".to_owned();
            return;
        }
        let count = entries.len();
        for entry in entries {
            let _ = self.commands.send(PlayerCommand::Enqueue(entry));
        }
        self.state.status = format!("queued {count} track(s)");
    }

    fn play_entries(&mut self, mut entries: Vec<QueueEntry>) {
        if entries.is_empty() {
            self.state.status = "no tracks to play".to_owned();
            return;
        }
        let count = entries.len();
        let first = entries.remove(0);
        let _ = self.commands.send(PlayerCommand::PlayNow(first));
        for entry in entries {
            let _ = self.commands.send(PlayerCommand::Enqueue(entry));
        }
        self.state.paused = false;
        self.state.status = format!("playing first of {count} track(s)");
    }

    fn selected_browser_item(&self) -> Option<browser::BrowserItem> {
        self.browser.as_ref()?.selected_item().cloned()
    }

    fn import_entries(&mut self, path: &Path) -> Option<Vec<QueueEntry>> {
        match sointty_playlist::read(path) {
            Ok(entries) if entries.is_empty() => {
                self.state.status = "playlist contains no tracks".to_owned();
                None
            }
            Ok(entries) => Some(
                entries
                    .into_iter()
                    .map(|entry| QueueEntry {
                        path: entry.path,
                        cue_range: entry.cue_range,
                    })
                    .collect(),
            ),
            Err(error) => {
                self.state.status = format!("playlist import failed: {error}");
                None
            }
        }
    }

    fn browser_activate(&mut self) {
        let Some(item) = self.selected_browser_item() else {
            return;
        };
        match item.kind {
            BrowserItemKind::Dir => {
                let result = self
                    .browser
                    .as_mut()
                    .map(|browser| browser.descend(item.path.clone()));
                match result {
                    Some(Ok(())) => {
                        self.invalidate_dir(&item.path);
                        self.remember_browser_dir(&item.path);
                    }
                    Some(Err(error)) => {
                        self.state.status = format!("cannot open {}: {error}", item.name)
                    }
                    None => {
                        self.open_browser(item.path);
                    }
                }
            }
            BrowserItemKind::Audio => self.play_entries(vec![QueueEntry::from(item.path)]),
            BrowserItemKind::Playlist => {
                if let Some(entries) = self.import_entries(&item.path) {
                    self.play_entries(entries);
                }
            }
        }
    }

    fn browser_enqueue(&mut self, whole_album: bool) {
        let Some(item) = self.selected_browser_item() else {
            self.state.status = "select a library item first".to_owned();
            return;
        };
        match item.kind {
            BrowserItemKind::Audio if whole_album => {
                self.start_album_collect(item.path, CollectTarget::Enqueue { play_first: false })
            }
            BrowserItemKind::Audio => self.enqueue_entries(vec![QueueEntry::from(item.path)]),
            BrowserItemKind::Playlist => {
                if let Some(entries) = self.import_entries(&item.path) {
                    self.enqueue_entries(entries);
                }
            }
            BrowserItemKind::Dir => {
                self.start_collect(item.path, CollectTarget::Enqueue { play_first: false })
            }
        }
    }

    fn browser_up(&mut self) {
        let Some(browser) = &mut self.browser else {
            self.open_locations();
            return;
        };
        let at_root = browser
            .current_dir
            .parent()
            .is_none_or(|parent| parent == browser.current_dir);
        if at_root {
            self.open_locations();
            return;
        }
        match browser.ascend() {
            Ok(()) => {
                let dir = browser.current_dir.clone();
                self.invalidate_dir(&dir);
                self.remember_browser_dir(&dir);
            }
            Err(error) => self.state.status = format!("cannot go up: {error}"),
        }
    }

    fn open_locations(&mut self) {
        match (self.hooks.locations)() {
            Ok(locations) => {
                self.mode = Mode::Locations(LocationsOverlay {
                    locations,
                    selected: 0,
                })
            }
            Err(error) => self.state.status = format!("locations: {error}"),
        }
    }

    fn open_output_settings(&mut self) {
        self.screen = Screen::Settings;
        self.focus = Focus::SettingsRows;
        self.settings_section = 0;
        self.settings_row = 0;
        self.refresh_devices();
    }

    fn refresh_devices(&mut self) {
        self.device_candidates = (self.hooks.device_provider)();
        if self.device_candidates.is_empty() {
            self.state.status = if cfg!(target_os = "linux") && self.state.shared_playback {
                "no direct hardware endpoints; shared uses system default".to_owned()
            } else {
                "no output devices found".to_owned()
            };
            self.device_candidate_sel = 0;
            self.rebuild_settings_rows();
            return;
        }
        self.state.device_names = self.device_candidates.clone();
        self.device_candidate_sel = self
            .device_candidates
            .iter()
            .position(|(id, _)| *id == self.state.device)
            .unwrap_or(0);
        self.rebuild_settings_rows();
    }

    fn apply_device_candidate(&mut self) {
        let Some((id, name)) = self
            .device_candidates
            .get(self.device_candidate_sel)
            .cloned()
        else {
            self.state.status = if cfg!(target_os = "linux") && self.state.shared_playback {
                "no direct hardware endpoints; shared uses system default".to_owned()
            } else {
                "no output devices found".to_owned()
            };
            return;
        };
        match (self.hooks.on_device_selected)(&id) {
            Ok(()) => {
                self.state.device = id.clone();
                self.state.config_error = None;
                let _ = self.commands.send(PlayerCommand::SelectDevice(id));
                self.state.status = format!("output device: {name}");
                self.rebuild_settings_rows();
            }
            Err(error) => self.state.status = format!("device not saved: {error}"),
        }
    }

    fn reset_device(&mut self) {
        match (self.hooks.on_device_reset)() {
            Ok(id) => {
                self.state.device = id.clone();
                self.state.config_error = None;
                let _ = self.commands.send(PlayerCommand::SelectDevice(id.clone()));
                self.state.status = format!("output device: platform default ({id})");
                self.refresh_devices();
            }
            Err(error) => self.state.status = format!("default device not saved: {error}"),
        }
    }

    fn toggle_float_to_int(&mut self) {
        let enabled = !self.state.float_to_int;
        if let Err(error) = (self.hooks.on_float_to_int_selected)(enabled) {
            self.state.status = format!("F32 compatibility not saved: {error}");
            return;
        }
        self.state.config_error = None;
        self.state.float_to_int = enabled;
        let _ = self.commands.send(PlayerCommand::SetFloatToInt(enabled));
        self.state.status = "F32→integer compatibility applies on next configure".to_owned();
        self.rebuild_settings_rows();
    }

    fn toggle_int_to_float(&mut self) {
        let enabled = !self.state.int_to_float;
        if let Err(error) = (self.hooks.on_int_to_float_selected)(enabled) {
            self.state.status = format!("integer-to-F32 compatibility not saved: {error}");
            return;
        }
        self.state.config_error = None;
        self.state.int_to_float = enabled;
        let _ = self.commands.send(PlayerCommand::SetIntToFloat(enabled));
        self.state.status = "integer→F32 compatibility applies on next configure".to_owned();
        self.rebuild_settings_rows();
    }

    fn toggle_shared_playback(&mut self) {
        let enabled = !self.state.shared_playback;
        // Save first: malformed/unreadable config must remain untouched, and
        // a failed save must not change the preference or issue a backend
        // switch command.
        if let Err(error) = (self.hooks.on_shared_playback_selected)(enabled) {
            self.state.status = format!("shared playback not saved: {error}");
            return;
        }
        self.state.config_error = None;
        self.state.shared_playback = enabled;
        let _ = self
            .commands
            .send(PlayerCommand::SetSharedPlayback(enabled));
        self.state.status = if enabled {
            "shared playback requested (OS mixer may process; not bit-perfect)".to_owned()
        } else {
            "direct/exclusive playback requested".to_owned()
        };
        self.rebuild_settings_rows();
    }

    fn set_timing(&mut self, period: Option<u32>, buffer: Option<u32>) {
        if period == Some(0) || buffer == Some(0) {
            self.state.status = "timing frames must be positive".to_owned();
            return;
        }
        if let Some(period) = period {
            let Some(minimum) = period.checked_mul(2) else {
                self.state.status = "period frames too large".to_owned();
                return;
            };
            if buffer.is_some_and(|buffer| buffer < minimum) {
                self.state.status =
                    "buffer frames must be at least twice the period frames".to_owned();
                return;
            }
        }
        if let Err(error) = (self.hooks.on_timing)(period, buffer) {
            self.state.status = format!("timing not saved: {error}");
            return;
        }
        self.state.config_error = None;
        self.state.timing = (period, buffer);
        let _ = self.commands.send(PlayerCommand::SetTiming {
            period_frames: period,
            buffer_frames: buffer,
        });
        self.state.status = if period.is_none() && buffer.is_none() {
            "DAC timing: Auto (applies on next configure)".to_owned()
        } else {
            "DAC timing: Custom (applies on next configure)".to_owned()
        };
        self.rebuild_settings_rows();
    }

    fn set_accent(&mut self, accent: Option<&str>) {
        if let Some(text) = accent
            && theme::parse_hex_color(text).is_none()
        {
            self.state.status = "accent must be exactly #RRGGBB".to_owned();
            return;
        }
        if let Err(error) = (self.hooks.on_accent)(accent) {
            self.state.status = format!("accent not saved: {error}");
            return;
        }
        self.state.config_error = None;
        self.state.accent = accent.map(str::to_owned);
        self.theme = Theme::detect(accent).0;
        self.state.status = match accent {
            Some(text) => format!("accent: {text}"),
            None => format!("accent reset to {}", theme::DEFAULT_ACCENT),
        };
        self.rebuild_settings_rows();
    }

    fn toggle_shuffle(&mut self) {
        let enabled = !self.state.shuffle;
        if self
            .commands
            .send(PlayerCommand::SetShuffle(enabled))
            .is_err()
        {
            self.state.status = "cannot change queue order: player disconnected".to_owned();
            return;
        }
        self.state.shuffle = enabled;
        self.state.status = if enabled {
            "random play ON"
        } else {
            "random play OFF"
        }
        .to_owned();
        self.rebuild_settings_rows();
    }

    fn toggle_pause(&mut self) {
        let command = if self.state.paused {
            PlayerCommand::Play
        } else {
            PlayerCommand::Pause
        };
        self.state.paused = !self.state.paused;
        let _ = self.commands.send(command);
    }

    fn seek_by(&self, seconds: i64) {
        let target = self.state.frame as i64 + seconds * i64::from(self.state.rate_hz);
        let _ = self
            .commands
            .send(PlayerCommand::SeekFrame(target.max(0) as u64));
    }

    fn catalog_writable(&mut self) -> bool {
        if let Some(error) = &self.state.catalog_error {
            self.state.status = format!("playlists read-only: {error}");
            return false;
        }
        true
    }

    /// Validate then mutate in-memory only. Compare old/new so no-op edits
    /// never mark dirty; the whole catalog is saved on Ctrl+S, not here.
    fn edit_catalog(
        &mut self,
        edit: impl FnOnce(&mut PlaylistCatalog) -> Result<(), String>,
    ) -> bool {
        if !self.catalog_writable() {
            return false;
        }
        let before = self.state.catalog.clone();
        let mut after = before.clone();
        if let Err(error) = edit(&mut after) {
            self.state.status = error;
            return false;
        }
        if after == before {
            return true;
        }
        if after.selected != before.selected || after.playlists.len() != before.playlists.len() {
            self.catalog_dirty = true;
        }
        for (index, (old, new)) in before
            .playlists
            .iter()
            .zip(after.playlists.iter())
            .enumerate()
        {
            if old.name != new.name {
                self.catalog_dirty = true;
                self.dirty_playlists.insert(index);
            }
            if old.entries != new.entries {
                self.dirty_playlists.insert(index);
            }
        }
        self.state.catalog = after;
        self.playlist_name_sel = self
            .playlist_name_sel
            .min(self.state.catalog.playlists.len() - 1);
        self.playlist_track_sel = self.playlist_track_sel.min(
            self.state.catalog.playlists[self.playlist_name_sel]
                .entries
                .len()
                .saturating_sub(1),
        );
        self.rebuild_playlist_rows();
        true
    }

    fn catalog_create(&mut self, name: String) -> bool {
        let name = name.trim().to_owned();
        let changed = self.edit_catalog(|catalog| {
            if name.is_empty() {
                return Err("playlist name must not be blank".to_owned());
            }
            if catalog.name_taken(&name) {
                return Err(format!("a playlist named \"{name}\" already exists"));
            }
            catalog.playlists.push(SavedPlaylist {
                name,
                entries: Vec::new(),
            });
            catalog.selected = catalog.playlists.len() - 1;
            Ok(())
        });
        if changed {
            self.dirty_playlists.insert(self.state.catalog.selected);
            self.playlist_name_sel = self.state.catalog.selected;
            self.playlist_track_sel = 0;
            self.playlist_selected.clear();
            self.rebuild_playlist_rows();
        }
        changed
    }

    fn catalog_rename(&mut self, name: String) {
        let name = name.trim().to_owned();
        let index = self.playlist_name_sel;
        self.edit_catalog(|catalog| {
            if name.is_empty() {
                return Err("playlist name must not be blank".to_owned());
            }
            if index >= catalog.playlists.len() {
                return Err("playlist selection is out of range".to_owned());
            }
            if catalog.playlists[index].name == name {
                return Ok(());
            }
            if catalog
                .playlists
                .iter()
                .enumerate()
                .any(|(other, playlist)| {
                    other != index && playlist.name.eq_ignore_ascii_case(&name)
                })
            {
                return Err(format!("a playlist named \"{name}\" already exists"));
            }
            catalog.playlists[index].name = name;
            Ok(())
        });
    }

    fn catalog_delete(&mut self) {
        let index = self.playlist_name_sel;
        let name = self
            .state
            .catalog
            .playlists
            .get(index)
            .map(|list| list.name.clone());
        let prior_dirty = self.dirty_playlists.clone();
        if self.edit_catalog(|catalog| {
            if catalog.playlists.len() <= 1 {
                return Err("cannot delete the final playlist".to_owned());
            }
            catalog.playlists.remove(index);
            catalog.selected = catalog.selected.min(catalog.playlists.len() - 1);
            Ok(())
        }) {
            self.deleted_name = name;
            self.dirty_playlists = prior_dirty
                .into_iter()
                .filter_map(|dirty| {
                    if dirty == index {
                        None
                    } else if dirty > index {
                        Some(dirty - 1)
                    } else {
                        Some(dirty)
                    }
                })
                .collect();
            self.playlist_name_sel = self
                .playlist_name_sel
                .min(self.state.catalog.playlists.len() - 1);
            self.playlist_track_sel = 0;
            self.playlist_selected.clear();
            self.rebuild_playlist_rows();
        }
    }

    fn catalog_select(&mut self, index: usize) {
        self.edit_catalog(|catalog| {
            if index >= catalog.playlists.len() {
                return Err("playlist selection is out of range".to_owned());
            }
            catalog.selected = index;
            Ok(())
        });
        self.playlist_name_sel = index.min(self.state.catalog.playlists.len() - 1);
        self.playlist_track_sel = 0;
        self.playlist_anchor = 0;
        self.playlist_selected.clear();
        self.rebuild_playlist_rows();
    }

    fn catalog_add_to(&mut self, index: usize, entries: Vec<QueueEntry>) {
        if entries.is_empty() {
            self.state.status = "no tracks to add".to_owned();
            return;
        }
        let count = entries.len();
        if self.edit_catalog(|catalog| {
            let Some(playlist) = catalog.playlists.get_mut(index) else {
                return Err("playlist no longer exists".to_owned());
            };
            playlist.entries.extend(entries);
            Ok(())
        }) {
            self.dirty_playlists.insert(index);
            let name = &self.state.catalog.playlists[index].name;
            self.state.status = format!("added {count} track(s) to {name}; Ctrl+S to save");
        }
    }

    fn catalog_import(&mut self, path: PathBuf, name: String) {
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.state.status = "playlist name must not be blank".to_owned();
            return;
        }
        if self.state.catalog.name_taken(&name) {
            self.state.status = format!("a playlist named \"{name}\" already exists");
            return;
        }
        let path = match std::path::absolute(&path) {
            Ok(path) => path,
            Err(error) => {
                self.state.status = format!("import path: {error}");
                return;
            }
        };
        let Some(entries) = self.import_entries(&path) else {
            return;
        };
        let count = entries.len();
        if self.edit_catalog(|catalog| {
            catalog.playlists.push(SavedPlaylist { name, entries });
            catalog.selected = catalog.playlists.len() - 1;
            Ok(())
        }) {
            self.playlist_name_sel = self.state.catalog.selected;
            self.dirty_playlists.insert(self.state.catalog.selected);
            self.rebuild_playlist_rows();
            self.state.status = format!("imported {count} track(s); Ctrl+S to save");
        }
    }

    fn catalog_remove_rows(&mut self) {
        let index = self.playlist_name_sel;
        let cursor = self.playlist_track_sel;
        let selected = self.playlist_selected.clone();
        let indices: HashSet<usize> = if selected.is_empty() {
            [cursor].into()
        } else {
            selected
        };
        if self.edit_catalog(|catalog| {
            let entries = &mut catalog.playlists[index].entries;
            *entries = entries
                .drain(..)
                .enumerate()
                .filter_map(|(row, entry)| (!indices.contains(&row)).then_some(entry))
                .collect();
            Ok(())
        }) {
            self.playlist_selected.clear();
            self.playlist_track_sel = self.playlist_track_sel.min(
                self.state.catalog.playlists[index]
                    .entries
                    .len()
                    .saturating_sub(1),
            );
        }
    }

    fn catalog_move_rows(&mut self, indices: Vec<usize>, gap: usize) {
        let index = self.playlist_name_sel;
        let first = gap;
        if self.edit_catalog(|catalog| {
            let entries = &mut catalog.playlists[index].entries;
            if indices.iter().any(|&row| row >= entries.len()) {
                return Err("playlist changed during move".to_owned());
            }
            let picked: HashSet<usize> = indices.iter().copied().collect();
            let mut lifted = Vec::with_capacity(indices.len());
            let mut remaining = Vec::with_capacity(entries.len() - indices.len());
            for (row, entry) in entries.drain(..).enumerate() {
                if picked.contains(&row) {
                    lifted.push(entry);
                } else {
                    remaining.push(entry);
                }
            }
            let gap = gap.min(remaining.len());
            remaining.splice(gap..gap, lifted);
            *entries = remaining;
            Ok(())
        }) {
            self.playlist_selected.clear();
            self.playlist_track_sel = first.min(
                self.state.catalog.playlists[index]
                    .entries
                    .len()
                    .saturating_sub(1),
            );
        }
    }

    fn save_catalog(&mut self) -> bool {
        if !self.has_unsaved_catalog() {
            self.state.status = "no playlist changes to save".to_owned();
            return true;
        }
        if !self.catalog_writable() {
            return false;
        }
        match (self.hooks.on_catalog)(&self.state.catalog) {
            Ok(()) => {
                self.dirty_playlists.clear();
                self.catalog_dirty = false;
                self.deleted_name = None;
                self.rebuild_playlist_rows();
                self.state.status = "playlists saved".to_owned();
                true
            }
            Err(error) => {
                self.state.status = format!("playlist not saved: {error}");
                false
            }
        }
    }

    fn has_unsaved_catalog(&self) -> bool {
        self.catalog_dirty || !self.dirty_playlists.is_empty() || self.deleted_name.is_some()
    }

    fn request_quit(&mut self) -> bool {
        if !self.has_unsaved_catalog() {
            return true;
        }
        if matches!(
            self.mode,
            Mode::Prompt(Prompt {
                kind: PromptKind::QuitConfirm,
                ..
            })
        ) {
            return false;
        }
        let first_dirty = self.dirty_playlists.iter().copied().min();
        let name = first_dirty
            .and_then(|index| {
                self.state
                    .catalog
                    .playlists
                    .get(index)
                    .map(|list| list.name.clone())
            })
            .or_else(|| self.deleted_name.clone())
            .unwrap_or_else(|| self.state.catalog.selected_playlist().name.clone());
        let count = self.dirty_playlists.len()
            + usize::from(self.deleted_name.is_some())
            + usize::from(
                self.catalog_dirty
                    && self.dirty_playlists.is_empty()
                    && self.deleted_name.is_none(),
            );
        let suffix = if count > 1 {
            format!(" ({count} changes)")
        } else {
            String::new()
        };
        let label = format!("Save changes to \"{name}\"{suffix}? (y)es / (n)o / (esc) cancel");
        let origin = std::mem::replace(&mut self.mode, Mode::Normal);
        self.mode = Mode::Prompt(Prompt {
            label,
            buffer: String::new(),
            kind: PromptKind::QuitConfirm,
            origin: Box::new(origin),
        });
        false
    }

    fn rebuild_queue_rows(&mut self) {
        let past: Vec<QueueItem> = self.past.iter().cloned().collect();
        self.queue_rows = screens::queue::build_rows(
            self.state.audible.as_ref(),
            &self.state.pending,
            &past,
            self,
            self.queue_selected,
        );
        let mut pending_total = Duration::ZERO;
        let mut known = true;
        for item in &self.state.pending {
            if let Some(duration) = screens::queue::entry_duration(item, self) {
                pending_total += duration;
            } else {
                known = false;
                break;
            }
        }
        self.pending_duration_total = known.then_some(pending_total);
        if self
            .queue_selected
            .is_some_and(|id| screens::queue::row_of(&self.queue_rows, id).is_none())
        {
            self.queue_selected = self.state.pending.first().map(|item| item.id);
        }
        if self.queue_selected.is_none() {
            self.queue_selected = self
                .state
                .pending
                .first()
                .map(|item| item.id)
                .or_else(|| self.state.audible.as_ref().map(|item| item.id));
        }
    }

    fn queue_selected_pending(&self) -> Option<TrackId> {
        let id = self.queue_selected?;
        self.state
            .pending
            .iter()
            .any(|item| item.id == id)
            .then_some(id)
    }

    fn apply_event(&mut self, event: PlayerEvent) -> bool {
        match event {
            PlayerEvent::QueueChanged { audible, pending } => {
                // History advances only when a different track becomes
                // audible, never on Pause/Stop/Error (those use separate
                // events and may clear the queue snapshot).
                if let (Some(previous), Some(next)) = (&self.state.audible, &audible)
                    && previous.id != next.id
                {
                    self.past.push_back(previous.clone());
                    while self.past.len() > screens::queue::PAST_LIMIT {
                        self.past.pop_front();
                    }
                }
                self.state.audible = audible;
                self.state.pending = pending;
                self.rebuild_queue_rows();
                true
            }
            PlayerEvent::QueueEditRejected { id, reason } => {
                self.state.status = format!("queue edit rejected for #{id}: {reason}");
                true
            }
            PlayerEvent::Playing {
                track,
                output,
                source,
                buffers,
                converted,
                shared_playback,
            } => {
                let same_track = track == self.state.current_track;
                let direction = if output.format == DeviceFormat::F32Le {
                    "integer→F32"
                } else {
                    "F32→integer"
                };
                self.state.status = match (shared_playback, converted) {
                    (true, true) => {
                        format!("playing (shared; {direction} converted; not bit-perfect)")
                    }
                    (true, false) => "playing (shared / OS mixer; not bit-perfect)".to_owned(),
                    (false, true) => {
                        format!("playing ({direction} converted; not bit-perfect)")
                    }
                    (false, false) => "playing (bit-perfect)".to_owned(),
                };
                self.state.paused = false;
                if !same_track {
                    self.state.tags = TrackTags::default();
                    self.state.frame = 0;
                }
                self.state.current_track = track;
                self.state.total_frames = None;
                self.state.rate_hz =
                    output.rate_hz * output.format.source_frames_per_wire_frame() as u32;
                self.state.converted = converted;
                self.state.shared_active = shared_playback;
                // `OutputSpec.device` names the active route (Linux shared
                // uses `default`); it must not overwrite the configured
                // Direct device picker selection retained in `state.device`.
                self.state.output_spec = Some(output);
                self.state.source_spec = Some(source);
                self.state.buffers = Some(buffers);
                self.rebuild_settings_rows();
                true
            }
            PlayerEvent::Tags { track, tags } => {
                if track == self.state.current_track {
                    self.state.tags = tags;
                }
                true
            }
            PlayerEvent::Duration {
                track,
                total_frames,
            } => {
                if track == self.state.current_track {
                    self.state.total_frames = total_frames;
                }
                true
            }
            PlayerEvent::Position { track, frame } => {
                if track == self.state.current_track {
                    self.state.frame = frame;
                }
                false // coalesce progress redraw to <=4 Hz
            }
            PlayerEvent::Paused => {
                self.state.status = "paused".to_owned();
                self.state.paused = true;
                self.state.shared_active = false;
                true
            }
            PlayerEvent::Reconfiguring => {
                self.state.status = "reconfiguring device".to_owned();
                self.state.output_spec = None;
                self.state.source_spec = None;
                self.state.buffers = None;
                self.state.converted = false;
                self.state.shared_active = false;
                self.state.total_frames = None;
                true
            }
            PlayerEvent::Underrun { .. } => {
                self.state.status = "underrun".to_owned();
                true
            }
            PlayerEvent::Stalled { .. } => {
                self.state.status = "stalled".to_owned();
                true
            }
            PlayerEvent::Error { kind, .. } => {
                self.state.status = format!("error: {kind}");
                self.state.paused = true;
                self.state.total_frames = None;
                self.state.output_spec = None;
                self.state.source_spec = None;
                self.state.buffers = None;
                self.state.shared_active = false;
                true
            }
            PlayerEvent::EndOfQueue => {
                if let Some(previous) = self.state.audible.take() {
                    self.past.push_back(previous);
                    while self.past.len() > screens::queue::PAST_LIMIT {
                        self.past.pop_front();
                    }
                }
                self.state.status = "end of queue".to_owned();
                self.state.paused = false;
                self.state.total_frames = None;
                self.state.frame = 0;
                self.state.output_spec = None;
                self.state.source_spec = None;
                self.state.buffers = None;
                self.state.shared_active = false;
                self.rebuild_queue_rows();
                true
            }
        }
    }

    fn open_search(&mut self) {
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        let mut roots = self.state.search_roots.clone();
        if let Some(browser) = &self.browser {
            roots.push(browser.current_dir.clone());
        }
        self.worker.send(WorkerMsg::SetRoots(roots));
        self.mode = Mode::Search(SearchOverlay {
            query: String::new(),
            scope: SearchScope::All,
            generation,
            results: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            status: SearchStatus::Indexing,
        });
    }

    fn update_search_query(&mut self, search: &mut SearchOverlay) {
        self.search_generation = self.search_generation.wrapping_add(1);
        search.generation = self.search_generation;
        search.results.clear();
        search.visible.clear();
        search.selected = 0;
        search.status = SearchStatus::Indexing;
        if !search.query.trim().is_empty() {
            for (index, playlist) in self.state.catalog.playlists.iter().enumerate() {
                if let Some((score, spans)) = search::match_spans(&search.query, &playlist.name) {
                    search.results.push(SearchItem::Catalog {
                        index,
                        rank: (
                            -score,
                            playlist.name.to_lowercase(),
                            format!("catalog:{index}"),
                        ),
                        spans,
                    });
                }
            }
            search.refresh_visible();
            self.worker.send(WorkerMsg::Query {
                generation: search.generation,
                text: search.query.clone(),
            });
        } else {
            self.worker.send(WorkerMsg::Cancel);
        }
    }

    fn search_selected_action(&mut self, item: SearchItem, action: Action) {
        match item {
            SearchItem::Catalog { index, .. } => {
                let entries = self
                    .state
                    .catalog
                    .playlists
                    .get(index)
                    .map(|list| list.entries.clone())
                    .unwrap_or_default();
                match action {
                    Action::Activate => self.play_entries(entries),
                    Action::EnqueueSelected => self.enqueue_entries(entries),
                    Action::AddToPlaylist => {
                        self.mode = Mode::PlaylistPicker(PlaylistPicker {
                            entries,
                            selected: 0,
                        })
                    }
                    Action::OpenFolder => {
                        self.screen = Screen::Playlists;
                        self.focus = Focus::PlaylistTracks;
                        self.playlist_name_sel = index;
                        self.mode = Mode::Normal;
                    }
                    _ => {}
                }
            }
            SearchItem::Worker(hit) => {
                let path = hit.path;
                match action {
                    Action::OpenFolder => {
                        let folder = if matches!(
                            hit.kind,
                            HitKind::Track | HitKind::Album | HitKind::PlaylistFile
                        ) {
                            path.parent().map(Path::to_path_buf).unwrap_or(path)
                        } else {
                            path
                        };
                        self.open_browser(folder);
                    }
                    Action::Activate | Action::EnqueueSelected | Action::AddToPlaylist => {
                        let target = match action {
                            Action::Activate => CollectTarget::Enqueue { play_first: true },
                            Action::EnqueueSelected => CollectTarget::Enqueue { play_first: false },
                            _ => CollectTarget::OpenPicker,
                        };
                        match hit.kind {
                            HitKind::Track => {
                                let entries = vec![QueueEntry::from(path)];
                                match target {
                                    CollectTarget::Enqueue { play_first: true } => {
                                        self.play_entries(entries)
                                    }
                                    CollectTarget::Enqueue { .. } => self.enqueue_entries(entries),
                                    CollectTarget::OpenPicker => {
                                        self.mode = Mode::PlaylistPicker(PlaylistPicker {
                                            entries,
                                            selected: 0,
                                        })
                                    }
                                }
                            }
                            HitKind::PlaylistFile => {
                                if let Some(entries) = self.import_entries(&path) {
                                    match target {
                                        CollectTarget::Enqueue { play_first: true } => {
                                            self.play_entries(entries)
                                        }
                                        CollectTarget::Enqueue { .. } => {
                                            self.enqueue_entries(entries)
                                        }
                                        CollectTarget::OpenPicker => {
                                            self.mode = Mode::PlaylistPicker(PlaylistPicker {
                                                entries,
                                                selected: 0,
                                            })
                                        }
                                    }
                                }
                            }
                            HitKind::Album => self.start_album_collect(path, target),
                            HitKind::Folder => self.start_collect(path, target),
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn start_picker_for_browser(&mut self) {
        let Some(item) = self.selected_browser_item() else {
            return;
        };
        match item.kind {
            BrowserItemKind::Audio => {
                self.mode = Mode::PlaylistPicker(PlaylistPicker {
                    entries: vec![QueueEntry::from(item.path)],
                    selected: 0,
                })
            }
            BrowserItemKind::Playlist => {
                if let Some(entries) = self.import_entries(&item.path) {
                    self.mode = Mode::PlaylistPicker(PlaylistPicker {
                        entries,
                        selected: 0,
                    });
                }
            }
            BrowserItemKind::Dir => self.start_collect(item.path, CollectTarget::OpenPicker),
        }
    }

    fn start_prompt(&mut self, label: &str, kind: InputAction) {
        let origin = std::mem::replace(&mut self.mode, Mode::Normal);
        self.mode = Mode::Prompt(Prompt {
            label: label.to_owned(),
            buffer: String::new(),
            kind: PromptKind::Text(kind),
            origin: Box::new(origin),
        });
    }

    fn submit_input(&mut self, action: InputAction, text: String) {
        let text = text.trim().to_owned();
        match action {
            InputAction::LocationPath => {
                if !text.is_empty() {
                    self.open_browser(PathBuf::from(text));
                }
            }
            InputAction::MapDriveLetter => {
                let Some(letter) = text.chars().next().filter(|_| text.len() == 1) else {
                    self.state.status = "enter a single drive letter".to_owned();
                    return;
                };
                self.start_prompt(
                    "Share (\\\\server\\share):",
                    InputAction::MapDriveShare(letter),
                );
            }
            InputAction::MapDriveShare(letter) => {
                if !text.is_empty() {
                    self.pending_os = Some(OsAction::MapDrive(letter, text));
                }
            }
            InputAction::MountShare => {
                if !text.is_empty() {
                    self.pending_os = Some(OsAction::MountShare(text));
                }
            }
            InputAction::PlaylistCreate => {
                let pending = match &self.mode {
                    Mode::PlaylistPicker(picker) => Some(picker.entries.clone()),
                    _ => None,
                };
                if self.catalog_create(text)
                    && let Some(entries) = pending
                {
                    self.catalog_add_to(self.state.catalog.selected, entries);
                    self.mode = Mode::Normal;
                }
            }
            InputAction::PlaylistRename => self.catalog_rename(text),
            InputAction::PlaylistImportPath => {
                if !text.is_empty() {
                    self.start_prompt(
                        "Import as (playlist name):",
                        InputAction::PlaylistImportName(PathBuf::from(text)),
                    );
                }
            }
            InputAction::PlaylistImportName(path) => self.catalog_import(path, text),
            InputAction::TimingPeriod => {
                let Ok(period) = text.parse::<u32>() else {
                    self.state.status = "period must be a positive decimal".to_owned();
                    return;
                };
                if period == 0 {
                    self.state.status = "period must be positive".to_owned();
                    return;
                }
                self.start_prompt("Buffer frames:", InputAction::TimingBuffer(period));
            }
            InputAction::TimingBuffer(period) => {
                let Ok(buffer) = text.parse::<u32>() else {
                    self.state.status = "buffer must be a positive decimal".to_owned();
                    return;
                };
                self.set_timing(Some(period), Some(buffer));
            }
            InputAction::Accent => self.set_accent(Some(&text)),
        }
    }

    fn open_actions(&mut self) {
        let mut items = Vec::new();
        if matches!(self.mode, Mode::Locations(_)) {
            items.push(("Enter path…".to_owned(), ActionItem::EnterPath));
            items.push(("Refresh".to_owned(), ActionItem::RefreshLocations));
            if self.hooks.map_drive.is_some() {
                items.push(("Map drive…".to_owned(), ActionItem::MapDrive));
                if let Mode::Locations(locations) = &self.mode {
                    for location in &locations.locations {
                        if location.mapped_network
                            && let Some(letter) = location.path.to_string_lossy().chars().next()
                        {
                            items.push((
                                format!("Disconnect {letter}:"),
                                ActionItem::Disconnect(letter),
                            ));
                        }
                    }
                }
            }
            if self.hooks.mount_share.is_some() {
                items.push(("Mount share…".to_owned(), ActionItem::MountShare));
            }
        } else if self.screen == Screen::Playlists {
            items.push(("Create playlist…".to_owned(), ActionItem::PlaylistCreate));
            items.push(("Rename playlist…".to_owned(), ActionItem::PlaylistRename));
            items.push(("Delete playlist".to_owned(), ActionItem::PlaylistDelete));
            items.push(("Import file…".to_owned(), ActionItem::PlaylistImport));
        } else {
            items.push((
                "Queue current folder".to_owned(),
                ActionItem::QueueCurrentFolder,
            ));
        }
        items.push(("Quit".to_owned(), ActionItem::Quit));
        let origin = std::mem::replace(&mut self.mode, Mode::Normal);
        self.mode = Mode::Actions(ActionsOverlay {
            items,
            selected: 0,
            origin: Box::new(origin),
        });
    }

    fn run_action(&mut self, action: ActionItem) -> bool {
        match action {
            ActionItem::EnterPath => {
                self.start_prompt("Path or \\\\server\\share:", InputAction::LocationPath)
            }
            ActionItem::RefreshLocations => self.open_locations(),
            ActionItem::MapDrive => {
                self.start_prompt("Drive letter (free, e.g. Z):", InputAction::MapDriveLetter)
            }
            ActionItem::MountShare => {
                self.start_prompt("SMB share (server/share):", InputAction::MountShare)
            }
            ActionItem::Disconnect(letter) => {
                if let Some(disconnect) = &mut self.hooks.disconnect_drive {
                    match disconnect(letter) {
                        Ok(()) => {
                            self.state.status = format!("disconnected {letter}:");
                            let on_drive = self.browser.as_ref().is_some_and(|browser| {
                                browser
                                    .current_dir
                                    .to_string_lossy()
                                    .to_uppercase()
                                    .starts_with(&format!("{letter}:"))
                            });
                            if on_drive {
                                self.state.last_browser_dir = self.state.home_dir.clone();
                                self.open_browser_startup();
                            }
                            self.refresh_search_roots();
                            self.open_locations();
                        }
                        Err(error) => self.state.status = error,
                    }
                }
            }
            ActionItem::PlaylistCreate => {
                self.start_prompt("New playlist name:", InputAction::PlaylistCreate)
            }
            ActionItem::PlaylistRename => {
                self.start_prompt("Rename playlist to:", InputAction::PlaylistRename)
            }
            ActionItem::PlaylistDelete => self.catalog_delete(),
            ActionItem::PlaylistImport => self.start_prompt(
                "Playlist file (.m3u/.m3u8/.pls/.xspf/.cue):",
                InputAction::PlaylistImportPath,
            ),
            ActionItem::QueueCurrentFolder => {
                if let Some(browser) = &self.browser {
                    self.start_collect(
                        browser.current_dir.clone(),
                        CollectTarget::Enqueue { play_first: false },
                    );
                }
            }
            ActionItem::Quit => return self.request_quit(),
        }
        false
    }

    fn run_os_action(&mut self, action: OsAction) {
        match action {
            OsAction::MapDrive(letter, share) => {
                if let Some(map_drive) = &mut self.hooks.map_drive {
                    match map_drive(letter, &share) {
                        Ok(root) => {
                            self.state.status = format!("mapped {letter}: to {share}");
                            self.open_browser(root);
                            self.refresh_search_roots();
                        }
                        Err(error) => self.state.status = error,
                    }
                } else {
                    self.state.status = "drive mapping unavailable on this OS".to_owned();
                }
            }
            OsAction::MountShare(share) => {
                if let Some(mount_share) = &mut self.hooks.mount_share {
                    match mount_share(&share) {
                        Ok(()) => {
                            self.state.status =
                                "mount requested; refresh Locations to browse it".to_owned();
                            self.refresh_search_roots();
                            self.open_locations();
                        }
                        Err(error) => self.state.status = error,
                    }
                } else {
                    self.state.status = "share mounting unavailable on this OS".to_owned();
                }
            }
        }
    }

    fn refresh_search_roots(&mut self) {
        if let Ok(locations) = (self.hooks.locations)() {
            self.state.search_roots = locations
                .into_iter()
                .map(|location| location.path)
                .collect();
        }
        let mut roots = self.state.search_roots.clone();
        if let Some(browser) = &self.browser {
            roots.push(browser.current_dir.clone());
        }
        self.worker.send(WorkerMsg::SetRoots(roots));
    }

    fn settings_row_count(&self) -> usize {
        match self.settings_section {
            0 => 7, // Output
            1 => 4, // Playback
            2 => 2, // Library & locations
            3 => 1, // Appearance
            4 => keymap::help_groups()
                .iter()
                .map(|(_, rows)| rows.len())
                .sum(),
            _ => 0,
        }
    }

    fn settings_rows(&self) -> Vec<screens::settings::SettingsRowView> {
        use screens::settings::SettingsRowView as Row;
        match self.settings_section {
            0 => {
                let device_name = self
                    .device_candidates
                    .get(self.device_candidate_sel)
                    .map(|(_, name)| name.as_str())
                    .unwrap_or(self.state.device_display());
                let (period, buffer) = self.state.timing;
                let shared = self.state.shared_playback;
                let device_note = if cfg!(target_os = "linux") {
                    "←→ preview · Enter apply · r default; hw: for Direct, shared uses ALSA default"
                } else {
                    "←→ preview · Enter apply · r platform default"
                };
                let access_note = if cfg!(target_os = "linux") {
                    "Enter/space toggle · r direct; sharing depends on the system-configured default mixer"
                } else {
                    "Enter/space toggle · r direct; the OS mixer may process audio"
                };
                vec![
                    Row { label: "Output device".to_owned(), value: device_name.to_owned(), note: Some(device_note.to_owned()), warning: None },
                    Row { label: "Access".to_owned(), value: if shared { "shared / OS mixer" } else { "direct / exclusive" }.to_owned(), note: Some(access_note.to_owned()), warning: shared.then(|| "shared playback may resample, remix or process audio; not bit-perfect".to_owned()) },
                    Row { label: "Fidelity".to_owned(), value: if shared { "OS-managed" } else { "strict first" }.to_owned(), note: Some(if shared { "client PCM to OS mixer; app/OS conversion possible" } else { "exact native format; conversion only with explicit compatibility opt-in" }.to_owned()), warning: None },
                    Row { label: "Period".to_owned(), value: period.map(|n| format!("{n} frames")).unwrap_or_else(|| "Auto".to_owned()), note: Some("← Auto · → custom period then buffer".to_owned()), warning: None },
                    Row { label: "Buffer".to_owned(), value: buffer.map(|n| format!("{n} frames")).unwrap_or_else(|| "Auto".to_owned()), note: Some("applies on next configure (next track or seek)".to_owned()), warning: None },
                    Row { label: "F32→integer".to_owned(), value: if self.state.float_to_int { "ON" } else { "OFF" }.to_owned(), note: Some("OFF keeps strict bit-perfect; ON permits non-bit-perfect conversion".to_owned()), warning: self.state.float_to_int.then(|| "converted output is not bit-perfect".to_owned()) },
                    Row { label: "integer→F32".to_owned(), value: if self.state.int_to_float { "ON" } else { "OFF" }.to_owned(), note: Some("S16/S24 exact in F32; S32 loses low bits".to_owned()), warning: self.state.int_to_float.then(|| "converted output is not bit-perfect".to_owned()) },
                ]
            }
            1 => vec![
                Row {
                    label: "Shuffle".to_owned(),
                    value: if self.state.shuffle { "ON" } else { "OFF" }.to_owned(),
                    note: Some("space / Enter toggles future queue order".to_owned()),
                    warning: None,
                },
                Row {
                    label: "Play/pause".to_owned(),
                    value: "space (Ctrl+Space in editable lists)".to_owned(),
                    note: None,
                    warning: None,
                },
                Row {
                    label: "Stop / next".to_owned(),
                    value: "s / n".to_owned(),
                    note: None,
                    warning: None,
                },
                Row {
                    label: "Seek".to_owned(),
                    value: "← / → 10 seconds".to_owned(),
                    note: None,
                    warning: None,
                },
            ],
            2 => vec![
                Row {
                    label: "Locations".to_owned(),
                    value: "Enter to open".to_owned(),
                    note: Some("browse, type path, refresh, map/mount/disconnect".to_owned()),
                    warning: None,
                },
                Row {
                    label: "Current folder".to_owned(),
                    value: self
                        .browser
                        .as_ref()
                        .map(|b| b.current_dir.display().to_string())
                        .unwrap_or_else(|| "none".to_owned()),
                    note: None,
                    warning: None,
                },
            ],
            3 => vec![Row {
                label: "Accent".to_owned(),
                value: self
                    .state
                    .accent
                    .clone()
                    .unwrap_or_else(|| theme::DEFAULT_ACCENT.to_owned()),
                note: Some("Enter: #RRGGBB · r: reset".to_owned()),
                warning: None,
            }],
            4 => keymap::help_groups()
                .into_iter()
                .flat_map(|(group, rows)| {
                    rows.into_iter().map(move |(key, label)| Row {
                        label: key.to_owned(),
                        value: label.to_owned(),
                        note: Some(group.to_owned()),
                        warning: None,
                    })
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn rebuild_settings_rows(&mut self) {
        self.settings_rows_cache = self.settings_rows();
    }

    fn settings_activate(&mut self, key: KeyCode) {
        match (self.settings_section, self.settings_row) {
            (0, 0) if key == KeyCode::Enter => self.apply_device_candidate(),
            (0, 0) if key == KeyCode::Char('r') => self.reset_device(),
            (0, 0) if matches!(key, KeyCode::Left | KeyCode::Right) => {
                if self.device_candidates.is_empty() {
                    self.refresh_devices();
                }
                let len = self.device_candidates.len();
                if len > 0 {
                    self.device_candidate_sel = if key == KeyCode::Left {
                        (self.device_candidate_sel + len - 1) % len
                    } else {
                        (self.device_candidate_sel + 1) % len
                    };
                }
                self.rebuild_settings_rows();
            }
            (0, 1)
                if matches!(
                    key,
                    KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('r')
                ) =>
            {
                if key == KeyCode::Char('r') && !self.state.shared_playback {
                    return;
                }
                self.toggle_shared_playback();
            }
            (0, 3 | 4) if key == KeyCode::Left || key == KeyCode::Char('r') => {
                self.set_timing(None, None)
            }
            (0, 3 | 4) if key == KeyCode::Right || key == KeyCode::Enter => {
                self.start_prompt("Period frames:", InputAction::TimingPeriod)
            }
            (0, 5)
                if matches!(
                    key,
                    KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('r')
                ) =>
            {
                if key == KeyCode::Char('r') && !self.state.float_to_int {
                    return;
                }
                self.toggle_float_to_int();
            }
            (0, 6)
                if matches!(
                    key,
                    KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('r')
                ) =>
            {
                if key == KeyCode::Char('r') && !self.state.int_to_float {
                    return;
                }
                self.toggle_int_to_float();
            }
            (1, 0)
                if matches!(
                    key,
                    KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('r')
                ) =>
            {
                if key == KeyCode::Char('r') && !self.state.shuffle {
                    return;
                }
                self.toggle_shuffle();
            }
            (2, 0) if key == KeyCode::Enter => self.open_locations(),
            (3, 0) if key == KeyCode::Enter => {
                self.start_prompt("Accent (#RRGGBB):", InputAction::Accent)
            }
            (3, 0) if key == KeyCode::Char('r') => self.set_accent(None),
            _ => {}
        }
    }

    fn playlist_entries_for_cursor(&self) -> Vec<QueueEntry> {
        let Some(list) = self.state.catalog.playlists.get(self.playlist_name_sel) else {
            return Vec::new();
        };
        let mut indices: Vec<usize> = if self.playlist_selected.is_empty() {
            vec![self.playlist_track_sel]
        } else {
            self.playlist_selected.iter().copied().collect()
        };
        indices.sort_unstable();
        indices
            .into_iter()
            .filter_map(|index| list.entries.get(index).cloned())
            .collect()
    }

    fn playlist_move_start(&mut self) {
        if !self.catalog_writable() {
            return;
        }
        let count = self.state.catalog.playlists[self.playlist_name_sel]
            .entries
            .len();
        if count == 0 {
            self.state.status = "playlist is empty".to_owned();
            return;
        }
        let mut indices: Vec<usize> = if self.playlist_selected.is_empty() {
            vec![self.playlist_track_sel.min(count - 1)]
        } else {
            self.playlist_selected
                .iter()
                .copied()
                .filter(|&index| index < count)
                .collect()
        };
        indices.sort_unstable();
        let remaining = count - indices.len();
        let gap = indices[0].min(remaining);
        self.mode = Mode::Move(MoveOverlay::Playlist { indices, gap });
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('q' | 'Q'))
        {
            return self.request_quit();
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Search(search) => self.handle_search_key(search, key),
            Mode::Move(moving) => self.handle_move_key(moving, key),
            Mode::Prompt(prompt) => self.handle_prompt_key(prompt, key),
            Mode::Help => match keymap::lookup_mode(ModeKind::Help, key) {
                Some(Action::CloseOverlay) => false,
                Some(Action::Quit) => self.request_quit(),
                Some(Action::CursorUp) => {
                    let step = if key.code == KeyCode::PageUp { 10 } else { 1 };
                    self.help_scroll = self.help_scroll.saturating_sub(step);
                    self.mode = Mode::Help;
                    false
                }
                Some(Action::CursorDown) => {
                    let step = if key.code == KeyCode::PageDown { 10 } else { 1 };
                    self.help_scroll = self.help_scroll.saturating_add(step);
                    self.mode = Mode::Help;
                    false
                }
                _ => {
                    self.mode = Mode::Help;
                    false
                }
            },
            Mode::Locations(locations) => self.handle_locations_key(locations, key),
            Mode::PlaylistPicker(picker) => self.handle_picker_key(picker, key),
            Mode::Actions(actions) => self.handle_actions_key(actions, key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> bool {
        keymap::lookup(self.focus, key).is_some_and(|action| self.dispatch(action))
    }

    fn dispatch(&mut self, action: Action) -> bool {
        match action {
            Action::CursorUp => self.move_cursor(-1),
            Action::CursorDown => self.move_cursor(1),
            Action::Activate => self.activate(),
            Action::BrowserUp => {
                if self.screen == Screen::Library && self.focus == Focus::Library {
                    self.browser_up();
                }
            }
            Action::FocusNext => self.cycle_focus(),
            Action::ShowLibrary => {
                self.screen = Screen::Library;
                self.focus = Focus::Library;
                if self.browser.is_none() {
                    self.open_browser_startup();
                }
            }
            Action::ShowPlaylists => {
                self.screen = Screen::Playlists;
                self.focus = Focus::PlaylistNames;
                self.playlist_anchor = self.playlist_track_sel;
            }
            Action::ShowSettings => {
                self.screen = Screen::Settings;
                self.focus = Focus::SettingsSections;
            }
            Action::OpenSearch => self.open_search(),
            Action::TogglePause => self.toggle_pause(),
            Action::Stop => {
                let _ = self.commands.send(PlayerCommand::Stop);
                self.state.paused = true;
            }
            Action::NextTrack => {
                let _ = self.commands.send(PlayerCommand::Next);
            }
            Action::SeekBack => {
                if self.screen == Screen::Settings && self.focus == Focus::SettingsRows {
                    self.settings_activate(KeyCode::Left);
                } else {
                    self.seek_by(-(SEEK_SECONDS as i64));
                }
            }
            Action::SeekForward => {
                if self.screen == Screen::Settings && self.focus == Focus::SettingsRows {
                    self.settings_activate(KeyCode::Right);
                } else {
                    self.seek_by(SEEK_SECONDS as i64);
                }
            }
            Action::ToggleShuffle => self.toggle_shuffle(),
            Action::Quit => return self.request_quit(),
            Action::BackOrQuit => {
                if self.screen == Screen::Library {
                    return self.request_quit();
                }
                self.screen = Screen::Library;
                self.focus = Focus::Library;
            }
            Action::ToggleHelp => {
                self.help_scroll = 0;
                self.mode = Mode::Help;
            }
            Action::OpenLocations => self.open_locations(),
            Action::OpenActions => self.open_actions(),
            Action::OpenOutputSettings => self.open_output_settings(),
            Action::FocusPlaylist => {
                self.screen = Screen::Playlists;
                self.focus = Focus::PlaylistTracks;
                self.playlist_anchor = self.playlist_track_sel;
            }
            Action::EnqueueSelected => match self.focus {
                Focus::Library => self.browser_enqueue(false),
                Focus::PlaylistNames => {
                    if let Some(list) = self.state.catalog.playlists.get(self.playlist_name_sel) {
                        self.enqueue_entries(list.entries.clone());
                    }
                }
                Focus::PlaylistTracks => self.enqueue_entries(self.playlist_entries_for_cursor()),
                _ => {}
            },
            Action::EnqueueAlbum => match self.focus {
                Focus::Library => self.browser_enqueue(true),
                Focus::PlaylistNames | Focus::PlaylistTracks => {
                    if let Some(list) = self.state.catalog.playlists.get(self.playlist_name_sel) {
                        self.enqueue_entries(list.entries.clone());
                    }
                }
                _ => {}
            },
            Action::AddToPlaylist => {
                if self.screen == Screen::Library && self.focus == Focus::Library {
                    self.start_picker_for_browser();
                } else {
                    self.screen = Screen::Playlists;
                    self.focus = Focus::PlaylistNames;
                }
            }
            Action::NewPlaylist => {
                self.start_prompt("New playlist name:", InputAction::PlaylistCreate)
            }
            Action::RenamePlaylist => {
                self.start_prompt("Rename playlist to:", InputAction::PlaylistRename)
            }
            Action::ToggleSelect => {
                let cursor = self.playlist_track_sel;
                if !self.playlist_selected.insert(cursor) {
                    self.playlist_selected.remove(&cursor);
                }
            }
            Action::RangeSelect => {
                let lo = self.playlist_anchor.min(self.playlist_track_sel);
                let hi = self.playlist_anchor.max(self.playlist_track_sel);
                self.playlist_selected = (lo..=hi).collect();
            }
            Action::RemoveRows => self.catalog_remove_rows(),
            Action::EnqueueSelection => self.enqueue_entries(self.playlist_entries_for_cursor()),
            Action::MoveStart => match self.focus {
                Focus::PlaylistTracks => self.playlist_move_start(),
                Focus::Queue => {
                    if let Some(id) = self.queue_selected_pending() {
                        self.mode = Mode::Move(MoveOverlay::Queue {
                            id,
                            target: Some(id),
                        });
                    } else {
                        self.state.status = "only upcoming queue rows can move".to_owned();
                    }
                }
                _ => {}
            },
            Action::ResetRow => self.settings_activate(KeyCode::Char('r')),
            Action::ToggleSetting => self.settings_activate(KeyCode::Char(' ')),
            Action::QueueRemove => {
                if let Some(id) = self.queue_selected_pending() {
                    let _ = self.commands.send(PlayerCommand::RemoveQueueEntry(id));
                } else {
                    self.state.status = "audible and past rows are read-only".to_owned();
                }
            }
            Action::QueueClearHistory => {
                self.past.clear();
                self.rebuild_queue_rows();
            }
            Action::SaveCatalog => {
                self.save_catalog();
            }
            Action::ToggleFloatToInt => self.toggle_float_to_int(),
            Action::ToggleIntToFloat => self.toggle_int_to_float(),
            Action::ToggleSharedPlayback => self.toggle_shared_playback(),
            Action::CloseOverlay
            | Action::DeleteChar
            | Action::OpenFolder
            | Action::ConfirmYes
            | Action::ConfirmNo => {
                // Modal-only actions are consumed by the overlay handlers.
            }
        }
        false
    }

    fn move_cursor(&mut self, delta: isize) {
        match self.focus {
            Focus::Library => {
                if let Some(browser) = &mut self.browser {
                    browser.move_selection(delta);
                }
            }
            Focus::Queue => {
                let selected_row = self
                    .queue_selected
                    .and_then(|id| screens::queue::row_of(&self.queue_rows, id))
                    .unwrap_or(0);
                let mut next = selected_row.saturating_add_signed(delta);
                loop {
                    if next >= self.queue_rows.len() {
                        break;
                    }
                    if let screens::queue::QueueRow::Track { id, .. } = self.queue_rows[next] {
                        self.queue_selected = Some(id);
                        break;
                    }
                    let candidate = next.saturating_add_signed(delta);
                    if candidate == next || candidate >= self.queue_rows.len() {
                        break;
                    }
                    next = candidate;
                }
            }
            Focus::PlaylistNames => {
                let len = self.state.catalog.playlists.len();
                if len > 0 {
                    self.playlist_name_sel = self
                        .playlist_name_sel
                        .saturating_add_signed(delta)
                        .min(len - 1);
                }
                self.rebuild_playlist_rows();
            }
            Focus::PlaylistTracks => {
                let len = self
                    .state
                    .catalog
                    .playlists
                    .get(self.playlist_name_sel)
                    .map(|list| list.entries.len())
                    .unwrap_or(0);
                if len > 0 {
                    self.playlist_track_sel = self
                        .playlist_track_sel
                        .saturating_add_signed(delta)
                        .min(len - 1);
                }
            }
            Focus::SettingsSections => {
                self.settings_section = self.settings_section.saturating_add_signed(delta).min(4);
                self.settings_row = 0;
                if self.settings_section == 0 {
                    self.refresh_devices();
                }
                self.rebuild_settings_rows();
            }
            Focus::SettingsRows => {
                let len = self.settings_row_count();
                if len > 0 {
                    self.settings_row = self.settings_row.saturating_add_signed(delta).min(len - 1);
                }
            }
        }
    }

    fn cycle_focus(&mut self) {
        self.focus = match self.screen {
            Screen::Library => {
                if self.focus == Focus::Queue {
                    Focus::Library
                } else {
                    Focus::Queue
                }
            }
            Screen::Playlists => {
                if self.focus == Focus::PlaylistNames {
                    Focus::PlaylistTracks
                } else {
                    Focus::PlaylistNames
                }
            }
            Screen::Settings => {
                if self.focus == Focus::SettingsSections {
                    Focus::SettingsRows
                } else {
                    Focus::SettingsSections
                }
            }
        };
        if self.focus == Focus::PlaylistTracks {
            self.playlist_anchor = self.playlist_track_sel;
        }
    }

    fn activate(&mut self) {
        match self.focus {
            Focus::Library => self.browser_activate(),
            Focus::Queue => {
                if let Some(id) = self.queue_selected_pending() {
                    let _ = self.commands.send(PlayerCommand::JumpToQueueEntry(id));
                } else {
                    self.state.status = "only upcoming queue rows can jump".to_owned();
                }
            }
            Focus::PlaylistNames => self.catalog_select(self.playlist_name_sel),
            Focus::PlaylistTracks => {
                let index = self.playlist_name_sel;
                let cursor = self.playlist_track_sel;
                if let Some(list) = self.state.catalog.playlists.get(index) {
                    if cursor < list.entries.len() {
                        self.play_entries(list.entries[cursor..].to_vec());
                    } else {
                        self.state.status = "select a playlist track first".to_owned();
                    }
                }
            }
            Focus::SettingsSections => {
                self.focus = Focus::SettingsRows;
                self.settings_row = 0;
                if self.settings_section == 0 {
                    self.refresh_devices();
                }
            }
            Focus::SettingsRows => self.settings_activate(KeyCode::Enter),
        }
    }

    fn handle_search_key(&mut self, mut search: SearchOverlay, key: KeyEvent) -> bool {
        match keymap::lookup_mode(ModeKind::Search, key) {
            Some(Action::CloseOverlay) => {
                self.worker.send(WorkerMsg::Cancel);
                false
            }
            Some(Action::FocusNext) => {
                search.scope = search.scope.next();
                search.refresh_visible();
                self.mode = Mode::Search(search);
                false
            }
            Some(Action::CursorUp) => {
                search.selected = search.selected.saturating_sub(1);
                self.mode = Mode::Search(search);
                false
            }
            Some(Action::CursorDown) => {
                search.selected = (search.selected + 1).min(search.visible.len().saturating_sub(1));
                self.mode = Mode::Search(search);
                false
            }
            Some(Action::DeleteChar) => {
                search.query.pop();
                self.update_search_query(&mut search);
                self.mode = Mode::Search(search);
                false
            }
            Some(
                action @ (Action::Activate
                | Action::EnqueueSelected
                | Action::AddToPlaylist
                | Action::OpenFolder),
            ) => {
                let item = search.selected_item().cloned();
                self.mode = Mode::Search(search);
                if let Some(item) = item {
                    self.search_selected_action(item, action);
                }
                false
            }
            _ => {
                if let KeyCode::Char(ch) = key.code
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    search.query.push(ch);
                    self.update_search_query(&mut search);
                }
                self.mode = Mode::Search(search);
                false
            }
        }
    }

    fn handle_move_key(&mut self, mut moving: MoveOverlay, key: KeyEvent) -> bool {
        let action = keymap::lookup_mode(ModeKind::Move, key);
        match (&mut moving, action) {
            (_, Some(Action::CloseOverlay)) => false,
            (MoveOverlay::Playlist { gap, .. }, Some(Action::CursorUp)) => {
                *gap = gap.saturating_sub(1);
                self.mode = Mode::Move(moving);
                false
            }
            (MoveOverlay::Playlist { indices, gap }, Some(Action::CursorDown)) => {
                let count = self.state.catalog.playlists[self.playlist_name_sel]
                    .entries
                    .len();
                *gap = (*gap + 1).min(count.saturating_sub(indices.len()));
                self.mode = Mode::Move(moving);
                false
            }
            (MoveOverlay::Playlist { indices, gap }, Some(Action::Activate)) => {
                let indices = indices.clone();
                let gap = *gap;
                self.catalog_move_rows(indices, gap);
                false
            }
            (MoveOverlay::Queue { target, .. }, Some(Action::CursorUp | Action::CursorDown)) => {
                let direction = if action == Some(Action::CursorUp) {
                    -1
                } else {
                    1
                };
                let pending = &self.state.pending;
                let position = target
                    .and_then(|id| pending.iter().position(|item| item.id == id))
                    .unwrap_or(pending.len());
                let next = position.saturating_add_signed(direction).min(pending.len());
                *target = pending.get(next).map(|item| item.id); // None is the end gap
                self.queue_selected = *target;
                self.mode = Mode::Move(moving);
                false
            }
            (MoveOverlay::Queue { id, target }, Some(Action::Activate)) => {
                self.queue_selected = Some(*id);
                // Some(id) is an engine no-op; None means append.
                let _ = self.commands.send(PlayerCommand::MoveQueueEntry {
                    id: *id,
                    before: *target,
                });
                false
            }
            (_, Some(Action::Quit)) => {
                self.mode = Mode::Move(moving);
                self.request_quit()
            }
            _ => {
                self.mode = Mode::Move(moving);
                false
            }
        }
    }

    fn handle_prompt_key(&mut self, mut prompt: Prompt, key: KeyEvent) -> bool {
        let mode = if matches!(prompt.kind, PromptKind::QuitConfirm) {
            ModeKind::QuitConfirm
        } else {
            ModeKind::Prompt
        };
        let action = keymap::lookup_mode(mode, key);
        match prompt.kind {
            PromptKind::QuitConfirm => match action {
                Some(Action::ConfirmYes) => {
                    if self.save_catalog() {
                        true
                    } else {
                        self.mode = Mode::Prompt(prompt);
                        false
                    }
                }
                Some(Action::ConfirmNo) => true,
                Some(Action::CloseOverlay) => {
                    self.mode = *prompt.origin;
                    false
                }
                _ => {
                    self.mode = Mode::Prompt(prompt);
                    false
                }
            },
            PromptKind::Text(_) => match action {
                Some(Action::CloseOverlay) => {
                    self.mode = *prompt.origin;
                    false
                }
                Some(Action::Activate) => {
                    let origin = *prompt.origin;
                    let kind = prompt.kind;
                    self.mode = origin;
                    if let PromptKind::Text(action) = kind {
                        self.submit_input(action, prompt.buffer);
                    }
                    false
                }
                Some(Action::DeleteChar) => {
                    prompt.buffer.pop();
                    self.mode = Mode::Prompt(prompt);
                    false
                }
                _ => {
                    if let KeyCode::Char(ch) = key.code
                        && !key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        prompt.buffer.push(ch);
                    }
                    self.mode = Mode::Prompt(prompt);
                    false
                }
            },
        }
    }

    fn handle_locations_key(&mut self, mut locations: LocationsOverlay, key: KeyEvent) -> bool {
        match keymap::lookup_mode(ModeKind::Locations, key) {
            Some(Action::CloseOverlay) => false,
            Some(Action::CursorUp) => {
                locations.selected = locations.selected.saturating_sub(1);
                self.mode = Mode::Locations(locations);
                false
            }
            Some(Action::CursorDown) => {
                locations.selected =
                    (locations.selected + 1).min(locations.locations.len().saturating_sub(1));
                self.mode = Mode::Locations(locations);
                false
            }
            Some(Action::Activate) => {
                let path = locations
                    .locations
                    .get(locations.selected)
                    .map(|location| location.path.clone());
                self.mode = Mode::Locations(locations);
                if let Some(path) = path {
                    self.open_browser(path);
                }
                false
            }
            Some(Action::OpenActions) => {
                self.mode = Mode::Locations(locations);
                self.open_actions();
                false
            }
            Some(Action::Quit) => {
                self.mode = Mode::Locations(locations);
                self.request_quit()
            }
            _ => {
                self.mode = Mode::Locations(locations);
                false
            }
        }
    }

    fn handle_picker_key(&mut self, mut picker: PlaylistPicker, key: KeyEvent) -> bool {
        match keymap::lookup_mode(ModeKind::PlaylistPicker, key) {
            Some(Action::CloseOverlay) => false,
            Some(Action::CursorUp) => {
                picker.selected = picker.selected.saturating_sub(1);
                self.mode = Mode::PlaylistPicker(picker);
                false
            }
            Some(Action::CursorDown) => {
                picker.selected = (picker.selected + 1).min(self.state.catalog.playlists.len());
                self.mode = Mode::PlaylistPicker(picker);
                false
            }
            Some(Action::NewPlaylist) => {
                self.mode = Mode::PlaylistPicker(picker);
                self.start_prompt("New playlist name:", InputAction::PlaylistCreate);
                false
            }
            Some(Action::Activate) => {
                let index = picker.selected;
                let entries = picker.entries;
                if index == self.state.catalog.playlists.len() {
                    self.mode = Mode::PlaylistPicker(PlaylistPicker {
                        entries,
                        selected: index,
                    });
                    self.start_prompt("New playlist name:", InputAction::PlaylistCreate);
                } else {
                    self.catalog_add_to(index, entries);
                }
                false
            }
            Some(Action::Quit) => {
                self.mode = Mode::PlaylistPicker(picker);
                self.request_quit()
            }
            _ => {
                self.mode = Mode::PlaylistPicker(picker);
                false
            }
        }
    }

    fn handle_actions_key(&mut self, mut actions: ActionsOverlay, key: KeyEvent) -> bool {
        match keymap::lookup_mode(ModeKind::Actions, key) {
            Some(Action::CloseOverlay) => {
                self.mode = *actions.origin;
                false
            }
            Some(Action::CursorUp) => {
                actions.selected = actions.selected.saturating_sub(1);
                self.mode = Mode::Actions(actions);
                false
            }
            Some(Action::CursorDown) => {
                actions.selected =
                    (actions.selected + 1).min(actions.items.len().saturating_sub(1));
                self.mode = Mode::Actions(actions);
                false
            }
            Some(Action::Activate) => {
                let action = actions
                    .items
                    .get(actions.selected)
                    .map(|(_, action)| action.clone());
                self.mode = *actions.origin;
                if let Some(action) = action {
                    return self.run_action(action);
                }
                false
            }
            Some(Action::Quit) => {
                self.mode = Mode::Actions(actions);
                self.request_quit()
            }
            _ => {
                self.mode = Mode::Actions(actions);
                false
            }
        }
    }
}

/// Start the TUI in raw/alternate-screen mode and always restore the
/// terminal on return.
pub fn run(
    commands: Sender<PlayerCommand>,
    events: Receiver<PlayerEvent>,
    options: RunOptions,
    hooks: Hooks,
) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = event_loop(&mut terminal, commands, events, options, hooks);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    terminal.clear()?;
    Ok(())
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    commands: Sender<PlayerCommand>,
    events: Receiver<PlayerEvent>,
    options: RunOptions,
    hooks: Hooks,
) -> io::Result<()> {
    let mut app = App::new(options, hooks, commands);
    let mut dirty = true;
    let mut progress_dirty = false;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    loop {
        while let Ok(event) = events.try_recv() {
            if app.apply_event(event) {
                dirty = true;
            } else {
                progress_dirty = true;
            }
        }
        if app.poll_worker() || app.poll_collect() {
            dirty = true;
        }
        if dirty || (progress_dirty && last_draw.elapsed() >= Duration::from_millis(250)) {
            let size = terminal.size()?;
            let main_height = layout::home(Rect::new(0, 0, size.width, size.height))
                .library
                .height
                .saturating_sub(5) as usize;
            app.prime_metadata(main_height);
            terminal.draw(|frame| draw(frame, &app))?;
            last_draw = Instant::now();
            dirty = false;
            progress_dirty = false;
        }
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if app.handle_key_event(key) {
                        return Ok(());
                    }
                    if let Some(action) = app.pending_os.take() {
                        suspend_terminal(terminal)?;
                        app.run_os_action(action);
                        resume_terminal(terminal)?;
                    }
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let regions = layout::home(area);
    draw_header(frame, app, regions.header);
    let player_data = player_view(app);
    let player_view = player_data.as_view();
    let main = Rect {
        x: area.x,
        y: regions.library.y,
        width: area.width,
        height: regions.library.height,
    };
    match app.screen {
        Screen::Library => {
            let queued: HashSet<(PathBuf, Option<sointty_core::CueRange>)> = app
                .state
                .audible
                .iter()
                .chain(app.state.pending.iter())
                .map(|item| screens::library::queue_key(&item.entry))
                .collect();
            if regions.queue_visible {
                if let Some(browser) = &app.browser {
                    screens::library::render(
                        frame,
                        regions.library,
                        browser,
                        app.focus == Focus::Library,
                        &queued,
                        app,
                        &app.theme,
                    );
                } else {
                    draw_empty_library(frame, regions.library, app);
                }
                draw_queue(frame, regions.queue, app);
            } else if app.focus == Focus::Queue {
                draw_queue(frame, main, app);
            } else if let Some(browser) = &app.browser {
                screens::library::render(frame, main, browser, true, &queued, app, &app.theme);
            } else {
                draw_empty_library(frame, main, app);
            }
            if regions.tier == layout::Tier::Wide {
                screens::player::render_panel(frame, regions.player, &player_view, &app.theme);
            } else {
                screens::player::render_mini(frame, regions.player, &player_view, &app.theme);
            }
        }
        Screen::Playlists => {
            draw_playlists(frame, main, app, regions.tier != layout::Tier::Wide);
            screens::player::render_mini(frame, regions.player, &player_view, &app.theme);
        }
        Screen::Settings => {
            draw_settings(frame, main, app, regions.tier != layout::Tier::Wide);
            screens::player::render_mini(frame, regions.player, &player_view, &app.theme);
        }
    }
    let mut footer = keymap::footer_pairs(app.focus, app.mode.kind());
    if app.mode.kind() == ModeKind::Normal && !footer.iter().any(|(key, _)| *key == "?") {
        footer.push(("?", "all keys"));
    }
    widgets::render_footer(frame, regions.footer, &footer, &app.theme);
    if regions.footer.height >= 2 && app.mode.kind() == ModeKind::Normal {
        let playback_row = Rect {
            y: regions.footer.y.saturating_add(1),
            height: 1,
            ..regions.footer
        };
        widgets::render_footer(frame, playback_row, &keymap::playback_pairs(), &app.theme);
    }
    draw_overlay(frame, app);
}

fn draw_header(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let theme = &app.theme;
    if layout::tier(frame.area()) == layout::Tier::Narrow {
        frame.render_widget(
            Paragraph::new(widgets::truncate_cells(
                "sointty · small terminal · Tab switches pane",
                area.width as usize,
            ))
            .style(theme.dimmed()),
            area,
        );
        return;
    }
    let tabs = [
        (Screen::Library, "1 Library"),
        (Screen::Playlists, "2 Playlists"),
        (Screen::Settings, "3 Settings"),
    ];
    let mut spans = vec![Span::styled(
        " sointty  ",
        theme.strong().add_modifier(Modifier::BOLD),
    )];
    for (screen, label) in tabs {
        spans.push(Span::styled(
            format!(" {label} "),
            if app.screen == screen {
                theme.accent_style().add_modifier(Modifier::BOLD)
            } else {
                theme.dimmed()
            },
        ));
    }
    let badge = match (&app.state.audible, &app.state.output_spec) {
        (Some(_), Some(_)) if !app.state.paused && app.state.shared_active => "● shared",
        (Some(_), Some(_)) if !app.state.paused && app.state.converted => "● converted",
        (Some(_), Some(_)) if !app.state.paused => "● bit-perfect",
        _ => "○ idle",
    };
    spans.push(Span::styled(
        format!("  {badge}  "),
        if badge.contains("bit-perfect") {
            theme.ok_style()
        } else if badge.contains("converted") || badge.contains("shared") {
            theme.accent_style()
        } else {
            theme.faint_style()
        },
    ));
    spans.push(Span::styled(
        widgets::truncate_cells(&app.state.status, area.width.saturating_sub(80) as usize),
        theme.dimmed(),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme.base()), area);
}

fn draw_empty_library(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let block = widgets::panel("Library", None, &app.theme, app.focus == Focus::Library);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new("No readable folder. Press l for Locations."),
        inner,
    );
}

fn draw_queue(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let current_remaining = if app.state.audible.is_some() {
        app.state.total_frames.map(|total| {
            Duration::from_secs(
                total.saturating_sub(app.state.frame) / u64::from(app.state.rate_hz.max(1)),
            )
        })
    } else {
        None
    };
    let track_count = app.state.pending.len() + usize::from(app.state.audible.is_some());
    let time_left = match (
        app.pending_duration_total,
        current_remaining,
        app.state.audible.is_some(),
    ) {
        (Some(pending), Some(current), true) => {
            format!("{} left", screens::clock(pending + current))
        }
        (Some(pending), _, false) => format!("{} left", screens::clock(pending)),
        _ => "time unknown".to_owned(),
    };
    let selected_row = app
        .queue_selected
        .and_then(|id| screens::queue::row_of(&app.queue_rows, id));
    screens::queue::render(
        frame,
        area,
        &screens::queue::QueueView {
            rows: &app.queue_rows,
            selected_row,
            focused: app.focus == Focus::Queue,
            shuffle: app.state.shuffle,
            track_count,
            time_left: &time_left,
        },
        &app.theme,
    );
}

fn draw_playlists(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App, narrow: bool) {
    let list = app
        .state
        .catalog
        .playlists
        .get(app.playlist_name_sel)
        .unwrap_or_else(|| app.state.catalog.selected_playlist());
    let lifted: HashSet<usize> = match &app.mode {
        Mode::Move(MoveOverlay::Playlist { indices, .. }) => indices.iter().copied().collect(),
        _ => HashSet::new(),
    };
    let move_gap = match &app.mode {
        Mode::Move(MoveOverlay::Playlist { gap, .. }) => Some(*gap),
        _ => None,
    };
    let path = app
        .state
        .catalog_path
        .as_ref()
        .map(|path| path.display().to_string());
    screens::playlists::render(
        frame,
        area,
        &screens::playlists::PlaylistsView {
            names: &app.playlist_names_cache,
            name_sel: app.playlist_name_sel,
            tracks: &app.playlist_tracks_cache,
            track_cursor: app.playlist_track_sel,
            checked: &app.playlist_selected,
            lifted: &lifted,
            tracks_title: &list.name,
            tracks_dirty: app.dirty_playlists.contains(&app.playlist_name_sel),
            focus: app.focus,
            catalog_path: path.as_deref(),
            catalog_error: app.state.catalog_error.as_deref(),
            move_gap,
        },
        &app.theme,
        narrow,
    );
}

fn draw_settings(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App, narrow: bool) {
    const SECTIONS: &[&str] = &[
        "Output",
        "Playback",
        "Library & locations",
        "Appearance",
        "Keys",
    ];
    let rows = &app.settings_rows_cache;
    let path = app
        .state
        .config_path
        .as_ref()
        .map(|path| path.display().to_string());
    let signal = match (app.state.source_spec, app.state.output_spec.as_ref()) {
        (Some(DecodedSpec::Pcm(source)), Some(output)) => Some(format!(
            "{:?} {} Hz · {} ch → {:?} {} Hz · {} ch → {}",
            source.encoding,
            source.rate_hz,
            source.layout.channels,
            output.format,
            output.rate_hz,
            output.layout.channels,
            output.device,
        )),
        (Some(DecodedSpec::Dsd(source)), Some(output)) => Some(format!(
            "DSD {} Hz · {} ch → {:?} {} Hz · {} ch → {}",
            source.dsd_rate_hz,
            source.layout.channels,
            output.format,
            output.rate_hz,
            output.layout.channels,
            output.device,
        )),
        _ => None,
    };
    screens::settings::render(
        frame,
        area,
        &screens::settings::SettingsView {
            sections: SECTIONS,
            section_sel: app.settings_section,
            rows,
            row_sel: app.settings_row,
            focus: app.focus,
            config_path: path.as_deref(),
            config_error: app.state.config_error.as_deref(),
            signal_path: signal.as_deref(),
        },
        &app.theme,
        narrow,
    );
}

fn player_view(app: &App) -> OwnedPlayerView {
    let badge = match (&app.state.audible, &app.state.output_spec) {
        (Some(_), Some(_)) if !app.state.paused && app.state.shared_active => {
            screens::player::PlaybackBadge::Shared
        }
        (Some(_), Some(_)) if !app.state.paused && app.state.converted => {
            screens::player::PlaybackBadge::Converted
        }
        (Some(_), Some(_)) if !app.state.paused => screens::player::PlaybackBadge::BitPerfect,
        _ => screens::player::PlaybackBadge::Idle,
    };
    let entry = app.state.audible.as_ref().map(|item| &item.entry);
    let (title, _tag) = entry
        .map(|entry| display::display_entry_title(entry, Some(&app.state.tags)))
        .unwrap_or_default();
    let subtitle = match (&app.state.tags.artist, &app.state.tags.album) {
        (Some(artist), Some(album)) => Some(format!("{artist} · {album}")),
        (Some(artist), None) => Some(artist.clone()),
        (None, Some(album)) => Some(album.clone()),
        (None, None) => None,
    };
    let rate = u64::from(app.state.rate_hz.max(1));
    let elapsed = Duration::from_secs(app.state.frame / rate);
    let total = app
        .state
        .total_frames
        .map(|frames| Duration::from_secs(frames / rate));
    let source = app.state.source_spec.map(|spec| match spec {
        DecodedSpec::Pcm(pcm) => format!(
            "{:?} {} Hz · {} ch",
            pcm.encoding, pcm.rate_hz, pcm.layout.channels
        ),
        DecodedSpec::Dsd(dsd) => format!("DSD {} Hz · {} ch", dsd.dsd_rate_hz, dsd.layout.channels),
    });
    let output = app.state.output_spec.as_ref().map(|spec| {
        let name = app
            .state
            .device_names
            .iter()
            .find(|(id, _)| *id == spec.device)
            .map(|(_, name)| name.as_str())
            .unwrap_or(&spec.device);
        format!(
            "{:?} {} Hz · {} ch · {} valid bits · {name}",
            spec.format, spec.rate_hz, spec.layout.channels, spec.valid_bits
        )
    });
    let buffers = app.state.buffers.map(|b| {
        format!(
            "period {} / buffer {} / ring {} frames",
            b.period_frames, b.buffer_frames, b.ring_frames,
        )
    });
    let path = entry.map(|entry| entry.path.display().to_string());
    OwnedPlayerView::new(
        badge,
        title,
        subtitle,
        elapsed,
        total,
        app.state.paused,
        app.state.status.clone(),
        source,
        output,
        buffers,
        path,
    )
}

/// Draw-local owned strings backing the borrowed `PlayerView`.
struct OwnedPlayerView {
    badge: screens::player::PlaybackBadge,
    title: String,
    subtitle: Option<String>,
    elapsed: Duration,
    total: Option<Duration>,
    paused: bool,
    status: String,
    source: Option<String>,
    output: Option<String>,
    buffers: Option<String>,
    path: Option<String>,
}

impl OwnedPlayerView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        badge: screens::player::PlaybackBadge,
        title: String,
        subtitle: Option<String>,
        elapsed: Duration,
        total: Option<Duration>,
        paused: bool,
        status: String,
        source: Option<String>,
        output: Option<String>,
        buffers: Option<String>,
        path: Option<String>,
    ) -> Self {
        Self {
            badge,
            title,
            subtitle,
            elapsed,
            total,
            paused,
            status,
            source,
            output,
            buffers,
            path,
        }
    }

    fn as_view(&self) -> screens::player::PlayerView<'_> {
        screens::player::PlayerView {
            badge: self.badge,
            title: &self.title,
            subtitle: self.subtitle.as_deref(),
            elapsed: self.elapsed,
            total: self.total,
            paused: self.paused,
            status: &self.status,
            source: self.source.as_deref(),
            output: self.output.as_deref(),
            buffers: self.buffers.as_deref(),
            path: self.path.as_deref(),
        }
    }
}

fn draw_overlay(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    let theme = &app.theme;
    match &app.mode {
        Mode::Normal => {}
        Mode::Search(search) => {
            let overlay = layout::centered_overlay(area, area.height.saturating_sub(4).min(30));
            frame.render_widget(Clear, overlay);
            let block = widgets::panel("Search", None, theme, true);
            let inner = block.inner(overlay);
            frame.render_widget(block, overlay);
            if inner.height == 0 {
                return;
            }
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(" / ", theme.accent_style()),
                    Span::styled(format!("{}▏", search.query), theme.strong()),
                ]),
                Line::from(Span::styled(
                    format!(
                        "All / Albums / Tracks / Folders / Playlists     scope: {}",
                        search.scope.label()
                    ),
                    theme.dimmed(),
                )),
                Line::from(Span::styled(
                    match &search.status {
                        SearchStatus::Indexing => {
                            format!("{} results · indexing…", search.visible.len())
                        }
                        SearchStatus::Done => format!("{} results", search.visible.len()),
                        SearchStatus::Incomplete { skipped_roots } => format!(
                            "{} results · incomplete ({skipped_roots} skipped roots)",
                            search.visible.len()
                        ),
                    },
                    theme.faint_style(),
                )),
            ];
            if search.query.is_empty() {
                lines.push(Line::from(Span::styled(
                    "Type to search locations. Tab changes scope; Esc closes.",
                    theme.faint_style(),
                )));
            } else {
                let display_rows = search.display_rows();
                let selected_result = search.visible.get(search.selected).copied();
                let selected_display = display_rows.iter().position(|row| {
                    matches!(row, SearchDisplayRow::Item(index) if Some(*index) == selected_result)
                }).unwrap_or(0);
                let rows_height = inner.height.saturating_sub(lines.len() as u16) as usize;
                let (start, end) =
                    screens::visible_window(display_rows.len(), selected_display, rows_height);
                for row in &display_rows[start..end] {
                    match row {
                        SearchDisplayRow::Header(label, count) => lines.push(Line::from(
                            Span::styled(format!("  {label} ({count})"), theme.accent_style()),
                        )),
                        SearchDisplayRow::More(count) => lines.push(Line::from(Span::styled(
                            format!("    … {count} more"),
                            theme.faint_style(),
                        ))),
                        SearchDisplayRow::Item(result_index) => {
                            let item = &search.results[*result_index];
                            let (kind, title, subtitle, title_spans, subtitle_spans): (
                                &str,
                                &str,
                                Option<&str>,
                                &[usize],
                                &[usize],
                            ) = match item {
                                SearchItem::Worker(hit) => (
                                    match hit.kind {
                                        HitKind::Track => "♪",
                                        HitKind::Album => "▤",
                                        HitKind::Folder => "▸",
                                        HitKind::PlaylistFile => "≡",
                                    },
                                    &hit.title,
                                    hit.subtitle.as_deref(),
                                    &hit.spans,
                                    &hit.subtitle_spans,
                                ),
                                SearchItem::Catalog { index, spans, .. } => (
                                    "≡",
                                    &app.state.catalog.playlists[*index].name,
                                    Some("saved playlist"),
                                    spans,
                                    &[],
                                ),
                            };
                            let selected = Some(*result_index) == selected_result;
                            let mut spans = vec![
                                Span::styled(
                                    if selected { "› " } else { "  " },
                                    theme.accent_style(),
                                ),
                                Span::styled(format!("{kind} "), theme.dimmed()),
                            ];
                            let title = widgets::truncate_cells(
                                title,
                                inner.width.saturating_sub(12) as usize,
                            );
                            for (index, ch) in title.chars().enumerate() {
                                let style = if title_spans.contains(&index) {
                                    theme.accent_style().add_modifier(Modifier::BOLD)
                                } else {
                                    theme.base()
                                };
                                spans.push(Span::styled(ch.to_string(), style));
                            }
                            if let Some(subtitle) = subtitle {
                                spans.push(Span::raw("  "));
                                let subtitle = widgets::truncate_cells(
                                    subtitle,
                                    inner
                                        .width
                                        .saturating_sub(widgets::text_cells(&title) as u16 + 6)
                                        as usize,
                                );
                                for (index, ch) in subtitle.chars().enumerate() {
                                    let style = if subtitle_spans.contains(&index) {
                                        theme.accent_style().add_modifier(Modifier::BOLD)
                                    } else {
                                        theme.faint_style()
                                    };
                                    spans.push(Span::styled(ch.to_string(), style));
                                }
                            }
                            lines.push(Line::from(spans).style(if selected {
                                theme.selected()
                            } else {
                                Style::default()
                            }));
                        }
                    }
                }
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Mode::Help => {
            let overlay = layout::centered_overlay(area, area.height.saturating_sub(2).min(36));
            frame.render_widget(Clear, overlay);
            let block = widgets::panel("All keys · ?/Esc closes", None, theme, true);
            let inner = block.inner(overlay);
            frame.render_widget(block, overlay);
            let mut lines = Vec::new();
            for (group, rows) in keymap::help_groups() {
                lines.push(Line::from(Span::styled(
                    group.to_owned(),
                    theme.accent_style(),
                )));
                for (key, label) in rows {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {key:<12}"), theme.keycap()),
                        Span::styled(label, theme.base()),
                    ]));
                }
            }
            let start = app
                .help_scroll
                .min(lines.len().saturating_sub(inner.height as usize));
            let end = (start + inner.height as usize).min(lines.len());
            frame.render_widget(Paragraph::new(lines[start..end].to_vec()), inner);
        }
        Mode::Locations(locations) => {
            let overlay = layout::centered_overlay(area, area.height.saturating_sub(6).min(22));
            frame.render_widget(Clear, overlay);
            let block = widgets::panel(
                "Locations · Enter browse · x actions",
                Some(locations.locations.len()),
                theme,
                true,
            );
            let inner = block.inner(overlay);
            frame.render_widget(block, overlay);
            let mut lines = Vec::new();
            for (index, location) in locations
                .locations
                .iter()
                .enumerate()
                .take(inner.height as usize)
            {
                let selected = index == locations.selected;
                lines.push(
                    Line::from(vec![
                        Span::styled(if selected { "› " } else { "  " }, theme.accent_style()),
                        Span::styled(
                            widgets::truncate_cells(
                                &location.label,
                                inner.width.saturating_sub(2) as usize,
                            ),
                            theme.base(),
                        ),
                    ])
                    .style(if selected {
                        theme.selected()
                    } else {
                        Style::default()
                    }),
                );
            }
            if lines.is_empty() {
                lines.push(Line::from("No locations; x → Enter path"));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Mode::PlaylistPicker(picker) => {
            let overlay = layout::centered_overlay(area, area.height.saturating_sub(6).min(22));
            frame.render_widget(Clear, overlay);
            let block = widgets::panel("Add to playlist", None, theme, true);
            let inner = block.inner(overlay);
            frame.render_widget(block, overlay);
            let mut lines = Vec::new();
            for (index, list) in app
                .state
                .catalog
                .playlists
                .iter()
                .enumerate()
                .take(inner.height as usize)
            {
                let selected = index == picker.selected;
                lines.push(
                    Line::from(vec![
                        Span::styled(if selected { "› " } else { "  " }, theme.accent_style()),
                        Span::styled(
                            format!("{} ({})", list.name, list.entries.len()),
                            theme.base(),
                        ),
                    ])
                    .style(if selected {
                        theme.selected()
                    } else {
                        Style::default()
                    }),
                );
            }
            lines.push(Line::from(Span::styled(
                "+ New playlist",
                theme.accent_style(),
            )));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Mode::Actions(actions) => {
            let overlay =
                layout::centered_overlay(area, (actions.items.len() as u16 + 2).min(area.height));
            frame.render_widget(Clear, overlay);
            let block = widgets::panel("Actions", None, theme, true);
            let inner = block.inner(overlay);
            frame.render_widget(block, overlay);
            let lines: Vec<Line<'static>> = actions
                .items
                .iter()
                .enumerate()
                .map(|(index, (label, _))| {
                    let selected = index == actions.selected;
                    Line::from(vec![
                        Span::styled(if selected { "› " } else { "  " }, theme.accent_style()),
                        Span::styled(label.clone(), theme.base()),
                    ])
                    .style(if selected {
                        theme.selected()
                    } else {
                        Style::default()
                    })
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Mode::Prompt(prompt) => {
            let overlay = layout::centered_overlay(area, 5);
            frame.render_widget(Clear, overlay);
            let block = widgets::panel("Input", None, theme, true);
            let inner = block.inner(overlay);
            frame.render_widget(block, overlay);
            let text = match prompt.kind {
                PromptKind::QuitConfirm => prompt.label.clone(),
                PromptKind::Text(_) => format!("{}\n{}▏", prompt.label, prompt.buffer),
            };
            frame.render_widget(Paragraph::new(text).style(theme.base()), inner);
        }
        Mode::Move(MoveOverlay::Queue { id, target }) => {
            let overlay = layout::centered_overlay(area, 3);
            frame.render_widget(Clear, overlay);
            let block = widgets::panel("Move queue entry", None, theme, true);
            let inner = block.inner(overlay);
            frame.render_widget(block, overlay);
            frame.render_widget(
                Paragraph::new(format!(
                    "Move #{id} before {:?} · ↑↓ target · Enter apply · Esc cancel",
                    target
                )),
                inner,
            );
        }
        Mode::Move(MoveOverlay::Playlist { .. }) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    struct Harness {
        app: App,
        sent: Receiver<PlayerCommand>,
        saves: Arc<Mutex<Vec<PlaylistCatalog>>>,
    }

    fn options() -> RunOptions {
        RunOptions {
            open_browser: false,
            initial_browser_dir: None,
            home_dir: None,
            current_device: "dev1".to_owned(),
            float_to_int: false,
            int_to_float: false,
            shared_playback: false,
            timing: (None, None),
            catalog: PlaylistCatalog::default(),
            catalog_error: None,
            catalog_path: Some(PathBuf::from("/data/playlists.toml")),
            config_path: Some(PathBuf::from("/config/config.toml")),
            config_error: None,
            accent: None,
            search_roots: Vec::new(),
            search_coverage_error: None,
        }
    }

    fn harness() -> Harness {
        let (commands, sent) = crossbeam_channel::unbounded();
        let saves = Arc::new(Mutex::new(Vec::new()));
        let saver = Arc::clone(&saves);
        let hooks = Hooks {
            device_provider: Box::new(|| vec![("dev1".to_owned(), "DAC".to_owned())]),
            on_device_selected: Box::new(|_| Ok(())),
            on_device_reset: Box::new(|| Ok("dev1".to_owned())),
            on_float_to_int_selected: Box::new(|_| Ok(())),
            on_int_to_float_selected: Box::new(|_| Ok(())),
            on_shared_playback_selected: Box::new(|_| Ok(())),
            on_browser_dir: Box::new(|_| Ok(())),
            on_catalog: Box::new(move |catalog| {
                saver.lock().push(catalog.clone());
                Ok(())
            }),
            on_timing: Box::new(|_, _| Ok(())),
            on_accent: Box::new(|_| Ok(())),
            locations: Box::new(|| Ok(Vec::new())),
            map_drive: None,
            disconnect_drive: None,
            mount_share: None,
        };
        Harness {
            app: App::new(options(), hooks, commands),
            sent,
            saves,
        }
    }

    impl Harness {
        fn key(&mut self, code: KeyCode) -> bool {
            self.app
                .handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
        }
    }

    #[test]
    fn global_playback_keys_work_outside_text_input() {
        let mut h = harness();
        h.key(KeyCode::Char('n'));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Next));
        h.key(KeyCode::Char(' '));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Pause));
        h.key(KeyCode::Char('s'));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Stop));
    }

    #[test]
    fn shift_modified_keys_dispatch_in_app() {
        let mut h = harness();
        // Shift+space arrives as Char(' ') with SHIFT; it must pause.
        assert!(!h.app.handle_key_event(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::SHIFT
        )));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Pause));
        // Shift+/ arrives as Char('?') with SHIFT; it must open help.
        assert!(!h.app.handle_key_event(KeyEvent::new(
            KeyCode::Char('?'),
            KeyModifiers::SHIFT
        )));
        assert!(matches!(h.app.mode, Mode::Help));
    }

    #[test]
    fn shared_toggle_saves_first_and_updates_settings_access_immediately() {
        let mut h = harness();
        h.app.hooks.on_shared_playback_selected = Box::new(|_| Err("invalid TOML".to_owned()));
        h.key(KeyCode::Char('g'));
        assert!(
            !h.app.state.shared_playback,
            "failed save leaves preference untouched"
        );
        assert!(
            h.sent.try_recv().is_err(),
            "failed save must not switch backend"
        );
        assert!(h.app.state.status.contains("not saved"));

        h.app.hooks.on_shared_playback_selected = Box::new(|_| Ok(()));
        h.key(KeyCode::Char('g'));
        assert!(h.app.state.shared_playback);
        assert!(matches!(
            h.sent.recv().unwrap(),
            PlayerCommand::SetSharedPlayback(true)
        ));
        assert_eq!(h.app.settings_rows_cache[1].value, "shared / OS mixer");
        assert!(
            h.app.settings_rows_cache[1]
                .warning
                .as_deref()
                .unwrap()
                .contains("not bit-perfect")
        );

        h.app.screen = Screen::Settings;
        h.app.focus = Focus::SettingsRows;
        h.app.settings_section = 0;
        h.app.settings_row = 1;
        h.key(KeyCode::Char('r'));
        assert!(!h.app.state.shared_playback);
        assert!(matches!(
            h.sent.recv().unwrap(),
            PlayerCommand::SetSharedPlayback(false)
        ));
        assert_eq!(h.app.settings_rows_cache[1].value, "direct / exclusive");
        h.key(KeyCode::Enter);
        assert!(h.app.state.shared_playback);
        assert!(matches!(
            h.sent.recv().unwrap(),
            PlayerCommand::SetSharedPlayback(true)
        ));
        h.key(KeyCode::Char(' '));
        assert!(!h.app.state.shared_playback);
        assert!(matches!(
            h.sent.recv().unwrap(),
            PlayerCommand::SetSharedPlayback(false)
        ));
    }

    #[test]
    fn text_input_owns_printable_keys_and_ctrl_q_requests_quit() {
        let mut h = harness();
        h.app.start_prompt("Name:", InputAction::PlaylistCreate);
        for ch in ['q', 's', 'n', 'r', 'g', ' ', '?'] {
            assert!(!h.key(KeyCode::Char(ch)));
        }
        let Mode::Prompt(prompt) = &h.app.mode else {
            panic!("prompt should stay open");
        };
        assert_eq!(prompt.buffer, "qsnrg ?");
        assert!(h.sent.try_recv().is_err());
        assert!(
            h.app
                .handle_key_event(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn queue_snapshot_reconciles_selection_by_id() {
        let mut h = harness();
        h.app.apply_event(PlayerEvent::QueueChanged {
            audible: None,
            pending: vec![
                QueueItem {
                    id: 1,
                    entry: QueueEntry::from(PathBuf::from("/a.flac")),
                },
                QueueItem {
                    id: 2,
                    entry: QueueEntry::from(PathBuf::from("/b.flac")),
                },
            ],
        });
        h.app.focus = Focus::Queue;
        h.app.queue_selected = Some(2);
        h.app.apply_event(PlayerEvent::QueueChanged {
            audible: None,
            pending: vec![QueueItem {
                id: 2,
                entry: QueueEntry::from(PathBuf::from("/b.flac")),
            }],
        });
        assert_eq!(h.app.queue_selected, Some(2));
        h.app.apply_event(PlayerEvent::QueueChanged {
            audible: None,
            pending: Vec::new(),
        });
        assert_eq!(h.app.queue_selected, None);
    }

    #[test]
    fn playlist_dirty_save_failure_retry_and_quit_prompt() {
        let mut h = harness();
        h.app.catalog_create("Chill".to_owned());
        assert!(h.app.has_unsaved_catalog());
        assert!(!h.key(KeyCode::Char('q')));
        assert!(matches!(
            h.app.mode,
            Mode::Prompt(Prompt {
                kind: PromptKind::QuitConfirm,
                ..
            })
        ));
        h.key(KeyCode::Esc);
        assert!(h.app.has_unsaved_catalog());
        h.app.save_catalog();
        assert!(!h.app.has_unsaved_catalog());
        assert_eq!(h.saves.lock().len(), 1);
        assert!(h.key(KeyCode::Char('q')));
    }

    #[test]
    fn tiny_draw_and_resize_restore_do_not_panic() {
        let h = harness();
        for (w, hgt) in [(1, 1), (40, 12), (79, 23), (80, 24), (140, 40)] {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, hgt)).unwrap();
            terminal.draw(|frame| draw(frame, &h.app)).unwrap();
            assert_eq!(terminal.backend().buffer().area, Rect::new(0, 0, w, hgt));
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "sointty-tui-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ))
    }

    fn item(id: TrackId, path: impl Into<PathBuf>) -> QueueItem {
        QueueItem {
            id,
            entry: QueueEntry::from(path.into()),
        }
    }

    #[test]
    fn library_enter_plays_now_a_only_queues_and_folder_skips_junk() {
        let root = temp_dir("library-actions");
        let folder = root.join("folder");
        std::fs::create_dir_all(&folder).unwrap();
        let real = root.join("01-Track.flac");
        std::fs::write(&real, b"").unwrap();
        std::fs::write(root.join("._01-Track.flac"), b"").unwrap();
        std::fs::write(root.join(".hidden.flac"), b"").unwrap();
        std::fs::write(root.join("Thumbs.db"), b"").unwrap();
        let nested = folder.join("nested.wav");
        std::fs::write(&nested, b"").unwrap();
        std::fs::write(folder.join("._nested.wav"), b"").unwrap();
        let mut h = harness();
        h.app.browser = Some(Browser::open(root.clone()).unwrap());
        // Directory first; choose the real audio file.
        h.app.browser.as_mut().unwrap().selected = 1;
        assert_eq!(h.app.browser.as_ref().unwrap().items.len(), 2);
        h.key(KeyCode::Enter);
        assert!(
            matches!(h.sent.recv().unwrap(), PlayerCommand::PlayNow(entry) if entry.path == real)
        );
        h.key(KeyCode::Char('a'));
        assert!(
            matches!(h.sent.recv().unwrap(), PlayerCommand::Enqueue(entry) if entry.path == real)
        );
        assert!(h.sent.try_recv().is_err(), "a must not send Play");
        h.app.browser.as_mut().unwrap().selected = 0;
        h.key(KeyCode::Char('a'));
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.app.collect_op.is_some() && Instant::now() < deadline {
            h.app.poll_collect();
            std::thread::yield_now();
        }
        assert!(
            h.app.collect_op.is_none(),
            "folder collection must complete"
        );
        assert!(
            matches!(h.sent.recv().unwrap(), PlayerCommand::Enqueue(entry) if entry.path == nested)
        );
        assert!(
            h.sent.try_recv().is_err(),
            "folder add enqueues exactly one real file"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queue_focus_uses_engine_ids_and_past_is_ui_only() {
        let mut h = harness();
        h.app.apply_event(PlayerEvent::QueueChanged {
            audible: Some(item(1, "/now.flac")),
            pending: vec![item(2, "/next.flac"), item(3, "/later.flac")],
        });
        h.app.focus = Focus::Queue;
        h.app.queue_selected = Some(2);
        h.key(KeyCode::Char('d'));
        assert!(matches!(
            h.sent.recv().unwrap(),
            PlayerCommand::RemoveQueueEntry(2)
        ));
        h.key(KeyCode::Char('m'));
        assert!(matches!(
            h.app.mode,
            Mode::Move(MoveOverlay::Queue { id: 2, .. })
        ));
        h.key(KeyCode::Down);
        h.key(KeyCode::Enter);
        assert!(matches!(
            h.sent.recv().unwrap(),
            PlayerCommand::MoveQueueEntry {
                id: 2,
                before: Some(3)
            }
        ));
        h.app.queue_selected = Some(3);
        h.key(KeyCode::Enter);
        assert!(matches!(
            h.sent.recv().unwrap(),
            PlayerCommand::JumpToQueueEntry(3)
        ));
        h.app.queue_selected = Some(1);
        h.key(KeyCode::Char('d'));
        assert!(h.app.state.status.contains("read-only"));
        assert!(h.sent.try_recv().is_err());
        // Only a different audible ID advances history.
        h.app.apply_event(PlayerEvent::Paused);
        h.app.apply_event(PlayerEvent::QueueChanged {
            audible: Some(item(1, "/now.flac")),
            pending: vec![item(2, "/next.flac")],
        });
        assert!(h.app.past.is_empty());
        h.app.apply_event(PlayerEvent::QueueChanged {
            audible: Some(item(2, "/next.flac")),
            pending: Vec::new(),
        });
        assert_eq!(h.app.past.front().unwrap().id, 1);
        h.app.apply_event(PlayerEvent::EndOfQueue);
        assert_eq!(h.app.past.back().unwrap().id, 2);
        h.key(KeyCode::Char('c'));
        assert!(h.app.past.is_empty());
        assert!(h.sent.try_recv().is_err(), "c clears only local history");
    }

    #[test]
    fn playlist_block_move_preserves_duplicate_paths_and_cue_ranges() {
        let mut h = harness();
        use sointty_core::CueRange;
        let image = PathBuf::from("/album/image.wav");
        let a = QueueEntry {
            path: image.clone(),
            cue_range: Some(CueRange {
                start_cd: 0,
                end_cd: Some(75),
            }),
        };
        let c = QueueEntry {
            path: image,
            cue_range: Some(CueRange {
                start_cd: 75,
                end_cd: None,
            }),
        };
        let b = QueueEntry::from(PathBuf::from("/b.flac"));
        let d = QueueEntry::from(PathBuf::from("/d.flac"));
        let e = QueueEntry::from(PathBuf::from("/e.flac"));
        h.app.state.catalog.playlists[0].entries =
            vec![a.clone(), b.clone(), c.clone(), d.clone(), e.clone()];
        h.app.screen = Screen::Playlists;
        h.app.focus = Focus::PlaylistTracks;
        h.app.playlist_selected = [0, 2].into();
        h.key(KeyCode::Char('m'));
        for _ in 0..3 {
            h.key(KeyCode::Down);
        }
        h.key(KeyCode::Enter);
        assert_eq!(
            h.app.state.catalog.playlists[0].entries,
            vec![b.clone(), d.clone(), e.clone(), a.clone(), c.clone()]
        );
        assert!(h.app.playlist_selected.is_empty());
        assert_eq!(h.app.playlist_track_sel, 3);
        assert!(h.app.dirty_playlists.contains(&0));
        // Remove one of the duplicate image paths by ROW INDEX, not path.
        h.app.playlist_selected = [3].into();
        h.key(KeyCode::Char('d'));
        assert_eq!(h.app.state.catalog.playlists[0].entries, vec![b, d, e, c]);
        assert_eq!(
            h.app.state.catalog.playlists[0].entries[3]
                .cue_range
                .unwrap()
                .start_cd,
            75
        );
        assert!(
            h.sent.try_recv().is_err(),
            "catalog edits never touch playback"
        );
    }

    #[test]
    fn picker_new_playlist_appends_pending_entry_without_playback() {
        let mut h = harness();
        let entry = QueueEntry::from(PathBuf::from("/music/pick.flac"));
        h.app.mode = Mode::PlaylistPicker(PlaylistPicker {
            entries: vec![entry.clone()],
            selected: 1,
        });
        h.key(KeyCode::Enter);
        assert!(matches!(h.app.mode, Mode::Prompt(_)));
        for ch in "Picked".chars() {
            h.key(KeyCode::Char(ch));
        }
        h.key(KeyCode::Enter);
        assert_eq!(h.app.state.catalog.playlists.len(), 2);
        assert_eq!(h.app.state.catalog.playlists[1].entries, vec![entry]);
        assert!(h.app.dirty_playlists.contains(&1));
        assert!(h.app.catalog_dirty);
        assert!(matches!(h.app.mode, Mode::Normal));
        assert!(h.sent.try_recv().is_err());
    }

    #[test]
    fn dirty_save_failure_retry_and_quit_yes_no_escape() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut h = harness();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_hook = Arc::clone(&attempts);
        h.app.hooks.on_catalog = Box::new(move |_| {
            if attempts_for_hook.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("disk full".to_owned())
            } else {
                Ok(())
            }
        });
        h.app.catalog_create("Chill".to_owned());
        assert!(!h.key(KeyCode::Char('q')));
        h.key(KeyCode::Esc); // cancel quit
        assert!(h.app.has_unsaved_catalog());
        assert!(!h.app.save_catalog());
        assert!(h.app.state.status.contains("disk full"));
        assert!(h.app.has_unsaved_catalog());
        assert!(!h.key(KeyCode::Char('q')));
        assert!(h.key(KeyCode::Char('y')), "retry save succeeds, then exits");
        assert!(!h.app.has_unsaved_catalog());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        let mut no = harness();
        no.app.catalog_create("Unsaved".to_owned());
        assert!(!no.key(KeyCode::Char('q')));
        assert!(no.key(KeyCode::Char('n')), "discard exits without saving");
        assert!(no.saves.lock().is_empty());

        let mut read_only = harness();
        read_only.app.state.catalog_error = Some("/data/playlists.toml: bad toml".to_owned());
        assert!(!read_only.app.catalog_create("Denied".to_owned()));
        assert_eq!(read_only.app.state.catalog.playlists.len(), 1);
        assert!(read_only.app.state.status.contains("read-only"));
        assert!(read_only.saves.lock().is_empty());
    }

    #[test]
    fn same_track_seek_preserves_tags_but_refreshes_verdict_and_duration() {
        use sointty_core::{ChannelLayout, SampleEncoding, StreamSpec};
        let mut h = harness();
        h.app.apply_event(PlayerEvent::QueueChanged {
            audible: Some(item(7, "/music/song.wav")),
            pending: Vec::new(),
        });
        let source = DecodedSpec::Pcm(StreamSpec {
            rate_hz: 44_100,
            layout: ChannelLayout::discrete(2),
            encoding: SampleEncoding::S16,
        });
        let buffers = BufferConfig {
            period_frames: 1_024,
            buffer_frames: 4_096,
            ring_frames: 16_384,
        };
        let output = OutputSpec {
            device: "dev1".to_owned(),
            rate_hz: 44_100,
            layout: ChannelLayout::discrete(2),
            format: DeviceFormat::S16Le,
            valid_bits: 16,
        };
        h.app.apply_event(PlayerEvent::Playing {
            track: 7,
            output: output.clone(),
            source,
            buffers,
            converted: false,
            shared_playback: false,
        });
        h.app.apply_event(PlayerEvent::Tags {
            track: 7,
            tags: TrackTags {
                title: Some("Song".to_owned()),
                artist: Some("Artist".to_owned()),
                album: Some("Album".to_owned()),
            },
        });
        h.app.apply_event(PlayerEvent::Duration {
            track: 7,
            total_frames: Some(441_000),
        });
        assert_eq!(
            player_view(&h.app).badge,
            screens::player::PlaybackBadge::BitPerfect
        );
        h.app.apply_event(PlayerEvent::Reconfiguring);
        assert_eq!(h.app.state.tags.title.as_deref(), Some("Song"));
        assert_eq!(h.app.state.total_frames, None);
        assert_eq!(
            player_view(&h.app).badge,
            screens::player::PlaybackBadge::Idle
        );
        let converted_output = OutputSpec {
            format: DeviceFormat::F32Le,
            valid_bits: 32,
            ..output
        };
        h.app.apply_event(PlayerEvent::Playing {
            track: 7,
            output: converted_output,
            source,
            buffers,
            converted: true,
            shared_playback: false,
        });
        assert_eq!(
            h.app.state.tags.title.as_deref(),
            Some("Song"),
            "same-track seek keeps tags"
        );
        assert_eq!(h.app.state.buffers, Some(buffers));
        assert_eq!(
            player_view(&h.app).badge,
            screens::player::PlaybackBadge::Converted
        );
        h.app.apply_event(PlayerEvent::Duration {
            track: 7,
            total_frames: Some(441_000),
        });
        h.app.apply_event(PlayerEvent::EndOfQueue);
        assert_eq!(
            player_view(&h.app).badge,
            screens::player::PlaybackBadge::Idle
        );
    }

    fn screen_text(app: &App, width: u16, height: u16) -> String {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn playback_keys_row_renders_on_wide_and_medium_not_narrow() {
        let h = harness();
        let wide = screen_text(&h.app, 140, 40);
        for marker in ["play/pause", "stop", "next", "seek -10s", "seek +10s", "shuffle"] {
            assert!(wide.contains(marker), "wide playback row shows {marker}");
        }
        let medium = screen_text(&h.app, 100, 30);
        assert!(
            medium.contains("stop"),
            "medium footer has the playback row"
        );
        let narrow = screen_text(&h.app, 60, 20);
        assert!(
            !narrow.contains("stop"),
            "narrow footer stays a single context row"
        );
    }

    #[test]
    fn shared_playing_badge_is_truthful_and_keeps_selected_direct_device() {
        use sointty_core::{ChannelLayout, SampleEncoding, StreamSpec};
        let mut h = harness();
        h.app.state.device = "hw:2,0".to_owned();
        h.app.state.shared_playback = true;
        // The saved preference alone is not an active-stream verdict.
        assert_eq!(
            player_view(&h.app).badge,
            screens::player::PlaybackBadge::Idle
        );
        h.app.apply_event(PlayerEvent::QueueChanged {
            audible: Some(item(7, "/music/song.wav")),
            pending: Vec::new(),
        });
        let source = DecodedSpec::Pcm(StreamSpec {
            rate_hz: 44_100,
            layout: ChannelLayout::discrete(2),
            encoding: SampleEncoding::S16,
        });
        let buffers = BufferConfig {
            period_frames: 1_024,
            buffer_frames: 4_096,
            ring_frames: 16_384,
        };
        // Linux shared output reports its actual route, `default`, but the
        // selected `hw:` endpoint must remain in the picker for Direct.
        let output = OutputSpec {
            device: "default".to_owned(),
            rate_hz: 44_100,
            layout: ChannelLayout::discrete(2),
            format: DeviceFormat::S16Le,
            valid_bits: 16,
        };
        h.app.apply_event(PlayerEvent::Playing {
            track: 7,
            output: output.clone(),
            source,
            buffers,
            converted: false,
            shared_playback: true,
        });
        assert_eq!(h.app.state.device, "hw:2,0");
        assert_eq!(h.app.state.output_spec.as_ref().unwrap().device, "default");
        assert_eq!(
            player_view(&h.app).badge,
            screens::player::PlaybackBadge::Shared
        );
        assert!(h.app.state.status.contains("shared"));
        assert!(h.app.state.status.contains("not bit-perfect"));
        let rendered = screen_text(&h.app, 140, 40);
        assert!(
            rendered.contains("● shared"),
            "Now playing and header show shared"
        );
        assert!(
            !rendered.contains("● bit-perfect"),
            "shared never gets a bit-perfect badge"
        );

        // Even when an app-side integer→F32 packer also converted, Shared
        // takes badge precedence and the status keeps the direction.
        h.app.apply_event(PlayerEvent::Playing {
            track: 7,
            output: OutputSpec {
                format: DeviceFormat::F32Le,
                valid_bits: 32,
                ..output
            },
            source,
            buffers,
            converted: true,
            shared_playback: true,
        });
        assert_eq!(
            player_view(&h.app).badge,
            screens::player::PlaybackBadge::Shared
        );
        assert!(h.app.state.status.contains("integer→F32 converted"));
        h.app.apply_event(PlayerEvent::Paused);
        assert_eq!(
            player_view(&h.app).badge,
            screens::player::PlaybackBadge::Idle
        );
        h.app.apply_event(PlayerEvent::Reconfiguring);
        assert_eq!(
            player_view(&h.app).badge,
            screens::player::PlaybackBadge::Idle
        );
    }

    #[test]
    fn three_screens_render_their_panels_and_tabs() {
        let mut h = harness();
        let library = screen_text(&h.app, 140, 40);
        for marker in [
            "1 Library",
            "2 Playlists",
            "3 Settings",
            "Library",
            "Queue · 0 tracks",
            "Now playing",
            "nothing playing",
        ] {
            assert!(
                library.contains(marker),
                "library screen missing {marker:?}"
            );
        }
        h.key(KeyCode::Char('2'));
        let playlists = screen_text(&h.app, 140, 40);
        assert!(
            playlists.contains("(empty: p from Library or Search adds tracks)"),
            "playlists screen missing empty-list hint"
        );
        h.key(KeyCode::Char('3'));
        let settings = screen_text(&h.app, 140, 40);
        for marker in ["Settings", "Output", "/config/config.toml"] {
            assert!(
                settings.contains(marker),
                "settings screen missing {marker:?}"
            );
        }
    }

    #[test]
    fn narrow_terminal_shows_small_terminal_hint_and_focused_list_only() {
        let h = harness();
        let text = screen_text(&h.app, 60, 20);
        assert!(
            text.contains("small terminal"),
            "narrow header must explain the reduced layout"
        );
        assert!(
            !text.contains("Now playing"),
            "narrow tier has no side-by-side player panel"
        );
    }

    #[test]
    fn no_color_surface_has_no_rgb_or_indexed_cells_and_keeps_selection() {
        use ratatui::style::Color;
        let root = temp_dir("no-color");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("song.flac"), b"").unwrap();
        let mut h = harness();
        h.app.browser = Some(Browser::open(root.clone()).unwrap());
        h.app.theme = Theme::build(true, true, None).0;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &h.app)).unwrap();
        let cells = &terminal.backend().buffer().content;
        assert!(
            cells
                .iter()
                .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
        );
        assert!(
            cells
                .iter()
                .any(|cell| cell.modifier.contains(Modifier::REVERSED))
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
