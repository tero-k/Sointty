use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use sointty_core::{
    BufferConfig, DeviceId, OutputSpec, PlayerCommand, PlayerEvent, QueueEntry, QueueItem,
    TrackId, TrackTags,
};

mod browser;

pub use browser::collect_audio_files;
use browser::{Browser, BrowserItemKind};

/// Lists selectable output devices as `(id, display name)` pairs; invoked
/// each time the device pane opens so hot-plugged endpoints appear.
pub type DeviceProvider = Box<dyn FnMut() -> Vec<(DeviceId, String)>>;
/// Persists a device selection; the error text is shown in the status line.
pub type DeviceSaver = Box<dyn FnMut(&DeviceId) -> Result<(), String>>;
/// Persists an explicit non-bit-perfect F32-to-integer compatibility choice.
pub type FloatCompatibilitySaver = Box<dyn FnMut(bool) -> Result<(), String>>;
/// Persists the last successfully browsed directory.
pub type BrowserDirSaver = Box<dyn FnMut(&Path) -> Result<(), String>>;
/// Persists the whole playlist catalog after any edit.
pub type PlaylistSaver = Box<dyn FnMut(&PlaylistCatalog) -> Result<(), String>>;
/// Persists DAC timing overrides; `None` clears a field back to Auto.
pub type TimingSaver = Box<dyn FnMut(Option<u32>, Option<u32>) -> Result<(), String>>;

/// A browsable mount root.
#[derive(Debug, Clone)]
pub struct Location {
    pub label: String,
    pub path: PathBuf,
    pub mapped_network: bool,
}

/// Lists available locations; invoked when the Locations pane opens.
pub type LocationProvider = Box<dyn FnMut() -> Result<Vec<Location>, String>>;
/// Maps a drive letter to a UNC share; returns the new root (Windows only).
pub type MapDrive = Box<dyn FnMut(char, &str) -> Result<PathBuf, String>>;
/// Disconnects a mapped network drive letter (Windows only).
pub type DisconnectDrive = Box<dyn FnMut(char) -> Result<(), String>>;
/// Mounts an SMB share through the OS (Unix only).
pub type MountShare = Box<dyn FnMut(&str) -> Result<(), String>>;

/// Seek step for the Left/Right arrow keys.
const SEEK_SECONDS: u64 = 10;

/// A named, persisted playlist: entries never leave the list unless the
/// user edits it; the live playback queue is independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPlaylist {
    pub name: String,
    pub entries: Vec<QueueEntry>,
}

/// All saved playlists plus the selected one. There is always at least one
/// list; the final list cannot be deleted.
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

pub struct TuiState {
    pub status: String,
    pub frame: u64,
    /// Source-frame rate for elapsed time: output rate times the DSD
    /// wire-frame factor.
    pub rate_hz: u32,
    /// Length in source frames of the audible track, when known.
    total_frames: Option<u64>,
    pub paused: bool,
    pub tags: TrackTags,
    current_track: TrackId,
    /// Active output device; updated from `Playing` events and the picker.
    device: DeviceId,
    /// `(id, friendly name)` cache from the device provider; refreshed when
    /// the picker opens. Lets the UI show names instead of endpoint IDs.
    device_names: Vec<(DeviceId, String)>,
    float_to_int: bool,
    /// Whether the last established output used F32-to-integer conversion.
    converted: bool,
    /// Negotiated output for the current stream, when playing.
    output_spec: Option<OutputSpec>,
    /// DAC timing overrides in effect (None = Auto per-field).
    timing: (Option<u32>, Option<u32>),
    /// Live queue from the engine: audible track and pending FIFO order.
    audible: Option<QueueItem>,
    pending: Vec<QueueItem>,
    /// Random playback order for future queue entries (not saved playlists).
    shuffle: bool,
    /// Saved playlists and the selected one.
    catalog: PlaylistCatalog,
    /// When the on-disk catalog is malformed: full path plus parse error.
    /// Edit actions are disabled until the file is repaired.
    catalog_error: Option<String>,
    /// Last successfully browsed directory, kept across pane closes and
    /// (via the saver) across launches.
    last_browser_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
}

impl TuiState {
    fn new(
        device: DeviceId,
        float_to_int: bool,
        timing: (Option<u32>, Option<u32>),
        catalog: PlaylistCatalog,
        catalog_error: Option<String>,
        last_browser_dir: Option<PathBuf>,
        home_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            total_frames: None,
            status: "idle".to_owned(),
            frame: 0,
            rate_hz: 44_100,
            paused: false,
            tags: TrackTags::default(),
            current_track: 0,
            device,
            device_names: Vec::new(),
            float_to_int,
            converted: false,
            output_spec: None,
            timing,
            audible: None,
            pending: Vec::new(),
            shuffle: false,
            catalog,
            catalog_error,
            last_browser_dir,
            home_dir,
        }
    }

    /// True when nothing is playing or paused mid-stream: safe for Play to
    /// start the queue.
    fn idle(&self) -> bool {
        self.audible.is_none() || self.paused
    }

    /// Friendly name for the active device when the provider knows it;
    /// otherwise the raw endpoint ID.
    fn device_display(&self) -> &str {
        self.device_names
            .iter()
            .find(|(id, _)| *id == self.device)
            .map(|(_, name)| name.as_str())
            .unwrap_or(&self.device)
    }
}

/// Selectable output devices shown by the `d` key.
struct DevicePane {
    devices: Vec<(DeviceId, String)>,
    selected: usize,
}

impl DevicePane {
    fn move_selection(&mut self, delta: isize) {
        if self.devices.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.devices.len() - 1);
    }
}

struct LocationsPane {
    locations: Vec<Location>,
    selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaylistFocus {
    Names,
    Entries,
}

struct PlaylistsPane {
    focus: PlaylistFocus,
    name_sel: usize,
    entry_sel: usize,
}

/// DAC settings rows.
struct SettingsPane {
    selected: usize,
}

/// What a submitted Input line means.
enum InputAction {
    /// Absolute path or UNC share typed into Locations.
    LocationPath,
    /// First step of drive mapping: the letter.
    MapDriveLetter,
    /// Second step: the \\server\share for a chosen letter.
    MapDriveShare(char),
    /// SMB share for the Unix/macOS OS mount action.
    MountShare,
    PlaylistCreate,
    PlaylistRename,
    PlaylistImportPath,
    PlaylistImportName(PathBuf),
    TimingPeriod,
    TimingBuffer(u32),
}

struct InputPane {
    prompt: String,
    buffer: String,
    action: InputAction,
}

/// One menu row.
struct MenuItem {
    label: String,
    action: MenuAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MenuAction {
    OpenBrowser,
    OpenLocations,
    OpenPlaylists,
    OpenQueue,
    ViewPlaylists,
    ViewQueue,
    OpenDevices,
    OpenSettings,
    ShowHelp,
    Quit,
    LocationsEnterPath,
    LocationsRefresh,
    LocationsMapDrive,
    LocationsMountShare,
    LocationsDisconnect(char),
    PlaylistCreate,
    PlaylistRename,
    PlaylistDelete,
    PlaylistDeleteConfirmed,
    PlaylistImport,
    PlaylistRemoveEntry,
    PlaylistMoveUp,
    PlaylistMoveDown,
    PlaylistEnqueue,
    PlaylistEnqueueAll,
    QueueEnqueueSelectedPlaylist,
    QueueToggleShuffle,
    Close,
}

struct MenuPane {
    /// Stack of (title, items); the top is shown.
    stack: Vec<(String, Vec<MenuItem>)>,
    selected: usize,
}

impl MenuPane {
    fn top() -> Self {
        Self {
            stack: vec![(
                "Menu".to_owned(),
                vec![
                    MenuItem { label: "Browse".to_owned(), action: MenuAction::OpenBrowser },
                    MenuItem { label: "Locations".to_owned(), action: MenuAction::OpenLocations },
                    MenuItem { label: "Playlists".to_owned(), action: MenuAction::OpenPlaylists },
                    MenuItem { label: "Queue".to_owned(), action: MenuAction::OpenQueue },
                    MenuItem { label: "Output device".to_owned(), action: MenuAction::OpenDevices },
                    MenuItem { label: "DAC settings".to_owned(), action: MenuAction::OpenSettings },
                    MenuItem { label: "Help".to_owned(), action: MenuAction::ShowHelp },
                    MenuItem { label: "Quit".to_owned(), action: MenuAction::Quit },
                ],
            )],
            selected: 0,
        }
    }

    fn push(&mut self, title: String, items: Vec<MenuItem>) {
        self.stack.push((title, items));
        self.selected = 0;
    }

    fn pop(&mut self) -> bool {
        if self.stack.len() > 1 {
            self.stack.pop();
            self.selected = 0;
            true
        } else {
            false
        }
    }

    fn current_items(&self) -> &[MenuItem] {
        &self.stack.last().unwrap().1
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.current_items().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.saturating_add_signed(delta).min(len - 1);
    }

    fn selected_action(&self) -> Option<&MenuAction> {
        self.current_items().get(self.selected).map(|item| &item.action)
    }
}

/// At most one pane is open at a time; opening one replaces the other.
enum Pane {
    Browser(Browser),
    Devices(DevicePane),
    Locations(LocationsPane),
    Menu(MenuPane),
    Playlists(PlaylistsPane),
    Settings(SettingsPane),
    Input(InputPane),
}

/// What the right column shows when a list pane is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RightView {
    SavedList,
    Queue,
}

/// Everything `run` needs that is not a callback.
pub struct RunOptions {
    pub open_browser: bool,
    /// Saved browsing directory from a previous launch.
    pub initial_browser_dir: Option<PathBuf>,
    pub home_dir: Option<PathBuf>,
    pub current_device: DeviceId,
    pub float_to_int: bool,
    /// DAC timing overrides from CLI/config (None = Auto).
    pub timing: (Option<u32>, Option<u32>),
    pub catalog: PlaylistCatalog,
    /// Full path plus parse error when the catalog file is malformed.
    pub catalog_error: Option<String>,
}

/// Callbacks into the app layer; OS-specific actions are `None` where the
/// platform does not support them.
pub struct Hooks {
    pub device_provider: DeviceProvider,
    pub on_device_selected: DeviceSaver,
    pub on_float_to_int_selected: FloatCompatibilitySaver,
    pub on_browser_dir: BrowserDirSaver,
    pub on_catalog: PlaylistSaver,
    pub on_timing: TimingSaver,
    pub locations: LocationProvider,
    pub map_drive: Option<MapDrive>,
    pub disconnect_drive: Option<DisconnectDrive>,
    pub mount_share: Option<MountShare>,
}

/// An action that must run with the terminal suspended (OS prompts).
enum OsAction {
    MapDrive(char, String),
    MountShare(String),
}

struct App {
    state: TuiState,
    pane: Option<Pane>,
    right: RightView,
    playlist_entry_sel: usize,
    /// Keyboard focus on the visible saved playlist in the right column.
    right_focused: bool,
    /// Browser view restored if a drive/location selection is canceled.
    browser_before_locations: Option<Browser>,
    /// Pane to restore after a modal input is canceled or fails.
    input_origin: Option<Pane>,
    commands: Sender<PlayerCommand>,
    hooks: Hooks,
    pending_os: Option<OsAction>,
}

impl App {
    fn new(options: RunOptions, hooks: Hooks, commands: Sender<PlayerCommand>) -> Self {
        let mut app = Self {
            state: TuiState::new(
                options.current_device,
                options.float_to_int,
                options.timing,
                options.catalog,
                options.catalog_error,
                options.initial_browser_dir,
                options.home_dir,
            ),
            pane: None,
            right: RightView::SavedList,
            playlist_entry_sel: 0,
            right_focused: false,
            browser_before_locations: None,
            input_origin: None,
            commands,
            hooks,
            pending_os: None,
        };
        // Prime the device-name cache so the Playback pane shows a friendly
        // name from the start; a failed enumeration only delays the name
        // until the picker is opened.
        app.state.device_names = (app.hooks.device_provider)();
        if options.open_browser {
            app.open_browser_startup();
        }
        app
    }

    /// Startup directory resolution: saved location, then home, then `/` on
    /// Unix or the first readable drive root on Windows. The process working
    /// directory is never used.
    fn open_browser_startup(&mut self) {
        if let Some(saved) = self.state.last_browser_dir.clone() {
            match Browser::open(saved.clone()) {
                Ok(browser) => {
                    self.pane = Some(Pane::Browser(browser));
                    return;
                }
                Err(error) => {
                    self.state.status = format!(
                        "saved folder {} is unreadable ({error}); falling back",
                        saved.display()
                    );
                }
            }
        }
        if let Some(home) = self.state.home_dir.clone()
            && let Ok(browser) = Browser::open(home)
        {
            self.pane = Some(Pane::Browser(browser));
            return;
        }
        #[cfg(not(windows))]
        {
            if let Ok(browser) = Browser::open(PathBuf::from("/")) {
                self.pane = Some(Pane::Browser(browser));
                return;
            }
        }
        #[cfg(windows)]
        for letter in b'A'..=b'Z' {
            let root = PathBuf::from(format!("{}:\\", letter as char));
            if let Ok(browser) = Browser::open(root) {
                self.pane = Some(Pane::Browser(browser));
                return;
            }
        }
        self.state.status =
            "no readable folder found; open Locations (l) and enter a path".to_owned();
    }

    /// Reopen the browser at the last successful directory, falling back to
    /// home. Never uses the process working directory.
    fn reopen_browser(&mut self) {
        let saved = self.state.last_browser_dir.clone();
        let home = self.state.home_dir.clone();
        for candidate in [saved, home].into_iter().flatten() {
            match Browser::open(candidate.clone()) {
                Ok(browser) => {
                    self.pane = Some(Pane::Browser(browser));
                    return;
                }
                Err(error) => {
                    self.state.status =
                        format!("{} is unreadable ({error}); falling back", candidate.display());
                }
            }
        }
        #[cfg(not(windows))]
        {
            if let Ok(browser) = Browser::open(PathBuf::from("/")) {
                self.pane = Some(Pane::Browser(browser));
                return;
            }
        }
        #[cfg(windows)]
        for letter in b'A'..=b'Z' {
            let root = PathBuf::from(format!("{}:\\", letter as char));
            if let Ok(browser) = Browser::open(root) {
                self.pane = Some(Pane::Browser(browser));
                return;
            }
        }
        self.state.status =
            "no readable folder found; open Locations (l) and enter a path".to_owned();
    }

    /// Remember a successfully browsed directory and persist it. A save
    /// failure keeps the navigation in memory and says the next launch
    /// might not restore it.
    fn remember_browser_dir(&mut self, dir: &Path) {
        self.browser_before_locations = None;
        self.state.last_browser_dir = Some(dir.to_path_buf());
        if let Err(error) = (self.hooks.on_browser_dir)(dir) {
            self.state.status =
                format!("folder saved for this session only; next launch may not restore it: {error}");
        }
    }

    fn toggle_browser(&mut self) {
        if matches!(self.pane, Some(Pane::Browser(_))) {
            self.close_pane();
            return;
        }
        self.reopen_browser();
    }

    fn close_pane(&mut self) {
        if matches!(self.pane, Some(Pane::Locations(_)))
            && let Some(browser) = self.browser_before_locations.take()
        {
            self.pane = Some(Pane::Browser(browser));
            return;
        }
        self.browser_before_locations = None;
        if let Some(Pane::Browser(browser)) = &self.pane {
            self.state.last_browser_dir = Some(browser.current_dir.clone());
        }
        self.pane = None;
    }

    fn toggle_devices(&mut self) {
        if matches!(self.pane, Some(Pane::Devices(_))) {
            self.close_pane();
            return;
        }
        let devices = (self.hooks.device_provider)();
        if devices.is_empty() {
            self.state.status = "no output devices found".to_owned();
            return;
        }
        let selected = devices
            .iter()
            .position(|(id, _)| *id == self.state.device)
            .unwrap_or(0);
        self.state.device_names = devices.clone();
        self.pane = Some(Pane::Devices(DevicePane { devices, selected }));
    }

    fn open_locations(&mut self) {
        match (self.hooks.locations)() {
            Ok(locations) => {
                match self.pane.take() {
                    Some(Pane::Browser(browser)) => self.browser_before_locations = Some(browser),
                    Some(Pane::Locations(_)) => {}
                    _ if !matches!(self.input_origin, Some(Pane::Locations(_))) => {
                        self.browser_before_locations = None;
                    }
                    _ => {}
                }
                self.pane = Some(Pane::Locations(LocationsPane { locations, selected: 0 }));
            }
            Err(error) => self.state.status = format!("locations: {error}"),
        }
    }

    fn open_playlists(&mut self) {
        self.pane = Some(Pane::Playlists(PlaylistsPane {
            focus: PlaylistFocus::Names,
            name_sel: self.state.catalog.selected,
            entry_sel: 0,
        }));
    }

    fn open_settings(&mut self) {
        self.pane = Some(Pane::Settings(SettingsPane { selected: 0 }));
    }

    fn toggle_menu(&mut self) {
        if matches!(self.pane, Some(Pane::Menu(_))) {
            self.close_pane();
            return;
        }
        let context = match &self.pane {
            Some(Pane::Locations(_)) => 1,
            Some(Pane::Playlists(pane)) => {
                self.playlist_entry_sel = pane.entry_sel;
                2
            }
            _ => 0,
        };
        self.close_pane();
        self.pane = Some(Pane::Menu(MenuPane::top()));
        match context {
            1 => self.open_locations_menu(),
            2 => self.open_playlist_menu(),
            _ => {}
        }
    }

    fn start_input(&mut self, prompt: &str, action: InputAction) {
        if self.input_origin.is_none() {
            self.input_origin = self.pane.take();
        }
        self.pane = Some(Pane::Input(InputPane {
            prompt: prompt.to_owned(),
            buffer: String::new(),
            action,
        }));
    }

    /// Queue one entry; start playback only when idle.
    fn enqueue_entry(&mut self, entry: QueueEntry, name: &str) {
        let _ = self.commands.send(PlayerCommand::Enqueue(entry));
        if self.state.idle() {
            let _ = self.commands.send(PlayerCommand::Play);
            self.state.paused = false;
        }
        self.state.status = format!("queued {name}");
    }

    /// Queue the highlighted saved list without changing the catalog selection.
    fn enqueue_playlist(&mut self, index: usize) {
        let Some(playlist) = self.state.catalog.playlists.get(index) else {
            return;
        };
        if playlist.entries.is_empty() {
            self.state.status = format!("playlist {} is empty", playlist.name);
            return;
        }
        let count = playlist.entries.len();
        for entry in &playlist.entries {
            let _ = self.commands.send(PlayerCommand::Enqueue(entry.clone()));
        }
        if self.state.idle() {
            let _ = self.commands.send(PlayerCommand::Play);
            self.state.paused = false;
        }
        self.state.status = format!("queued {count} track(s) from {}", playlist.name);
    }

    fn toggle_shuffle(&mut self) {
        let enabled = !self.state.shuffle;
        if self.commands.send(PlayerCommand::SetShuffle(enabled)).is_err() {
            self.state.status = "cannot change queue order: player disconnected".to_owned();
            return;
        }
        self.state.shuffle = enabled;
        self.state.status = if enabled {
            "random play ON (current and prepared next stay in place)".to_owned()
        } else {
            "random play OFF (remaining order kept; new tracks append)".to_owned()
        };
    }

    // ---- catalog editing -------------------------------------------------

    /// True when the catalog may be edited; otherwise explains why not.
    fn catalog_writable(&mut self) -> bool {
        if let Some(error) = &self.state.catalog_error {
            self.state.status = format!("playlists read-only: {error}");
            return false;
        }
        true
    }

    fn edit_catalog(&mut self, edit: impl FnOnce(&mut PlaylistCatalog) -> Result<(), String>) -> bool {
        if !self.catalog_writable() {
            return false;
        }
        let mut catalog = self.state.catalog.clone();
        if let Err(error) = edit(&mut catalog) {
            self.state.status = error;
            return false;
        }
        match (self.hooks.on_catalog)(&catalog) {
            Ok(()) => {
                self.state.catalog = catalog;
                true
            }
            Err(error) => {
                self.state.status = format!("playlist not saved: {error}");
                false
            }
        }
    }

    fn catalog_create(&mut self, name: String) {
        let name = name.trim().to_owned();
        self.edit_catalog(|catalog| {
            if name.is_empty() {
                return Err("playlist name must not be blank".to_owned());
            }
            if catalog.name_taken(&name) {
                return Err(format!("a playlist named \"{name}\" already exists"));
            }
            catalog.playlists.push(SavedPlaylist { name, entries: Vec::new() });
            catalog.selected = catalog.playlists.len() - 1;
            Ok(())
        });
    }

    fn catalog_rename(&mut self, name: String) {
        let name = name.trim().to_owned();
        self.edit_catalog(|catalog| {
            if name.is_empty() {
                return Err("playlist name must not be blank".to_owned());
            }
            let current = catalog.selected;
            if catalog.playlists[current].name.eq_ignore_ascii_case(&name) {
                return Ok(());
            }
            if catalog.name_taken(&name) {
                return Err(format!("a playlist named \"{name}\" already exists"));
            }
            catalog.playlists[current].name = name;
            Ok(())
        });
    }

    fn catalog_delete(&mut self) {
        self.edit_catalog(|catalog| {
            if catalog.playlists.len() <= 1 {
                return Err("cannot delete the final playlist".to_owned());
            }
            catalog.playlists.remove(catalog.selected);
            catalog.selected = catalog.selected.min(catalog.playlists.len() - 1);
            Ok(())
        });
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
        // Resolve a typed relative playlist against the current directory
        // once; persisted entries must be absolute even when tracks are
        // missing, so relaunching from another cwd never changes them.
        let path = match std::path::absolute(&path) {
            Ok(path) => path,
            Err(error) => {
                self.state.status = format!("import path: {error}");
                return;
            }
        };
        let entries = match sointty_playlist::read(&path) {
            Ok(entries) => entries.into_iter().map(|entry| {
                std::path::absolute(&entry.path).map(|path| QueueEntry {
                    path,
                    cue_range: entry.cue_range,
                })
            }).collect::<io::Result<Vec<_>>>(),
            Err(error) => {
                self.state.status = format!("import failed: {error}");
                return;
            }
        };
        let entries = match entries {
            Ok(entries) => entries,
            Err(error) => {
                self.state.status = format!("import path: {error}");
                return;
            }
        };
        if entries.is_empty() {
            self.state.status = "import: playlist contains no tracks".to_owned();
            return;
        }
        let count = entries.len();
        if self.edit_catalog(|catalog| {
            catalog.playlists.push(SavedPlaylist { name: name.clone(), entries });
            catalog.selected = catalog.playlists.len() - 1;
            Ok(())
        }) {
            self.state.status = format!("imported {count} track(s)");
        }
    }

    fn catalog_remove_entry(&mut self, index: usize) {
        self.edit_catalog(|catalog| {
            let entries = &mut catalog.playlists[catalog.selected].entries;
            if index < entries.len() {
                entries.remove(index);
            }
            Ok(())
        });
    }

    fn catalog_move_entry(&mut self, index: usize, delta: isize) {
        self.edit_catalog(|catalog| {
            let entries = &mut catalog.playlists[catalog.selected].entries;
            let Some(target) = index.checked_add_signed(delta) else {
                return Ok(());
            };
            if index < entries.len() && target < entries.len() {
                entries.swap(index, target);
            }
            Ok(())
        });
    }

    fn catalog_select(&mut self, index: usize) {
        self.edit_catalog(|catalog| {
            if index >= catalog.playlists.len() {
                return Err("playlist selection is out of range".to_owned());
            }
            catalog.selected = index;
            Ok(())
        });
    }

    /// Append paths to the selected saved list. No playback effect.
    fn catalog_add(&mut self, entries: Vec<QueueEntry>, what: &str) {
        if entries.is_empty() {
            self.state.status = format!("no audio files found in {what}");
            return;
        }
        let count = entries.len();
        if self.edit_catalog(|catalog| {
            catalog.playlists[catalog.selected].entries.extend(entries);
            Ok(())
        }) {
            let name = &self.state.catalog.selected_playlist().name;
            self.state.status = format!("added {count} track(s) to {name}");
        }
    }

    // ---- browser add actions ----------------------------------------------

    /// `a`: add the selected browser item to the selected saved list.
    fn browser_add_selected(&mut self) {
        let Some(Pane::Browser(browser)) = &self.pane else {
            return;
        };
        let Some(item) = browser.selected_item() else {
            return;
        };
        let item = item.clone();
        match item.kind {
            BrowserItemKind::Audio => {
                self.catalog_add(vec![QueueEntry::from(item.path)], &item.name);
            }
            BrowserItemKind::Playlist => match sointty_playlist::read(&item.path) {
                Ok(entries) => {
                    let entries = entries
                        .into_iter()
                        .map(|entry| QueueEntry {
                            path: entry.path,
                            cue_range: entry.cue_range,
                        })
                        .collect::<Vec<_>>();
                    self.catalog_add(entries, &item.name);
                }
                Err(error) => self.state.status = format!("sointty: {error}"),
            },
            BrowserItemKind::Dir => match collect_audio_files(&item.path) {
                Ok(paths) => {
                    let entries = paths.into_iter().map(QueueEntry::from).collect();
                    self.catalog_add(entries, &item.name);
                }
                Err(error) => {
                    self.state.status = format!("nothing added: {}: {error}", item.name)
                }
            },
        }
    }

    /// `A`: add the current browser folder recursively.
    fn browser_add_current_folder(&mut self) {
        let Some(Pane::Browser(browser)) = &self.pane else {
            return;
        };
        let dir = browser.current_dir.clone();
        match collect_audio_files(&dir) {
            Ok(paths) => {
                let entries = paths.into_iter().map(QueueEntry::from).collect();
                self.catalog_add(entries, &dir.display().to_string());
            }
            Err(error) => {
                self.state.status = format!("nothing added: {}: {error}", dir.display())
            }
        }
    }

    // ---- timing ------------------------------------------------------------

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
        self.state.timing = (period, buffer);
        let _ = self
            .commands
            .send(PlayerCommand::SetTiming { period_frames: period, buffer_frames: buffer });
        self.state.status = match (period, buffer) {
            (None, None) => "DAC timing: Auto (per-track rate)".to_owned(),
            _ => "DAC timing: Custom (applies on next track/seek)".to_owned(),
        };
    }

    // ---- input submission ---------------------------------------------------

    fn submit_input(&mut self, input: InputPane) {
        let text = input.buffer.trim().to_owned();
        match input.action {
            InputAction::LocationPath => {
                if text.is_empty() {
                    return;
                }
                match Browser::open(PathBuf::from(&text)) {
                    Ok(browser) => {
                        let dir = browser.current_dir.clone();
                        self.pane = Some(Pane::Browser(browser));
                        self.remember_browser_dir(&dir);
                    }
                    Err(error) => {
                        self.state.status = format!("cannot open {text}: {error}");
                    }
                }
            }
            InputAction::MapDriveLetter => {
                let letter = match text.chars().next() {
                    Some(letter) if text.len() == 1 => letter,
                    _ => {
                        self.state.status = "enter a single drive letter".to_owned();
                        return;
                    }
                };
                self.start_input("Share (\\\\server\\share):", InputAction::MapDriveShare(letter));
            }
            InputAction::MapDriveShare(letter) => {
                if text.is_empty() {
                    return;
                }
                self.pending_os = Some(OsAction::MapDrive(letter, text));
            }
            InputAction::MountShare => {
                if text.is_empty() {
                    return;
                }
                self.pending_os = Some(OsAction::MountShare(text));
            }
            InputAction::PlaylistCreate => self.catalog_create(text),
            InputAction::PlaylistRename => self.catalog_rename(text),
            InputAction::PlaylistImportPath => {
                if text.is_empty() {
                    return;
                }
                self.start_input(
                    "Import as (playlist name):",
                    InputAction::PlaylistImportName(PathBuf::from(text)),
                );
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
                self.start_input("Buffer frames:", InputAction::TimingBuffer(period));
            }
            InputAction::TimingBuffer(period) => {
                let Ok(buffer) = text.parse::<u32>() else {
                    self.state.status = "buffer must be a positive decimal".to_owned();
                    return;
                };
                if buffer == 0 {
                    self.state.status = "buffer must be positive".to_owned();
                    return;
                }
                self.set_timing(Some(period), Some(buffer));
            }
        }
    }

    // ---- menu dispatch -------------------------------------------------------

    fn open_locations_menu(&mut self) {
        let mut items = vec![
            MenuItem { label: "Open locations".to_owned(), action: MenuAction::LocationsRefresh },
            MenuItem { label: "Enter path...".to_owned(), action: MenuAction::LocationsEnterPath },
            MenuItem { label: "Refresh".to_owned(), action: MenuAction::LocationsRefresh },
        ];
        if self.hooks.map_drive.is_some() {
            items.push(MenuItem { label: "Map drive...".to_owned(), action: MenuAction::LocationsMapDrive });
            let mapped: Vec<char> = match (self.hooks.locations)() {
                Ok(locations) => locations
                    .iter()
                    .filter(|location| location.mapped_network)
                    .filter_map(|location| location.path.to_string_lossy().chars().next())
                    .collect(),
                Err(_) => Vec::new(),
            };
            for letter in mapped {
                items.push(MenuItem {
                    label: format!("Disconnect {letter}:"),
                    action: MenuAction::LocationsDisconnect(letter),
                });
            }
        }
        if self.hooks.mount_share.is_some() {
            items.push(MenuItem { label: "Mount share...".to_owned(), action: MenuAction::LocationsMountShare });
        }
        let title = "Locations".to_owned();
        match &mut self.pane {
            Some(Pane::Menu(menu)) => menu.push(title, items),
            _ => {
                let mut menu = MenuPane::top();
                menu.push(title, items);
                self.pane = Some(Pane::Menu(menu));
            }
        }
    }

    fn open_playlist_menu(&mut self) {
        let items = vec![
            MenuItem { label: "View playlists".to_owned(), action: MenuAction::ViewPlaylists },
            MenuItem { label: "Create...".to_owned(), action: MenuAction::PlaylistCreate },
            MenuItem { label: "Rename...".to_owned(), action: MenuAction::PlaylistRename },
            MenuItem { label: "Delete".to_owned(), action: MenuAction::PlaylistDelete },
            MenuItem { label: "Import file...".to_owned(), action: MenuAction::PlaylistImport },
            MenuItem { label: "Remove entry".to_owned(), action: MenuAction::PlaylistRemoveEntry },
            MenuItem { label: "Move entry up".to_owned(), action: MenuAction::PlaylistMoveUp },
            MenuItem { label: "Move entry down".to_owned(), action: MenuAction::PlaylistMoveDown },
            MenuItem { label: "Enqueue selected".to_owned(), action: MenuAction::PlaylistEnqueue },
            MenuItem { label: "Enqueue all".to_owned(), action: MenuAction::PlaylistEnqueueAll },
        ];
        let title = "Playlists".to_owned();
        match &mut self.pane {
            Some(Pane::Menu(menu)) => menu.push(title, items),
            _ => {
                let mut menu = MenuPane::top();
                menu.push(title, items);
                self.pane = Some(Pane::Menu(menu));
            }
        }
    }

    fn open_queue_menu(&mut self) {
        let items = vec![
            MenuItem {
                label: "Enqueue selected playlist".to_owned(),
                action: MenuAction::QueueEnqueueSelectedPlaylist,
            },
            MenuItem { label: "View queue".to_owned(), action: MenuAction::ViewQueue },
            MenuItem {
                label: format!("Random play: {} (toggle)", if self.state.shuffle { "ON" } else { "OFF" }),
                action: MenuAction::QueueToggleShuffle,
            },
        ];
        let title = "Queue".to_owned();
        match &mut self.pane {
            Some(Pane::Menu(menu)) => menu.push(title, items),
            _ => {
                let mut menu = MenuPane::top();
                menu.push(title, items);
                self.pane = Some(Pane::Menu(menu));
            }
        }
    }

    fn show_help(&mut self) {
        let lines = [
            "space play/pause | s stop | n next | <-/-> seek 10 s",
            "b browser | l drives/locations | p playlists | m menu",
            "p lists: Enter select list, Tab entries, a queue list/song",
            "f focus visible playlist | a/Enter song | A whole list",
            "r random queue ON/OFF | Tab queue/playlist | d device",
            "browser: a add item | A add folder | Backspace up/drives",
            "c F32->int (NOT bit-perfect) | ? help | Esc back | q quit",
            "Ctrl+Q quits even while typing in an input prompt.",
            "",
            "Fidelity: playback is always the exact native format; nothing is",
            "resampled, remixed or converted unless F32->integer compatibility",
            "is explicitly enabled (it is OFF by default and clearly labeled).",
        ];
        let items = lines
            .iter()
            .map(|line| MenuItem { label: line.to_string(), action: MenuAction::Close })
            .collect();
        let title = "Help".to_owned();
        match &mut self.pane {
            Some(Pane::Menu(menu)) => menu.push(title, items),
            _ => {
                let mut menu = MenuPane::top();
                menu.push(title, items);
                self.pane = Some(Pane::Menu(menu));
            }
        }
    }

    fn dispatch(&mut self, action: MenuAction) -> bool {
        match action {
            MenuAction::OpenBrowser => self.toggle_browser(),
            MenuAction::OpenLocations => self.open_locations_menu(),
            MenuAction::OpenPlaylists => self.open_playlist_menu(),
            MenuAction::OpenQueue => self.open_queue_menu(),
            MenuAction::ViewPlaylists => self.open_playlists(),
            MenuAction::ViewQueue => {
                self.right = RightView::Queue;
                self.close_pane();
            }
            MenuAction::QueueToggleShuffle => {
                self.toggle_shuffle();
                self.close_pane();
            }
            MenuAction::OpenDevices => self.toggle_devices(),
            MenuAction::OpenSettings => self.open_settings(),
            MenuAction::ShowHelp => self.show_help(),
            MenuAction::Quit => return true,
            MenuAction::LocationsEnterPath => {
                self.start_input("Path or \\\\server\\share:", InputAction::LocationPath);
            }
            MenuAction::LocationsRefresh => self.open_locations(),
            MenuAction::LocationsMapDrive => {
                self.start_input("Drive letter (free, e.g. Z):", InputAction::MapDriveLetter);
            }
            MenuAction::LocationsMountShare => {
                self.start_input("SMB share (server/share):", InputAction::MountShare);
            }
            MenuAction::LocationsDisconnect(letter) => {
                if let Some(disconnect) = &mut self.hooks.disconnect_drive {
                    match disconnect(letter) {
                        Ok(()) => {
                            self.state.status = format!("disconnected {letter}:");
                            // If the browser root was on that drive, reset to home.
                            let on_drive = matches!(
                                &self.pane,
                                Some(Pane::Browser(browser))
                                    if browser
                                        .current_dir
                                        .to_string_lossy()
                                        .to_uppercase()
                                        .starts_with(&format!("{letter}:"))
                            );
                            let last_on_drive = self
                                .state
                                .last_browser_dir
                                .as_ref()
                                .is_some_and(|dir| {
                                    dir.to_string_lossy()
                                        .to_uppercase()
                                        .starts_with(&format!("{letter}:"))
                                });
                            if on_drive || last_on_drive {
                                self.state.last_browser_dir = self.state.home_dir.clone();
                                if on_drive {
                                    self.pane = None;
                                    self.reopen_browser();
                                }
                            }
                            self.close_pane();
                        }
                        Err(error) => self.state.status = error,
                    }
                }
            }
            MenuAction::PlaylistCreate => {
                self.start_input("New playlist name:", InputAction::PlaylistCreate);
            }
            MenuAction::PlaylistRename => {
                self.start_input("Rename playlist to:", InputAction::PlaylistRename);
            }
            MenuAction::PlaylistDelete => {
                let name = self.state.catalog.selected_playlist().name.clone();
                let title = format!("Delete \"{name}\"?");
                let items = vec![
                    MenuItem { label: "Cancel".to_owned(), action: MenuAction::Close },
                    MenuItem {
                        label: format!("Delete \"{name}\""),
                        action: MenuAction::PlaylistDeleteConfirmed,
                    },
                ];
                if let Some(Pane::Menu(menu)) = &mut self.pane {
                    menu.push(title, items);
                }
            }
            MenuAction::PlaylistDeleteConfirmed => self.catalog_delete(),
            MenuAction::PlaylistImport => {
                self.start_input("Playlist file (.m3u/.m3u8/.pls/.xspf/.cue):", InputAction::PlaylistImportPath);
            }
            MenuAction::PlaylistRemoveEntry => {
                let index = self.playlist_entry_sel;
                if index < self.state.catalog.selected_playlist().entries.len() {
                    self.catalog_remove_entry(index);
                    self.playlist_entry_sel = index.min(
                        self.state.catalog.selected_playlist().entries.len().saturating_sub(1),
                    );
                } else {
                    self.state.status = "select a playlist entry first".to_owned();
                }
            }
            MenuAction::PlaylistMoveUp | MenuAction::PlaylistMoveDown => {
                let delta = if action == MenuAction::PlaylistMoveUp { -1 } else { 1 };
                let index = self.playlist_entry_sel;
                let count = self.state.catalog.selected_playlist().entries.len();
                if let Some(target) = index.checked_add_signed(delta)
                    && index < count && target < count
                {
                    self.catalog_move_entry(index, delta);
                    self.playlist_entry_sel = target;
                }
            }
            MenuAction::PlaylistEnqueue => {
                let playlist = self.state.catalog.selected_playlist();
                if let Some(entry) = playlist.entries.get(self.playlist_entry_sel).cloned() {
                    self.enqueue_entry(entry, "track");
                } else {
                    self.state.status = "select a playlist entry first".to_owned();
                }
            }
            MenuAction::PlaylistEnqueueAll | MenuAction::QueueEnqueueSelectedPlaylist => {
                self.enqueue_playlist(self.state.catalog.selected);
            }
            MenuAction::Close => {
                if let Some(Pane::Menu(menu)) = &mut self.pane
                    && !menu.pop()
                {
                    self.close_pane();
                }
            }
        }
        false
    }

    /// Run a queued OS action. In the real event loop the terminal is
    /// suspended around it; tests call this directly.
    fn run_os_action(&mut self, action: OsAction) {
        match action {
            OsAction::MapDrive(letter, share) => {
                if let Some(map_drive) = &mut self.hooks.map_drive {
                    match map_drive(letter, &share) {
                        Ok(root) => {
                            self.state.status = format!("mapped {letter}: to {share}");
                            match Browser::open(root) {
                                Ok(browser) => {
                                    let dir = browser.current_dir.clone();
                                    self.pane = Some(Pane::Browser(browser));
                                    self.remember_browser_dir(&dir);
                                }
                                Err(error) => {
                                    self.state.status = format!("mapped but cannot browse: {error}");
                                }
                            }
                        }
                        Err(error) => self.state.status = error,
                    }
                } else {
                    self.state.status = "drive mapping is unavailable on this OS".to_owned();
                }
            }
            OsAction::MountShare(share) => {
                if let Some(mount_share) = &mut self.hooks.mount_share {
                    match mount_share(&share) {
                        Ok(()) => {
                            self.state.status =
                                "mount requested; refresh Locations to browse the new mount".to_owned();
                            self.open_locations();
                        }
                        Err(error) => self.state.status = error,
                    }
                } else {
                    self.state.status = "share mounting is unavailable on this OS".to_owned();
                }
            }
        }
        if self.pane.is_none() {
            self.pane = self.input_origin.take();
        } else {
            self.input_origin = None;
        }
    }

    // ---- input ---------------------------------------------------------------

    fn handle_key_event(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('q' | 'Q'))
        {
            return true;
        }
        self.handle_key(key.code)
    }

    fn handle_key(&mut self, key: KeyCode) -> bool {
        // Input owns the keyboard: letters like q/s/n/space are text there.
        if matches!(self.pane, Some(Pane::Input(_))) {
            self.handle_input_key(key);
            return false;
        }
        if self.right_focused {
            match key {
                KeyCode::Esc | KeyCode::Char('f') => {
                    self.right_focused = false;
                    return false;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.playlist_entry_sel = self.playlist_entry_sel.saturating_sub(1);
                    return false;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let count = self.state.catalog.selected_playlist().entries.len();
                    if count > 0 {
                        self.playlist_entry_sel = (self.playlist_entry_sel + 1).min(count - 1);
                    }
                    return false;
                }
                KeyCode::Enter => {
                    let entries = &self.state.catalog.selected_playlist().entries;
                    self.playlist_entry_sel = self.playlist_entry_sel.min(entries.len().saturating_sub(1));
                    if let Some(entry) = entries.get(self.playlist_entry_sel).cloned() {
                        self.enqueue_entry(entry, "track");
                    }
                    return false;
                }
                KeyCode::Char('a') => {
                    let entry = self.state.catalog.selected_playlist().entries
                        .get(self.playlist_entry_sel).cloned();
                    if let Some(entry) = entry {
                        self.enqueue_entry(entry, "track");
                    } else {
                        self.state.status = "select a playlist entry first".to_owned();
                    }
                    return false;
                }
                KeyCode::Char('A') => {
                    self.enqueue_playlist(self.state.catalog.selected);
                    return false;
                }
                _ => {}
            }
        }
        if matches!(key, KeyCode::Char('b' | 'd' | 'l' | 'm' | 'p')) {
            self.right_focused = false;
        }
        match key {
            KeyCode::Char('q') => return true,
            KeyCode::Char('?') => {
                self.show_help();
                return false;
            }
            KeyCode::Esc => {
                // Esc closes an open pane first; with no pane it quits.
                if self.pane.is_none() {
                    return true;
                }
                if let Some(Pane::Menu(menu)) = &mut self.pane
                    && menu.pop()
                {
                    return false;
                }
                self.close_pane();
                return false;
            }
            KeyCode::Char(' ') => {
                let command = if self.state.paused {
                    PlayerCommand::Play
                } else {
                    PlayerCommand::Pause
                };
                self.state.paused = !self.state.paused;
                let _ = self.commands.send(command);
                return false;
            }
            KeyCode::Char('s') => {
                self.state.paused = true;
                let _ = self.commands.send(PlayerCommand::Stop);
                return false;
            }
            KeyCode::Char('r') => {
                self.toggle_shuffle();
                return false;
            }
            KeyCode::Char('n') => {
                let _ = self.commands.send(PlayerCommand::Next);
                return false;
            }
            KeyCode::Left => {
                self.seek_by(-(SEEK_SECONDS as i64));
                return false;
            }
            KeyCode::Right => {
                self.seek_by(SEEK_SECONDS as i64);
                return false;
            }
            KeyCode::Char('b') => {
                self.toggle_browser();
                return false;
            }
            KeyCode::Char('d') => {
                self.toggle_devices();
                return false;
            }
            KeyCode::Char('l') => {
                if matches!(self.pane, Some(Pane::Locations(_))) {
                    self.close_pane();
                } else {
                    self.open_locations();
                }
                return false;
            }
            KeyCode::Char('m') => {
                self.toggle_menu();
                return false;
            }
            KeyCode::Char('p') => {
                if matches!(self.pane, Some(Pane::Playlists(_))) {
                    self.close_pane();
                } else {
                    self.open_playlists();
                }
                return false;
            }
            KeyCode::Char('f') => {
                if let Some(Pane::Playlists(pane)) = &mut self.pane {
                    pane.focus = PlaylistFocus::Entries;
                    return false;
                }
                self.right = RightView::SavedList;
                self.right_focused = true;
                let count = self.state.catalog.selected_playlist().entries.len();
                self.playlist_entry_sel = self.playlist_entry_sel.min(count.saturating_sub(1));
                return false;
            }
            KeyCode::Tab => {
                if let Some(Pane::Playlists(pane)) = &mut self.pane {
                    pane.focus = match pane.focus {
                        PlaylistFocus::Names => PlaylistFocus::Entries,
                        PlaylistFocus::Entries => PlaylistFocus::Names,
                    };
                } else {
                    self.right = match self.right {
                        RightView::SavedList => RightView::Queue,
                        RightView::Queue => RightView::SavedList,
                    };
                }
                self.right_focused = false;
                return false;
            }
            KeyCode::Char('a' | 'A') if matches!(self.pane, Some(Pane::Playlists(_))) => {
                let (name, entry, focus) = match &self.pane {
                    Some(Pane::Playlists(pane)) => (pane.name_sel, pane.entry_sel, pane.focus),
                    _ => unreachable!(),
                };
                if key == KeyCode::Char('A') || focus == PlaylistFocus::Names {
                    self.enqueue_playlist(name);
                } else if let Some(track) = self.state.catalog.playlists
                    .get(name).and_then(|list| list.entries.get(entry)).cloned()
                {
                    self.enqueue_entry(track, "track");
                } else {
                    self.state.status = "select a playlist entry first".to_owned();
                }
                return false;
            }
            KeyCode::Char('a') if matches!(self.pane, Some(Pane::Browser(_))) => {
                self.browser_add_selected();
                return false;
            }
            KeyCode::Char('A') if matches!(self.pane, Some(Pane::Browser(_))) => {
                self.browser_add_current_folder();
                return false;
            }
            KeyCode::Char('c') => {
                let enabled = !self.state.float_to_int;
                let _ = self.commands.send(PlayerCommand::SetFloatToInt(enabled));
                self.state.float_to_int = enabled;
                if let Err(error) = (self.hooks.on_float_to_int_selected)(enabled) {
                    self.state.status = format!("F32 compatibility not saved: {error}");
                }
                return false;
            }
            _ => {}
        }
        // Selecting a saved list is persisted before committing, and never
        // changes the independent live queue.
        if key == KeyCode::Enter {
            let selection = match &self.pane {
                Some(Pane::Playlists(pane)) => Some((pane.name_sel, pane.entry_sel, pane.focus)),
                _ => None,
            };
            if let Some((name, entry, focus)) = selection {
                match focus {
                    PlaylistFocus::Names => {
                        self.catalog_select(name);
                        if let Some(Pane::Playlists(pane)) = &mut self.pane {
                            pane.entry_sel = 0;
                        }
                        self.playlist_entry_sel = 0;
                    }
                    PlaylistFocus::Entries => {
                        if let Some(track) = self.state.catalog.playlists
                            .get(name).and_then(|list| list.entries.get(entry)).cloned()
                        {
                            self.enqueue_entry(track, "track");
                        }
                    }
                }
                return false;
            }
        }
        let Some(pane) = &mut self.pane else {
            return false;
        };
        match pane {
            Pane::Browser(browser) => match key {
                KeyCode::Up | KeyCode::Char('k') => browser.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => browser.move_selection(1),
                KeyCode::Backspace => {
                    let at_root = browser.current_dir.parent()
                        .is_none_or(|parent| parent == browser.current_dir);
                    if at_root {
                        self.open_locations();
                    } else if browser.ascend().is_ok() {
                        let dir = browser.current_dir.clone();
                        self.remember_browser_dir(&dir);
                    } else {
                        self.state.status = "cannot go up".to_owned();
                    }
                }
                KeyCode::Enter => {
                    if let Some(item) = browser.selected_item() {
                        let item = item.clone();
                        match item.kind {
                            BrowserItemKind::Dir => {
                                if browser.descend(item.path.clone()).is_ok() {
                                    let dir = browser.current_dir.clone();
                                    self.remember_browser_dir(&dir);
                                } else {
                                    self.state.status =
                                        format!("cannot open {}", item.name);
                                }
                            }
                            BrowserItemKind::Audio => {
                                self.enqueue_entry(QueueEntry::from(item.path), &item.name);
                            }
                            BrowserItemKind::Playlist => {
                                match sointty_playlist::read(&item.path) {
                                    Ok(entries) => {
                                        let count = entries.len();
                                        for entry in entries {
                                            let _ = self.commands.send(PlayerCommand::Enqueue(
                                                QueueEntry {
                                                    path: entry.path,
                                                    cue_range: entry.cue_range,
                                                },
                                            ));
                                        }
                                        let _ = self.commands.send(PlayerCommand::Play);
                                        self.state.paused = false;
                                        self.state.status = format!("queued {count} track(s)");
                                    }
                                    Err(error) => {
                                        self.state.status = format!("sointty: {error}");
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            },
            Pane::Devices(devices) => match key {
                KeyCode::Up | KeyCode::Char('k') => devices.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => devices.move_selection(1),
                KeyCode::Enter => {
                    if let Some((id, name)) = devices.devices.get(devices.selected).cloned() {
                        let _ = self.commands.send(PlayerCommand::SelectDevice(id.clone()));
                        match (self.hooks.on_device_selected)(&id) {
                            Ok(()) => self.state.status = format!("output device: {name}"),
                            Err(error) => {
                                self.state.status =
                                    format!("device selected but not saved: {error}");
                            }
                        }
                        self.state.device = id;
                        self.pane = None;
                    }
                }
                _ => {}
            },
            Pane::Locations(locations) => match key {
                KeyCode::Up | KeyCode::Char('k') => {
                    locations.selected = locations.selected.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !locations.locations.is_empty() {
                        locations.selected =
                            (locations.selected + 1).min(locations.locations.len() - 1);
                    }
                }
                KeyCode::Enter => {
                    if let Some(location) = locations.locations.get(locations.selected) {
                        let path = location.path.clone();
                        match Browser::open(path) {
                            Ok(browser) => {
                                let dir = browser.current_dir.clone();
                                self.pane = Some(Pane::Browser(browser));
                                self.remember_browser_dir(&dir);
                            }
                            Err(error) => {
                                self.state.status = format!("cannot open: {error}");
                            }
                        }
                    }
                }
                _ => {}
            },
            Pane::Menu(menu) => match key {
                KeyCode::Up | KeyCode::Char('k') => menu.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => menu.move_selection(1),
                KeyCode::Enter => {
                    if let Some(action) = menu.selected_action().cloned() {
                        return self.dispatch(action);
                    }
                }
                _ => {}
            },
            Pane::Playlists(pane) => {
                let name_count = self.state.catalog.playlists.len();
                let entry_count = self
                    .state
                    .catalog
                    .playlists
                    .get(pane.name_sel)
                    .map(|playlist| playlist.entries.len())
                    .unwrap_or(0);
                match key {
                    KeyCode::Up | KeyCode::Char('k') => match pane.focus {
                        PlaylistFocus::Names => pane.name_sel = pane.name_sel.saturating_sub(1),
                        PlaylistFocus::Entries => {
                            pane.entry_sel = pane.entry_sel.saturating_sub(1)
                        }
                    },
                    KeyCode::Down | KeyCode::Char('j') => match pane.focus {
                        PlaylistFocus::Names => {
                            if name_count > 0 {
                                pane.name_sel = (pane.name_sel + 1).min(name_count - 1);
                            }
                        }
                        PlaylistFocus::Entries => {
                            if entry_count > 0 {
                                pane.entry_sel = (pane.entry_sel + 1).min(entry_count - 1);
                            }
                        }
                    },
                    KeyCode::Enter => {}
                    _ => {}
                }
                self.playlist_entry_sel = pane.entry_sel;
            }
            Pane::Settings(settings) => match key {
                KeyCode::Up | KeyCode::Char('k') => {
                    settings.selected = settings.selected.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    settings.selected = (settings.selected + 1).min(3);
                }
                KeyCode::Enter => match settings.selected {
                    0 => self.toggle_devices(),
                    1 => self.set_timing(None, None),
                    2 => self.start_input("Period frames:", InputAction::TimingPeriod),
                    3 => {
                        let enabled = !self.state.float_to_int;
                        let _ = self.commands.send(PlayerCommand::SetFloatToInt(enabled));
                        self.state.float_to_int = enabled;
                        if let Err(error) = (self.hooks.on_float_to_int_selected)(enabled) {
                            self.state.status =
                                format!("F32 compatibility not saved: {error}");
                        }
                    }
                    _ => {}
                },
                _ => {}
            },
            Pane::Input(_) => {}
        }
        false
    }

    fn handle_input_key(&mut self, key: KeyCode) {
        let Some(Pane::Input(input)) = &mut self.pane else {
            return;
        };
        match key {
            KeyCode::Esc => {
                self.pane = self.input_origin.take();
            }
            KeyCode::Enter => {
                let Some(Pane::Input(input)) = self.pane.take() else {
                    return;
                };
                self.submit_input(input);
                if self.pending_os.is_none() {
                    if self.pane.is_none() {
                        self.pane = self.input_origin.take();
                    } else if !matches!(self.pane, Some(Pane::Input(_))) {
                        self.input_origin = None;
                    }
                }
            }
            KeyCode::Backspace => {
                input.buffer.pop();
            }
            KeyCode::Char(ch) => input.buffer.push(ch),
            _ => {}
        }
    }

    fn seek_by(&self, seconds: i64) {
        let target = self.state.frame as i64 + seconds * i64::from(self.state.rate_hz);
        let _ = self.commands.send(PlayerCommand::SeekFrame(target.max(0) as u64));
    }

    fn apply_event(&mut self, event: PlayerEvent) {
        let state = &mut self.state;
        match event {
            PlayerEvent::QueueChanged { audible, pending } => {
                state.audible = audible;
                state.pending = pending;
            }
            PlayerEvent::Playing { track, output, converted } => {
                state.status = if converted {
                    "playing (F32 converted; not bit-perfect)".to_owned()
                } else {
                    "playing (bit-perfect)".to_owned()
                };
                state.paused = false;
                state.tags = TrackTags::default();
                state.current_track = track;
                state.frame = 0;
                state.total_frames = None;
                // Elapsed time counts source frames; DSD wire frames pack
                // several source frames, so scale the rate accordingly.
                state.rate_hz =
                    output.rate_hz * output.format.source_frames_per_wire_frame() as u32;
                state.converted = converted;
                state.device = output.device.clone();
                state.output_spec = Some(output);
            }
            PlayerEvent::Tags { track, tags } => {
                if track == state.current_track {
                    state.tags = tags;
                }
            }
            PlayerEvent::Duration { track, total_frames } => {
                if track == state.current_track {
                    state.total_frames = total_frames;
                }
            }
            PlayerEvent::Position { track, frame } => {
                if track == state.current_track {
                    state.frame = frame;
                }
            }
            PlayerEvent::Paused => {
                state.status = "paused".to_owned();
                state.paused = true;
            }
            PlayerEvent::Reconfiguring => {
                state.status = "reconfiguring device".to_owned();
                state.tags = TrackTags::default();
                state.output_spec = None;
                state.converted = false;
            }
            PlayerEvent::Underrun { .. } => state.status = "underrun".to_owned(),
            PlayerEvent::Stalled { .. } => state.status = "stalled".to_owned(),
            PlayerEvent::Error { kind, .. } => {
                state.status = format!("error: {kind}");
                // No stream is playing after a command-path failure. Space
                // must start the next queued track, not send another Pause.
                state.total_frames = None;
                state.paused = true;
            }
            PlayerEvent::EndOfQueue => {
                state.status = "end of queue".to_owned();
                state.paused = false;
                state.total_frames = None;
                state.frame = 0;
            }
        }
    }
}

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

/// Suspend the alternate screen and raw mode for an interactive OS prompt;
/// `resume_terminal` must run afterwards even on error.
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
    loop {
        while let Ok(event) = events.try_recv() {
            app.apply_event(event);
            dirty = true;
        }
        if dirty {
            terminal.draw(|frame| draw(frame, &app))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if app.handle_key_event(key) {
                return Ok(());
            }
            if let Some(action) = app.pending_os.take() {
                // The OS credential/mount prompt needs a normal terminal.
                // Always restore, even when the action fails.
                suspend_terminal(terminal)?;
                app.run_os_action(action);
                resume_terminal(terminal)?;
            }
            dirty = true;
        }
    }
}

fn metadata_line(tags: &TrackTags) -> String {
    let mut line = match (&tags.artist, &tags.title) {
        (Some(artist), Some(title)) => format!("{artist} - {title}"),
        (None, Some(title)) => title.clone(),
        (Some(artist), None) => artist.clone(),
        (None, None) => String::new(),
    };
    if let Some(album) = &tags.album {
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&format!("({album})"));
    }
    line
}

fn queue_entry_name(entry: &QueueEntry) -> String {
    let mut name = entry
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.path.display().to_string());
    if let Some(range) = entry.cue_range {
        name.push_str(&match range.end_cd {
            Some(end) => format!(" [cue {}:{}]", range.start_cd, end),
            None => format!(" [cue {}:end]", range.start_cd),
        });
    }
    name
}

fn clock(seconds: u64) -> String {
    if seconds >= 3_600 {
        format!("{}:{:02}:{:02}", seconds / 3_600, (seconds / 60) % 60, seconds % 60)
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

fn progress_bar(frame: u64, total: Option<u64>, width: usize) -> String {
    let filled = total.filter(|&length| length > 0)
        .map(|length| (u128::from(frame.min(length)) * width as u128 / u128::from(length)) as usize)
        .unwrap_or(0);
    format!("progress: [{}{}]", "#".repeat(filled), "-".repeat(width - filled))
}

/// Keep the available keys on screen for the active pane. Input is modal:
/// printable shortcuts are text, and Ctrl+Q is its explicit exit chord.
fn key_hints(app: &App) -> [&'static str; 3] {
    if matches!(app.pane, Some(Pane::Input(_))) {
        return [
            "Input: type text (q, s, n, space and ? are characters here)",
            "Enter Submit | Backspace Delete | Esc Cancel",
            "Ctrl+Q Quit immediately from input",
        ];
    }
    let context = if app.right_focused {
        "Playlist: Up/Down Select | Enter/a Queue song | A Queue all | f/Esc Back"
    } else {
        match &app.pane {
            Some(Pane::Browser(_)) =>
                "Files: ↑/↓ Select | Enter Open/Play | Bksp Up | a Add | A Add folder | f Focus",
            Some(Pane::Devices(_)) =>
                "Output: Up/Down Select | Enter Choose | d/Esc Close",
            Some(Pane::Locations(_)) =>
                "Locations: Up/Down Select | Enter Browse | m Map/Mount | l/Esc Close",
            Some(Pane::Menu(_)) =>
                "Menu: Up/Down Select | Enter Open | Esc Back",
            Some(Pane::Playlists(pane)) if pane.focus == PlaylistFocus::Names =>
                "Lists: Up/Down Select | Enter Choose | a/A Queue list | Tab Songs | m Edit",
            Some(Pane::Playlists(_)) =>
                "Songs: Up/Down Select | Enter/a Queue song | A Queue list | Tab Lists | m Edit",
            Some(Pane::Settings(_)) =>
                "DAC: Up/Down Select | Enter Change | Esc Close",
            Some(Pane::Input(_)) => unreachable!(),
            None if app.right == RightView::Queue =>
                "Queue: r Random play | Tab Saved list | p Manage playlists",
            None =>
                "Saved list: f Focus songs | Tab Queue | p Manage playlists",
        }
    };
    [
        "q Quit | ? Help | Space Play/Pause | s Stop | n Next | ←/→ Seek | r Random",
        "b Browse | p Lists | l Locations | d DAC | m Menu | c F32->int | Tab View",
        context,
    ]
}

fn draw_key_hints(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let block = Block::default().title("Keys").borders(Borders::ALL);
    let inside = block.inner(area);
    frame.render_widget(block, area);
    for (row, hint) in key_hints(app).into_iter().enumerate() {
        if row as u16 >= inside.height {
            break;
        }
        frame.render_widget(
            Paragraph::new(hint),
            Rect::new(inside.x, inside.y + row as u16, inside.width, 1),
        );
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let state = &app.state;
    let track = metadata_line(&state.tags);
    let bottom = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(if track.is_empty() { 11 } else { 12 }),
            Constraint::Min(3),
            Constraint::Length(5),
        ])
        .split(frame.area());
    let title = Paragraph::new("sointty — direct/exclusive music player")
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, bottom[0]);
    draw_key_hints(frame, app, bottom[3]);

    let (output_line, fidelity) = match &state.output_spec {
        None => ("not configured".to_owned(), "not configured".to_owned()),
        Some(output) => (
            format!(
                "{} Hz / {} ch / {:?} ({} valid bits)",
                output.rate_hz, output.layout.channels, output.format, output.valid_bits
            ),
            if state.converted {
                "converted F32→integer (NOT bit-perfect)".to_owned()
            } else {
                "decoder PCM preserved (bit-perfect)".to_owned()
            },
        ),
    };
    let seconds = state.frame / u64::from(state.rate_hz.max(1));
    let default_timing = BufferConfig::default_for_rate(state.rate_hz);
    let timing_line = match state.timing {
        (None, None) => format!(
            "timing: Auto (DSD uses fixed safe timing; PCM now ~{}/{} frames)",
            default_timing.period_frames, default_timing.buffer_frames
        ),
        (period, buffer) => format!(
            "timing: Custom period {} / buffer {} frames",
            period
                .map(|value| value.to_string())
                .unwrap_or_else(|| format!("auto {}", default_timing.period_frames)),
            buffer
                .map(|value| value.to_string())
                .unwrap_or_else(|| format!("auto {}", default_timing.buffer_frames)),
        ),
    };
    let full_time = state.total_frames
        .map(|total| clock(total / u64::from(state.rate_hz.max(1))))
        .unwrap_or_else(|| "--:--".to_owned());
    let bar_width = usize::from(bottom[1].width.saturating_sub(2)).saturating_sub(12);
    let bar = progress_bar(state.frame, state.total_frames, bar_width);
    let playback = Paragraph::new(format!(
        "status: {}{}\nposition: {} frames\ndevice: {}\noutput: {}\nfidelity: {}\n{}\nF32→integer compatibility: {}\ntime: {} / {}\n{}",
        state.status,
        if track.is_empty() {
            String::new()
        } else {
            format!("\ntrack: {track}")
        },
        state.frame,
        state.device_display(),
        output_line,
        fidelity,
        timing_line,
        if state.float_to_int {
            "ON (non-bit-perfect when used)"
        } else {
            "OFF (strict)"
        },
        clock(seconds),
        full_time,
        bar,
    ))
    .block(Block::default().title("Playback").borders(Borders::ALL));
    frame.render_widget(playback, bottom[1]);

    let highlight = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    // Right column: selected saved playlist or the live queue.
    let render_right = |frame: &mut ratatui::Frame<'_>, area| match app.right {
        RightView::SavedList => {
            let playlist = state.catalog.selected_playlist();
            let items: Vec<ListItem> = if playlist.entries.is_empty() {
                vec![ListItem::new("(empty — browser: a adds item, A adds folder)")]
            } else {
                playlist
                    .entries
                    .iter()
                    .map(|entry| ListItem::new(queue_entry_name(entry)))
                    .collect()
            };
            let title = if app.right_focused {
                format!("Playlist [Enter/a song, A all, Esc back]: {}", playlist.name)
            } else {
                format!("Playlist [f focus, Tab queue]: {}", playlist.name)
            };
            let list = List::new(items)
                .block(Block::default().title(title).borders(Borders::ALL))
                .highlight_style(highlight)
                .highlight_symbol("> ");
            let mut list_state = ListState::default();
            if app.right_focused && !playlist.entries.is_empty() {
                list_state.select(Some(app.playlist_entry_sel.min(playlist.entries.len() - 1)));
            }
            frame.render_stateful_widget(list, area, &mut list_state);
        }
        RightView::Queue => {
            let mut items: Vec<ListItem> = Vec::new();
            if let Some(audible) = &state.audible {
                items.push(ListItem::new(format!(
                    "> #{} {}",
                    audible.id,
                    queue_entry_name(&audible.entry)
                )));
            }
            for item in &state.pending {
                items.push(ListItem::new(format!(
                    "  #{} {}",
                    item.id,
                    queue_entry_name(&item.entry)
                )));
            }
            if items.is_empty() {
                items.push(ListItem::new("(queue empty)"));
            }
            let list = List::new(items).block(
                Block::default()
                    .title(format!("Queue [r random: {}, Tab playlist]", if state.shuffle { "ON" } else { "OFF" }))
                    .borders(Borders::ALL),
            );
            frame.render_widget(list, area);
        }
    };

    match &app.pane {
        None => render_right(frame, bottom[2]),
        Some(pane) => {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                .split(bottom[2]);
            match pane {
                Pane::Browser(browser) => {
                    let items: Vec<ListItem> = browser
                        .items
                        .iter()
                        .map(|item| {
                            let marker = match item.kind {
                                BrowserItemKind::Dir => "[d] ",
                                BrowserItemKind::Audio => "    ",
                                BrowserItemKind::Playlist => "[p] ",
                            };
                            ListItem::new(format!("{marker}{}", item.name))
                        })
                        .collect();
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .title(format!(
                                    "Browser [l drives, f playlist] — {}",
                                    browser.current_dir.display()
                                ))
                                .borders(Borders::ALL),
                        )
                        .highlight_style(highlight)
                        .highlight_symbol("> ");
                    let mut list_state = ListState::default();
                    if !app.right_focused && !browser.items.is_empty() {
                        list_state.select(Some(browser.selected));
                    }
                    frame.render_stateful_widget(list, columns[0], &mut list_state);
                }
                Pane::Devices(devices) => {
                    let items: Vec<ListItem> = devices
                        .devices
                        .iter()
                        .map(|(id, name)| {
                            let marker = if *id == state.device { "* " } else { "  " };
                            ListItem::new(format!("{marker}{name}"))
                        })
                        .collect();
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .title("Output device — Enter: select, d/Esc: close")
                                .borders(Borders::ALL),
                        )
                        .highlight_style(highlight)
                        .highlight_symbol("> ");
                    let mut list_state = ListState::default();
                    if !devices.devices.is_empty() {
                        list_state.select(Some(devices.selected));
                    }
                    frame.render_stateful_widget(list, columns[0], &mut list_state);
                }
                Pane::Locations(locations) => {
                    let items: Vec<ListItem> = if locations.locations.is_empty() {
                        vec![ListItem::new("(no locations; menu: Enter path...)")]
                    } else {
                        locations
                            .locations
                            .iter()
                            .map(|location| ListItem::new(location.label.clone()))
                            .collect()
                    };
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .title("Locations — Enter: browse, m: map/mount, l/Esc: close")
                                .borders(Borders::ALL),
                        )
                        .highlight_style(highlight)
                        .highlight_symbol("> ");
                    let mut list_state = ListState::default();
                    if !locations.locations.is_empty() {
                        list_state.select(Some(locations.selected));
                    }
                    frame.render_stateful_widget(list, columns[0], &mut list_state);
                }
                Pane::Menu(menu) => {
                    let (title, items) = menu.stack.last().unwrap();
                    let items: Vec<ListItem> = items
                        .iter()
                        .map(|item| ListItem::new(item.label.clone()))
                        .collect();
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .title(format!("{title} — Enter: select, Esc: back"))
                                .borders(Borders::ALL),
                        )
                        .highlight_style(highlight)
                        .highlight_symbol("> ");
                    let mut list_state = ListState::default();
                    if !menu.current_items().is_empty() {
                        list_state.select(Some(menu.selected));
                    }
                    frame.render_stateful_widget(list, columns[0], &mut list_state);
                }
                Pane::Playlists(pane) => {
                    let names: Vec<ListItem> = state
                        .catalog
                        .playlists
                        .iter()
                        .enumerate()
                        .map(|(index, playlist)| {
                            let marker = if index == state.catalog.selected {
                                "* "
                            } else {
                                "  "
                            };
                            ListItem::new(format!("{marker}{} ({})", playlist.name, playlist.entries.len()))
                        })
                        .collect();
                    let title = if state.catalog_error.is_some() {
                        "Playlists (READ-ONLY: catalog file malformed)".to_owned()
                    } else {
                        "Playlists: Enter select, Tab focus, a queue list/song, A whole list".to_owned()
                    };
                    let list = List::new(names)
                        .block(Block::default().title(title).borders(Borders::ALL))
                        .highlight_style(if pane.focus == PlaylistFocus::Names {
                            highlight
                        } else {
                            Style::default()
                        })
                        .highlight_symbol("> ");
                    let mut list_state = ListState::default();
                    if !state.catalog.playlists.is_empty() {
                        list_state.select(Some(pane.name_sel));
                    }
                    frame.render_stateful_widget(list, columns[0], &mut list_state);

                    let playlist = &state.catalog.playlists[pane
                        .name_sel
                        .min(state.catalog.playlists.len().saturating_sub(1))];
                    let entries: Vec<ListItem> = if playlist.entries.is_empty() {
                        vec![ListItem::new("(empty)")]
                    } else {
                        playlist
                            .entries
                            .iter()
                            .map(|entry| ListItem::new(queue_entry_name(entry)))
                            .collect()
                    };
                    let list = List::new(entries)
                        .block(
                            Block::default()
                                .title(format!("{} entries: Enter/a song, A whole list", playlist.name))
                                .borders(Borders::ALL),
                        )
                        .highlight_style(if pane.focus == PlaylistFocus::Entries {
                            highlight
                        } else {
                            Style::default()
                        })
                        .highlight_symbol("> ");
                    let mut list_state = ListState::default();
                    if !playlist.entries.is_empty() {
                        list_state.select(Some(pane.entry_sel));
                    }
                    frame.render_stateful_widget(list, columns[1], &mut list_state);
                    return;
                }
                Pane::Settings(settings) => {
                    let default_timing = BufferConfig::default_for_rate(state.rate_hz);
                    let rows = vec![
                        format!("Output device: {}", state.device_display()),
                        "DAC timing: Auto (per-track rate)".to_owned(),
                        format!(
                            "DAC timing: Custom... (now period {} / buffer {})",
                            state
                                .timing
                                .0
                                .map(|value| value.to_string())
                                .unwrap_or_else(|| format!("auto {}", default_timing.period_frames)),
                            state
                                .timing
                                .1
                                .map(|value| value.to_string())
                                .unwrap_or_else(|| format!("auto {}", default_timing.buffer_frames)),
                        ),
                        format!(
                            "F32→integer compatibility: {} (OFF = strict bit-perfect)",
                            if state.float_to_int { "ON" } else { "OFF" }
                        ),
                    ];
                    let items: Vec<ListItem> = rows.into_iter().map(ListItem::new).collect();
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .title("DAC settings — Enter: change, Esc: close")
                                .borders(Borders::ALL),
                        )
                        .highlight_style(highlight)
                        .highlight_symbol("> ");
                    let mut list_state = ListState::default();
                    list_state.select(Some(settings.selected));
                    frame.render_stateful_widget(list, columns[0], &mut list_state);
                }
                Pane::Input(input) => {
                    let text = Paragraph::new(format!("{}\n{}_", input.prompt, input.buffer))
                        .block(Block::default().title("Input — Enter: ok, Esc: cancel").borders(Borders::ALL));
                    frame.render_widget(text, columns[0]);
                }
            }
            render_right(frame, columns[1]);
        }
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
        saved_devices: Arc<Mutex<Vec<DeviceId>>>,
        saved_float: Arc<Mutex<Vec<bool>>>,
        saved_dirs: Arc<Mutex<Vec<PathBuf>>>,
        saved_catalogs: Arc<Mutex<Vec<PlaylistCatalog>>>,
        saved_timing: Arc<Mutex<Vec<(Option<u32>, Option<u32>)>>>,
    }

    fn noop_hooks(devices: Vec<(DeviceId, String)>) -> Hooks {
        Hooks {
            device_provider: Box::new(move || devices.clone()),
            on_device_selected: Box::new(|_| Ok(())),
            on_float_to_int_selected: Box::new(|_| Ok(())),
            on_browser_dir: Box::new(|_| Ok(())),
            on_catalog: Box::new(|_| Ok(())),
            on_timing: Box::new(|_, _| Ok(())),
            locations: Box::new(|| Ok(Vec::new())),
            map_drive: None,
            disconnect_drive: None,
            mount_share: None,
        }
    }

    fn options() -> RunOptions {
        RunOptions {
            open_browser: false,
            initial_browser_dir: None,
            home_dir: None,
            current_device: "dev1".to_owned(),
            float_to_int: false,
            timing: (None, None),
            catalog: PlaylistCatalog::default(),
            catalog_error: None,
        }
    }

    fn harness(devices: Vec<(DeviceId, String)>) -> Harness {
        let (commands, sent) = crossbeam_channel::unbounded();
        let saved_devices = Arc::new(Mutex::new(Vec::new()));
        let saved_float = Arc::new(Mutex::new(Vec::new()));
        let saved_dirs = Arc::new(Mutex::new(Vec::new()));
        let saved_catalogs = Arc::new(Mutex::new(Vec::new()));
        let saved_timing = Arc::new(Mutex::new(Vec::new()));
        let hooks = {
            let devices_sink = Arc::clone(&saved_devices);
            let float_sink = Arc::clone(&saved_float);
            let dirs_sink = Arc::clone(&saved_dirs);
            let catalogs_sink = Arc::clone(&saved_catalogs);
            let timing_sink = Arc::clone(&saved_timing);
            Hooks {
                device_provider: Box::new(move || devices.clone()),
                on_device_selected: Box::new(move |device| {
                    devices_sink.lock().push(device.clone());
                    Ok(())
                }),
                on_float_to_int_selected: Box::new(move |enabled| {
                    float_sink.lock().push(enabled);
                    Ok(())
                }),
                on_browser_dir: Box::new(move |dir| {
                    dirs_sink.lock().push(dir.to_path_buf());
                    Ok(())
                }),
                on_catalog: Box::new(move |catalog| {
                    catalogs_sink.lock().push(catalog.clone());
                    Ok(())
                }),
                on_timing: Box::new(move |period, buffer| {
                    timing_sink.lock().push((period, buffer));
                    Ok(())
                }),
                locations: Box::new(|| Ok(Vec::new())),
                map_drive: None,
                disconnect_drive: None,
                mount_share: None,
            }
        };
        Harness {
            app: App::new(options(), hooks, commands),
            sent,
            saved_devices,
            saved_float,
            saved_dirs,
            saved_catalogs,
            saved_timing,
        }
    }

    impl Harness {
        fn key(&mut self, key: KeyCode) -> bool {
            self.app.handle_key(key)
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("sointty-tui-{tag}-{}-{nanos}", std::process::id()))
    }

    fn audio_browser() -> (PathBuf, Browser) {
        let dir = temp_dir("audio");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("track.flac"), b"").unwrap();
        let browser = Browser::open(dir.clone()).unwrap();
        (dir, browser)
    }

    #[test]
    fn enter_on_audio_enqueues_and_starts_playback() {
        // Regression: the old browser only sent Enqueue, so choosing a file
        // never started playback.
        let (dir, browser) = audio_browser();
        let mut h = harness(Vec::new());
        h.app.pane = Some(Pane::Browser(browser));
        assert!(!h.key(KeyCode::Enter));
        let enqueued = h.sent.recv().unwrap();
        assert!(matches!(enqueued, PlayerCommand::Enqueue(_)));
        let play = h.sent.recv().unwrap();
        assert!(matches!(play, PlayerCommand::Play));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn playback_keys_work_while_browser_is_open() {
        // Regression: the open browser swallowed every key except its own, so
        // pause/next/stop were unreachable while browsing.
        let (dir, browser) = audio_browser();
        let mut h = harness(Vec::new());
        h.app.pane = Some(Pane::Browser(browser));
        h.key(KeyCode::Char('n'));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Next));
        h.key(KeyCode::Char(' '));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Pause));
        h.key(KeyCode::Char(' '));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Play));
        h.key(KeyCode::Char('s'));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Stop));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn arrows_seek_relative_to_current_position() {
        let mut h = harness(Vec::new());
        h.app.state.frame = 60 * 44_100;
        h.key(KeyCode::Left);
        let command = h.sent.recv().unwrap();
        assert!(matches!(&command, PlayerCommand::SeekFrame(f) if *f == 50 * 44_100));
        h.key(KeyCode::Right);
        let command = h.sent.recv().unwrap();
        assert!(matches!(&command, PlayerCommand::SeekFrame(f) if *f == 70 * 44_100));
    }

    #[test]
    fn seek_before_track_start_clamps_to_zero() {
        let mut h = harness(Vec::new());
        h.app.state.frame = 5 * 44_100;
        h.key(KeyCode::Left);
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::SeekFrame(0)));
    }

    #[test]
    fn device_pane_selects_and_persists_device() {
        let devices = vec![
            ("dev1".to_owned(), "Built-in DAC".to_owned()),
            ("dev2".to_owned(), "USB DAC".to_owned()),
        ];
        let mut h = harness(devices);
        h.key(KeyCode::Char('d'));
        // Current device preselected; move to the USB DAC and confirm.
        h.key(KeyCode::Down);
        h.key(KeyCode::Enter);
        let command = h.sent.recv().unwrap();
        assert!(matches!(&command, PlayerCommand::SelectDevice(id) if id == "dev2"));
        assert_eq!(h.saved_devices.lock().as_slice(), &["dev2".to_owned()]);
        assert_eq!(h.app.state.device, "dev2");
        assert!(h.app.pane.is_none());
    }


    #[test]
    fn device_display_prefers_friendly_name_and_falls_back_to_id() {
        let mut h = harness(vec![(
            "{0.0.0.00000000}.{guid}".to_owned(),
            "iFi USB DAC".to_owned(),
        )]);
        // Startup cache primed from the provider; unknown ids fall back.
        assert_eq!(h.app.state.device_display(), "dev1");
        h.app.state.device = "{0.0.0.00000000}.{guid}".to_owned();
        assert_eq!(h.app.state.device_display(), "iFi USB DAC");
    }
    #[test]
    fn esc_closes_pane_before_quitting() {
        let mut h = harness(vec![("dev1".to_owned(), "DAC".to_owned())]);
        h.key(KeyCode::Char('d'));
        assert!(h.app.pane.is_some());
        assert!(!h.key(KeyCode::Esc));
        assert!(h.app.pane.is_none());
        assert!(h.key(KeyCode::Esc));
    }

    #[test]
    fn saver_error_is_reported_but_selection_still_applies() {
        let (commands, sent) = crossbeam_channel::unbounded();
        let mut hooks = noop_hooks(vec![("dev2".to_owned(), "USB DAC".to_owned())]);
        hooks.on_device_selected = Box::new(|_| Err("disk full".to_owned()));
        let mut app = App::new(options(), hooks, commands);
        app.handle_key(KeyCode::Char('d'));
        app.handle_key(KeyCode::Enter);
        let command = sent.recv().unwrap();
        assert!(matches!(&command, PlayerCommand::SelectDevice(id) if id == "dev2"));
        assert_eq!(app.state.device, "dev2");
        assert!(app.state.status.contains("not saved"));
    }

    #[test]
    fn space_retries_queue_after_playback_error() {
        let mut h = harness(Vec::new());
        h.app.apply_event(PlayerEvent::Error {
            track: Some(1),
            kind: sointty_core::PlayerError::DeviceBusy,
        });
        h.key(KeyCode::Char(' '));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Play));
    }

    #[test]
    fn compatibility_toggle_is_explicit_and_saved() {
        let mut h = harness(Vec::new());
        assert!(!h.app.state.float_to_int);
        h.key(KeyCode::Char('c'));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::SetFloatToInt(true)));
        assert!(h.app.state.float_to_int);
        h.key(KeyCode::Char('c'));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::SetFloatToInt(false)));
        assert!(!h.app.state.float_to_int);
        assert_eq!(*h.saved_float.lock(), vec![true, false]);
    }

    #[test]
    fn converted_playback_is_labeled_non_bit_perfect() {
        let mut h = harness(Vec::new());
        h.app.state.float_to_int = true;
        h.app.apply_event(PlayerEvent::Playing {
            track: 1,
            output: sointty_core::OutputSpec {
                device: "dev1".to_owned(),
                rate_hz: 44_100,
                layout: sointty_core::ChannelLayout::discrete(2),
                format: sointty_core::DeviceFormat::S32Le,
                valid_bits: 32,
            },
            converted: true,
        });
        assert!(h.app.state.status.contains("not bit-perfect"));
        h.app.apply_event(PlayerEvent::EndOfQueue);
        assert!(h.app.state.converted, "last output must still be labeled converted");
    }

    // ---- remembered browsing -------------------------------------------------

    #[test]
    fn browser_reopens_at_last_directory_and_persists_descend() {
        let dir = temp_dir("remember");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let mut h = harness(Vec::new());
        h.app.state.last_browser_dir = Some(dir.clone());
        // b opens at the remembered dir.
        h.key(KeyCode::Char('b'));
        let Some(Pane::Browser(browser)) = &h.app.pane else {
            panic!("browser should be open");
        };
        assert_eq!(browser.current_dir, dir);
        // Descend persists the new directory.
        h.key(KeyCode::Enter);
        let Some(Pane::Browser(browser)) = &h.app.pane else {
            panic!("browser should still be open");
        };
        assert_eq!(browser.current_dir, sub);
        assert_eq!(h.saved_dirs.lock().as_slice(), &[sub.clone()]);
        // Close and reopen: still at the subdirectory.
        h.key(KeyCode::Char('b'));
        assert!(h.app.pane.is_none());
        h.key(KeyCode::Char('b'));
        let Some(Pane::Browser(browser)) = &h.app.pane else {
            panic!("browser should reopen");
        };
        assert_eq!(browser.current_dir, sub);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removed_saved_folder_falls_back_to_home_with_warning() {
        let home = temp_dir("home");
        std::fs::create_dir_all(&home).unwrap();
        let gone = temp_dir("gone");
        // Never created: the saved folder is missing.
        let mut h = harness(Vec::new());
        h.app.state.last_browser_dir = Some(gone.clone());
        h.app.state.home_dir = Some(home.clone());
        h.key(KeyCode::Char('b'));
        let Some(Pane::Browser(browser)) = &h.app.pane else {
            panic!("browser should fall back to home");
        };
        assert_eq!(browser.current_dir, home);
        assert!(h.app.state.status.contains("unreadable"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn startup_never_uses_process_working_directory() {
        let mut opts = options();
        opts.open_browser = true;
        opts.initial_browser_dir = None;
        opts.home_dir = Some(temp_dir("home2"));
        std::fs::create_dir_all(opts.home_dir.as_ref().unwrap()).unwrap();
        let (commands, _sent) = crossbeam_channel::unbounded();
        let app = App::new(opts, noop_hooks(Vec::new()), commands);
        let Some(Pane::Browser(browser)) = &app.pane else {
            panic!("browser should open at startup");
        };
        assert_ne!(
            browser.current_dir,
            std::env::current_dir().unwrap(),
            "process cwd must never be the default"
        );
        std::fs::remove_dir_all(browser.current_dir.clone()).ok();
    }

    #[test]
    fn saved_folder_reopens_on_new_app_launch() {
        let root = temp_dir("relaunch");
        let sub = root.join("album");
        std::fs::create_dir_all(&sub).unwrap();
        let mut first = harness(Vec::new());
        first.app.state.last_browser_dir = Some(root.clone());
        first.key(KeyCode::Char('b'));
        first.key(KeyCode::Enter);
        let saved = first.saved_dirs.lock().last().cloned().unwrap();
        let mut opts = options();
        opts.open_browser = true;
        opts.initial_browser_dir = Some(saved);
        let (commands, _) = crossbeam_channel::unbounded();
        let relaunched = App::new(opts, noop_hooks(Vec::new()), commands);
        let Some(Pane::Browser(browser)) = &relaunched.pane else {
            panic!("browser should open at saved path");
        };
        assert_eq!(browser.current_dir, sub);
        std::fs::remove_dir_all(root).unwrap();
    }

    // ---- folder adds ----------------------------------------------------------

    #[test]
    fn adding_folder_collects_audio_only_in_deterministic_order() {
        let dir = temp_dir("collect");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("A.flac"), b"").unwrap();
        std::fs::write(dir.join("nested").join("b.mp3"), b"").unwrap();
        std::fs::write(dir.join("album.cue"), b"").unwrap();
        std::fs::write(dir.join("album.m3u8"), b"").unwrap();
        let browser = Browser::open(dir.clone()).unwrap();
        let mut h = harness(Vec::new());
        h.app.pane = Some(Pane::Browser(browser));
        h.key(KeyCode::Char('A'));
        let names: Vec<String> = h.app.state.catalog.playlists[0]
            .entries
            .iter()
            .map(|entry| {
                entry.path.file_name().unwrap().to_string_lossy().into_owned()
            })
            .collect();
        assert_eq!(names, ["b.mp3", "A.flac"]);
        assert_eq!(h.saved_catalogs.lock().len(), 1);
        // No playback command: the live queue is untouched.
        assert!(h.sent.try_recv().is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_folder_add_reports_zero_without_save() {
        let dir = temp_dir("empty");
        std::fs::create_dir_all(&dir).unwrap();
        let browser = Browser::open(dir.clone()).unwrap();
        let mut h = harness(Vec::new());
        h.app.pane = Some(Pane::Browser(browser));
        h.key(KeyCode::Char('A'));
        assert!(h.app.state.status.contains("no audio files"));
        assert!(h.saved_catalogs.lock().is_empty());
        assert!(h.app.state.catalog.playlists[0].entries.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- playlists -------------------------------------------------------------

    #[test]
    fn removed_folder_add_aborts_without_partial_save() {
        let dir = temp_dir("removed-add");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("track.flac"), b"").unwrap();
        let browser = Browser::open(dir.clone()).unwrap();
        let mut h = harness(Vec::new());
        h.app.pane = Some(Pane::Browser(browser));
        std::fs::remove_dir_all(&dir).unwrap();
        h.key(KeyCode::Char('A'));
        assert!(h.app.state.status.contains("nothing added"));
        assert!(h.saved_catalogs.lock().is_empty());
        assert!(h.app.state.catalog.selected_playlist().entries.is_empty());
    }

    #[test]
    fn create_rename_delete_playlists() {
        let mut h = harness(Vec::new());
        h.app.catalog_create("Chill".to_owned());
        assert_eq!(h.app.state.catalog.playlists.len(), 2);
        assert_eq!(h.app.state.catalog.selected, 1);
        // Duplicate names rejected case-insensitively.
        h.app.catalog_create("chill".to_owned());
        assert_eq!(h.app.state.catalog.playlists.len(), 2);
        assert!(h.app.state.status.contains("already exists"));
        h.app.catalog_rename("Chill 2".to_owned());
        assert_eq!(h.app.state.catalog.playlists[1].name, "Chill 2");
        h.app.catalog_delete();
        assert_eq!(h.app.state.catalog.playlists.len(), 1);
        assert_eq!(h.app.state.catalog.selected, 0);
        // The final list cannot be deleted.
        h.app.catalog_delete();
        assert_eq!(h.app.state.catalog.playlists.len(), 1);
        assert!(h.app.state.status.contains("final"));
    }

    #[test]
    fn switching_lists_never_touches_live_queue() {
        let mut h = harness(Vec::new());
        h.app.catalog_create("Second".to_owned());
        h.app.state.pending = vec![QueueItem {
            id: 7,
            entry: QueueEntry::from(PathBuf::from("/music/playing.flac")),
        }];
        h.app.catalog_select(0);
        assert_eq!(h.app.state.pending.len(), 1);
        assert_eq!(h.app.state.pending[0].id, 7);
        assert!(h.sent.try_recv().is_err(), "no playback commands on switch");
    }

    #[test]
    fn playlist_entry_enter_queues_and_plays_when_idle() {
        let mut h = harness(Vec::new());
        h.app.state.catalog.playlists[0].entries.push(QueueEntry::from(PathBuf::from("/t.flac")));
        h.key(KeyCode::Char('p'));
        h.key(KeyCode::Tab); // focus entries
        h.key(KeyCode::Enter);
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Enqueue(_)));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Play));
    }

    #[test]
    fn playlist_entry_enter_does_not_restart_while_playing() {
        let mut h = harness(Vec::new());
        h.app.state.catalog.playlists[0].entries.push(QueueEntry::from(PathBuf::from("/t.flac")));
        h.app.state.audible = Some(QueueItem {
            id: 3,
            entry: QueueEntry::from(PathBuf::from("/now.flac")),
        });
        h.key(KeyCode::Char('p'));
        h.key(KeyCode::Tab);
        h.key(KeyCode::Enter);
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Enqueue(_)));
        assert!(h.sent.try_recv().is_err(), "no Play while audible");
    }

    #[test]
    fn highlighted_playlist_and_song_append_without_selecting_or_restarting() {
        let mut h = harness(Vec::new());
        let first = QueueEntry::from(PathBuf::from("first.flac"));
        let second = QueueEntry::from(PathBuf::from("second.flac"));
        h.app.state.catalog.playlists.push(SavedPlaylist {
            name: "Second".to_owned(),
            entries: vec![first.clone(), second.clone()],
        });
        h.app.state.audible = Some(QueueItem {
            id: 4,
            entry: QueueEntry::from(PathBuf::from("playing.flac")),
        });
        h.key(KeyCode::Char('p'));
        h.key(KeyCode::Down); // highlighted but not the selected saved list
        h.key(KeyCode::Char('a'));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Enqueue(entry) if entry == first));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Enqueue(entry) if entry == second));
        assert_eq!(h.app.state.catalog.selected, 0);
        assert!(h.sent.try_recv().is_err(), "adding a whole list must not restart playback");
        h.key(KeyCode::Tab); // entries of the highlighted list
        h.key(KeyCode::Down);
        h.key(KeyCode::Char('a'));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Enqueue(entry) if entry == second));
        assert!(h.sent.try_recv().is_err(), "one song must not restart playback");
        h.key(KeyCode::Char('A'));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Enqueue(entry) if entry == first));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Enqueue(entry) if entry == second));
        assert!(h.sent.try_recv().is_err());
    }

    #[test]
    fn focused_visible_playlist_queues_whole_list_in_order() {
        let mut h = harness(Vec::new());
        let first = QueueEntry::from(PathBuf::from("a.flac"));
        let second = QueueEntry::from(PathBuf::from("b.flac"));
        h.app.state.catalog.playlists[0].entries = vec![first.clone(), second.clone()];
        h.key(KeyCode::Char('f'));
        h.key(KeyCode::Char('A'));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Enqueue(entry) if entry == first));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Enqueue(entry) if entry == second));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Play));
        assert!(h.sent.try_recv().is_err());
    }

    #[test]
    fn random_play_toggle_from_queue_menu_and_key() {
        let mut h = harness(Vec::new());
        h.key(KeyCode::Char('m'));
        h.app.dispatch(MenuAction::OpenQueue);
        h.app.dispatch(MenuAction::QueueToggleShuffle);
        assert!(h.app.state.shuffle);
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::SetShuffle(true)));
        h.key(KeyCode::Char('r'));
        assert!(!h.app.state.shuffle);
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::SetShuffle(false)));
        assert!(h.sent.try_recv().is_err());
    }

    #[test]
    fn malformed_catalog_disables_edits() {
        let mut opts = options();
        opts.catalog_error = Some("/data/playlists.toml: bad toml".to_owned());
        let (commands, _sent) = crossbeam_channel::unbounded();
        let mut app = App::new(opts, noop_hooks(Vec::new()), commands);
        app.catalog_create("Nope".to_owned());
        assert_eq!(app.state.catalog.playlists.len(), 1, "edit must be rejected");
        assert!(app.state.status.contains("read-only"));
        assert!(app.state.status.contains("/data/playlists.toml"));
    }

    #[test]
    fn failed_catalog_save_keeps_prior_model() {
        let (commands, _sent) = crossbeam_channel::unbounded();
        let mut hooks = noop_hooks(Vec::new());
        hooks.on_catalog = Box::new(|_| Err("disk full".to_owned()));
        let mut app = App::new(options(), hooks, commands);
        app.catalog_create("Nope".to_owned());
        assert_eq!(app.state.catalog.playlists.len(), 1, "failed save must not commit");
        assert!(app.state.status.contains("not saved"));
    }

    #[test]
    fn import_cue_preserves_ranges_and_duplicate_paths() {
        let dir = temp_dir("cue");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("album.wav"), b"").unwrap();
        std::fs::write(
            dir.join("album.cue"),
            "FILE \"album.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 01:00:00\n",
        )
        .unwrap();
        let mut h = harness(Vec::new());
        h.app.catalog_import(dir.join("album.cue"), "Album".to_owned());
        let playlist = &h.app.state.catalog.playlists[1];
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[0].path, playlist.entries[1].path);
        assert_eq!(
            playlist.entries[0].cue_range.unwrap().start_cd,
            0,
        );
        assert!(playlist.entries[0].cue_range.unwrap().end_cd.is_some());
        assert!(playlist.entries[1].cue_range.unwrap().end_cd.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- timing ----------------------------------------------------------------

    #[test]
    fn timing_auto_clears_overrides_and_custom_validates() {
        let mut h = harness(Vec::new());
        h.app.set_timing(Some(1_024), Some(4_096));
        assert_eq!(h.app.state.timing, (Some(1_024), Some(4_096)));
        assert!(matches!(
            h.sent.recv().unwrap(),
            PlayerCommand::SetTiming { period_frames: Some(1_024), buffer_frames: Some(4_096) }
        ));
        // Undersized buffer is rejected without a save or command.
        h.app.set_timing(Some(4_096), Some(1_024));
        assert_eq!(h.app.state.timing, (Some(1_024), Some(4_096)));
        assert!(h.app.state.status.contains("twice"));
        // Auto clears both fields.
        h.app.set_timing(None, None);
        assert_eq!(h.app.state.timing, (None, None));
        assert!(matches!(
            h.sent.recv().unwrap(),
            PlayerCommand::SetTiming { period_frames: None, buffer_frames: None }
        ));
        assert_eq!(
            h.saved_timing.lock().as_slice(),
            &[(Some(1_024), Some(4_096)), (None, None)]
        );
    }

    // ---- menu -------------------------------------------------------------------

    #[test]
    fn menu_navigates_and_quit_exits() {
        let mut h = harness(Vec::new());
        h.key(KeyCode::Char('m'));
        assert!(matches!(h.app.pane, Some(Pane::Menu(_))));
        // Bottom item is Quit.
        for _ in 0..7 {
            h.key(KeyCode::Down);
        }
        assert!(h.key(KeyCode::Enter), "Quit must exit");
    }

    #[test]
    fn menu_esc_pops_submenu_before_closing() {
        let mut h = harness(Vec::new());
        h.key(KeyCode::Char('m'));
        // Locations item opens the Locations submenu.
        h.key(KeyCode::Down);
        h.key(KeyCode::Enter);
        let Some(Pane::Menu(menu)) = &h.app.pane else {
            panic!("menu should be open");
        };
        assert_eq!(menu.stack.len(), 2);
        h.key(KeyCode::Esc);
        let Some(Pane::Menu(menu)) = &h.app.pane else {
            panic!("menu should still be open");
        };
        assert_eq!(menu.stack.len(), 1);
        h.key(KeyCode::Esc);
        assert!(h.app.pane.is_none());
    }

    #[test]
    fn keyboard_footer_tracks_browser_playlist_focus_and_input() {
        fn screen(app: &App) -> String {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
            terminal.draw(|frame| draw(frame, app)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..24).map(|y| {
                (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>()
            }).collect::<Vec<_>>().join("\n")
        }

        let dir = temp_dir("key-hints");
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = harness(Vec::new());
        h.app.pane = Some(Pane::Browser(Browser::open(dir.clone()).unwrap()));
        let browser = screen(&h.app);
        assert!(browser.contains("q Quit | ? Help"), "{browser}");
        assert!(browser.contains("a Add | A Add folder | f Focus"), "{browser}");
        h.key(KeyCode::Char('p'));
        let names = screen(&h.app);
        assert!(names.contains("a/A Queue list | Tab Songs"), "{names}");
        h.key(KeyCode::Tab);
        let entries = screen(&h.app);
        assert!(entries.contains("Enter/a Queue song | A Queue list"), "{entries}");
        h.app.start_input("New playlist name:", InputAction::PlaylistCreate);
        let input = screen(&h.app);
        assert!(input.contains("Ctrl+Q Quit immediately from input"), "{input}");
        assert!(!input.contains("q Quit | ? Help"), "{input}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quit_bypasses_nested_menus_and_text_input_requires_control_q() {
        let mut h = harness(Vec::new());
        h.key(KeyCode::Char('m'));
        h.app.dispatch(MenuAction::OpenQueue);
        h.key(KeyCode::Char('?'));
        let Some(Pane::Menu(menu)) = &h.app.pane else {
            panic!("help must open as a menu");
        };
        assert_eq!(menu.stack.last().unwrap().0, "Help");
        assert!(h.key(KeyCode::Char('q')), "q exits from a nested menu in one press");

        h.app.start_input("Name:", InputAction::PlaylistCreate);
        assert!(!h.app.handle_key_event(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
        let Some(Pane::Input(input)) = &h.app.pane else {
            panic!("q must remain valid input");
        };
        assert_eq!(input.buffer, "q");
        assert!(h.app.handle_key_event(KeyEvent::new(
            KeyCode::Char('q'), KeyModifiers::CONTROL
        )), "Ctrl+Q exits directly from input");
    }

    // ---- input ------------------------------------------------------------------

    #[test]
    fn input_owns_text_keys() {
        let mut h = harness(Vec::new());
        h.app.start_input("Name:", InputAction::PlaylistCreate);
        // These would be transport, queue-control, or quit keys globally.
        for ch in ['q', 's', 'n', 'r', ' '] {
            assert!(!h.key(KeyCode::Char(ch)), "input must not quit");
        }
        let Some(Pane::Input(input)) = &h.app.pane else {
            panic!("input should be open");
        };
        assert_eq!(input.buffer, "qsnr ");
        h.key(KeyCode::Esc);
        assert!(h.app.pane.is_none());
        assert!(h.sent.try_recv().is_err(), "no commands leaked from input");
    }

    #[test]
    fn tab_toggles_right_view_outside_playlists() {
        let mut h = harness(Vec::new());
        assert_eq!(h.app.right, RightView::SavedList);
        h.key(KeyCode::Tab);
        assert_eq!(h.app.right, RightView::Queue);
        h.key(KeyCode::Tab);
        assert_eq!(h.app.right, RightView::SavedList);
    }

    #[test]
    fn browser_root_opens_drives_and_cancel_restores_browser() {
        let root = std::env::current_dir().unwrap()
            .ancestors().last().unwrap().to_path_buf();
        let other = temp_dir("different-drive");
        std::fs::create_dir_all(&other).unwrap();
        let mut h = harness(Vec::new());
        h.app.hooks.locations = Box::new({
            let root = root.clone();
            let other = other.clone();
            move || Ok(vec![
                Location {
                    label: root.display().to_string(),
                    path: root.clone(),
                    mapped_network: false,
                },
                Location {
                    label: other.display().to_string(),
                    path: other.clone(),
                    mapped_network: false,
                },
            ])
        });
        h.app.pane = Some(Pane::Browser(Browser::open(root.clone()).unwrap()));
        h.key(KeyCode::Backspace);
        assert!(matches!(h.app.pane, Some(Pane::Locations(_))));
        h.key(KeyCode::Esc);
        let Some(Pane::Browser(browser)) = &h.app.pane else {
            panic!("canceled drive selection must restore browser");
        };
        assert_eq!(browser.current_dir, root);
        assert!(h.saved_dirs.lock().is_empty(), "no fictitious directory saved");

        h.key(KeyCode::Backspace);
        h.key(KeyCode::Down);
        h.key(KeyCode::Enter);
        let Some(Pane::Browser(browser)) = &h.app.pane else {
            panic!("selecting a location must browse its path");
        };
        assert_eq!(browser.current_dir, other);
        assert_eq!(h.saved_dirs.lock().as_slice(), [other.clone()]);
        std::fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn focused_visible_playlist_plays_selected_entry_without_replacing_list() {
        let mut h = harness(Vec::new());
        let first = QueueEntry::from(PathBuf::from("first.flac"));
        let second = QueueEntry::from(PathBuf::from("second.flac"));
        h.app.state.catalog.playlists[0].entries = vec![first, second.clone()];
        h.key(KeyCode::Char('f'));
        assert!(h.app.right_focused);
        h.key(KeyCode::Down);
        h.key(KeyCode::Enter);
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Enqueue(entry) if entry == second));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Play));
        assert!(h.sent.try_recv().is_err());
        assert_eq!(h.app.state.catalog.selected_playlist().entries.len(), 2);
        h.key(KeyCode::Esc);
        assert!(!h.app.right_focused);
        h.key(KeyCode::Tab);
        assert_eq!(h.app.right, RightView::Queue);
    }

    #[test]
    fn canceling_locations_path_prompt_restores_locations_pane() {
        let mut h = harness(Vec::new());
        h.key(KeyCode::Char('l'));
        assert!(matches!(h.app.pane, Some(Pane::Locations(_))));
        h.app.start_input("Path:", InputAction::LocationPath);
        assert!(matches!(h.app.pane, Some(Pane::Input(_))));
        h.key(KeyCode::Esc);
        let Some(Pane::Locations(_)) = &h.app.pane else {
            panic!("Esc must restore the Locations pane");
        };
    }

    #[test]
    fn failed_locations_path_restores_locations_pane() {
        let mut h = harness(Vec::new());
        h.key(KeyCode::Char('l'));
        h.app.start_input("Path:", InputAction::LocationPath);
        for ch in "/definitely/not/a/real/dir".chars() {
            h.key(KeyCode::Char(ch));
        }
        h.key(KeyCode::Enter);
        assert!(h.app.state.status.contains("cannot open"));
        let Some(Pane::Locations(_)) = &h.app.pane else {
            panic!("a failed path must restore the Locations pane");
        };
    }

    #[test]
    fn failing_map_drive_restores_locations_pane() {
        let mut h = harness(Vec::new());
        h.app.hooks.map_drive = Some(Box::new(|_, _| Err("OS error 85".to_owned())));
        h.key(KeyCode::Char('l'));
        h.app.start_input("Drive letter:", InputAction::MapDriveLetter);
        h.key(KeyCode::Char('Z'));
        h.key(KeyCode::Enter);
        // Second prompt in the chain: share path.
        let Some(Pane::Input(input)) = &h.app.pane else {
            panic!("share prompt should follow the letter");
        };
        assert!(matches!(input.action, InputAction::MapDriveShare('Z')));
        for ch in "\\\\nas\\music".chars() {
            h.key(KeyCode::Char(ch));
        }
        h.key(KeyCode::Enter);
        let action = h.app.pending_os.take().unwrap();
        h.app.run_os_action(action);
        assert_eq!(h.app.state.status, "OS error 85");
        let Some(Pane::Locations(_)) = &h.app.pane else {
            panic!("a failed mapping must restore the Locations pane");
        };
    }

    #[test]
    fn successful_map_drive_opens_browser_and_drops_origin() {
        let root = temp_dir("mapped");
        std::fs::create_dir_all(&root).unwrap();
        let mapped = root.clone();
        let mut h = harness(Vec::new());
        h.app.hooks.map_drive = Some(Box::new(move |_, _| Ok(mapped.clone())));
        h.key(KeyCode::Char('l'));
        h.app.start_input("Drive letter:", InputAction::MapDriveLetter);
        h.key(KeyCode::Char('Z'));
        h.key(KeyCode::Enter);
        for ch in "\\\\nas\\music".chars() {
            h.key(KeyCode::Char(ch));
        }
        h.key(KeyCode::Enter);
        let action = h.app.pending_os.take().unwrap();
        h.app.run_os_action(action);
        let Some(Pane::Browser(browser)) = &h.app.pane else {
            panic!("successful mapping must open the browser");
        };
        assert_eq!(browser.current_dir, root);
        assert!(h.app.input_origin.is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn playback_duration_correlates_with_track_and_progress_is_visible() {
        let mut h = harness(Vec::new());
        h.app.apply_event(PlayerEvent::Playing {
            track: 7,
            output: OutputSpec {
                device: "dev1".to_owned(),
                rate_hz: 44_100,
                layout: sointty_core::ChannelLayout::discrete(2),
                format: sointty_core::DeviceFormat::S16Le,
                valid_bits: 16,
            },
            converted: false,
        });
        h.app.apply_event(PlayerEvent::Duration { track: 8, total_frames: Some(10) });
        assert_eq!(h.app.state.total_frames, None);
        h.app.apply_event(PlayerEvent::Duration { track: 7, total_frames: Some(441_000) });
        h.app.apply_event(PlayerEvent::Position { track: 8, frame: 999 });
        assert_eq!(h.app.state.frame, 0);
        h.app.apply_event(PlayerEvent::Position { track: 7, frame: 220_500 });
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &h.app)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen = (0..24).map(|y| {
            (0..90).map(|x| buffer[(x, y)].symbol()).collect::<String>()
        }).collect::<Vec<_>>().join("\n");
        assert!(screen.contains("time: 00:05 / 00:10"), "{screen}");
        assert!(screen.contains(&progress_bar(220_500, Some(441_000), 76)), "{screen}");

        h.app.apply_event(PlayerEvent::EndOfQueue);
        assert_eq!(h.app.state.total_frames, None);
        assert_eq!(h.app.state.frame, 0);
    }

    #[test]
    fn unknown_and_clamped_progress_never_claims_a_duration() {
        assert_eq!(progress_bar(80, None, 10), "progress: [----------]");
        assert_eq!(progress_bar(80, Some(40), 10), "progress: [##########]");
        assert_eq!(clock(3_661), "1:01:01");
    }

}
