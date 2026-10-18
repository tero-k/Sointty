//! The single typed binding table. Every key dispatch, the footer, and the
//! `?` help screen are driven from [`BINDINGS`], so a binding cannot exist
//! in one place but not another.
//!
//! Dispatch precedence (see [`lookup`]): the caller handles modal text
//! input, picker, and move overlays first; then pane-specific bindings win
//! over global ones for the same key.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Top-level screens selected by `1`/`2`/`3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Library,
    Playlists,
    Settings,
}

/// Which pane holds keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Library,
    Queue,
    PlaylistNames,
    PlaylistTracks,
    SettingsSections,
    SettingsRows,
}

/// Modal state machine kinds; the data-carrying `Mode` enum lives in the
/// app, this kind drives dispatch and help/footer selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeKind {
    Normal,
    Search,
    Move,
    /// Editable text input.
    Prompt,
    /// Dirty-catalog quit confirmation (y/n/Esc).
    QuitConfirm,
    Help,
    Locations,
    PlaylistPicker,
    Actions,
}

/// Everything a key press can mean. Context decides the concrete behavior;
/// this enum is deliberately flat so the table stays the single source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    CursorUp,
    CursorDown,
    /// Enter: open/play/jump/apply depending on context.
    Activate,
    /// Backspace: browser directory up, or open Locations at a root.
    BrowserUp,
    /// Tab: swap panes / cycle scope.
    FocusNext,
    ShowLibrary,
    ShowPlaylists,
    ShowSettings,
    OpenSearch,
    TogglePause,
    Stop,
    NextTrack,
    SeekBack,
    SeekForward,
    ToggleShuffle,
    Quit,
    BackOrQuit,
    CloseOverlay,
    DeleteChar,
    OpenFolder,
    ConfirmYes,
    ConfirmNo,
    ToggleHelp,
    OpenLocations,
    OpenActions,
    /// `d` outside Queue/playlist tracks: jump to Output settings.
    OpenOutputSettings,
    FocusPlaylist,
    /// `a`: enqueue the selected song/folder/expanded playlist.
    EnqueueSelected,
    /// `A`: enqueue the selected track's album or directory.
    EnqueueAlbum,
    /// `p`: add-to-playlist picker (Library/Search) or Playlists screen.
    AddToPlaylist,
    NewPlaylist,
    RenamePlaylist,
    ToggleSelect,
    RangeSelect,
    RemoveRows,
    /// `Q`: enqueue the selected playlist rows in order.
    EnqueueSelection,
    MoveStart,
    ResetRow,
    ToggleSetting,
    QueueRemove,
    /// Clear the UI-only past-row history (hidden from the footer).
    QueueClearHistory,
    SaveCatalog,
    ToggleFloatToInt,
    ToggleIntToFloat,
    ToggleSharedPlayback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Global,
    On(Focus),
    In(ModeKind),
}

/// One binding: match data plus footer/help presentation.
pub struct Binding {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub action: Action,
    scope: Scope,
    /// Keycap label for footer and help.
    pub key: &'static str,
    /// What the key does, for footer and help.
    pub label: &'static str,
    /// Eligible for the context footer. `c`/`i` stay footer-hidden.
    pub footer: bool,
    /// Help screen group.
    pub group: &'static str,
}

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;

const fn global(
    code: KeyCode,
    action: Action,
    key: &'static str,
    label: &'static str,
    group: &'static str,
) -> Binding {
    Binding {
        code,
        modifiers: NONE,
        action,
        scope: Scope::Global,
        key,
        label,
        footer: true,
        group,
    }
}

const fn on(
    focus: Focus,
    code: KeyCode,
    action: Action,
    key: &'static str,
    label: &'static str,
    group: &'static str,
) -> Binding {
    Binding {
        code,
        modifiers: NONE,
        action,
        scope: Scope::On(focus),
        key,
        label,
        footer: true,
        group,
    }
}

const fn in_mode(
    mode: ModeKind,
    code: KeyCode,
    action: Action,
    key: &'static str,
    label: &'static str,
    group: &'static str,
) -> Binding {
    Binding {
        code,
        modifiers: NONE,
        action,
        scope: Scope::In(mode),
        key,
        label,
        footer: true,
        group,
    }
}

/// The whole binding table. Pane-specific (`On`) entries precede globals
/// for the same key so the focused pane wins.
pub static BINDINGS: &[Binding] = &[
    // -- Queue focus ------------------------------------------------------
    on(
        Focus::Queue,
        KeyCode::Char('d'),
        Action::QueueRemove,
        "d",
        "remove",
        "Queue",
    ),
    on(
        Focus::Queue,
        KeyCode::Char('m'),
        Action::MoveStart,
        "m",
        "move",
        "Queue",
    ),
    on(
        Focus::Queue,
        KeyCode::Enter,
        Action::Activate,
        "enter",
        "jump",
        "Queue",
    ),
    Binding {
        footer: false,
        ..on(
            Focus::Queue,
            KeyCode::Char('c'),
            Action::QueueClearHistory,
            "c",
            "clear played history",
            "Queue",
        )
    },
    // -- Playlist names focus ---------------------------------------------
    on(
        Focus::PlaylistNames,
        KeyCode::Char('n'),
        Action::NewPlaylist,
        "n",
        "new playlist",
        "Playlists",
    ),
    on(
        Focus::PlaylistNames,
        KeyCode::Enter,
        Action::Activate,
        "enter",
        "select list",
        "Playlists",
    ),
    // -- Playlist tracks focus --------------------------------------------
    on(
        Focus::PlaylistTracks,
        KeyCode::Char(' '),
        Action::ToggleSelect,
        "space",
        "select",
        "Playlists",
    ),
    on(
        Focus::PlaylistTracks,
        KeyCode::Char('v'),
        Action::RangeSelect,
        "v",
        "select range",
        "Playlists",
    ),
    on(
        Focus::PlaylistTracks,
        KeyCode::Char('d'),
        Action::RemoveRows,
        "d",
        "remove selected",
        "Playlists",
    ),
    on(
        Focus::PlaylistTracks,
        KeyCode::Char('m'),
        Action::MoveStart,
        "m",
        "move selected",
        "Playlists",
    ),
    on(
        Focus::PlaylistTracks,
        KeyCode::Char('r'),
        Action::RenamePlaylist,
        "r",
        "rename list",
        "Playlists",
    ),
    on(
        Focus::PlaylistTracks,
        KeyCode::Char('Q'),
        Action::EnqueueSelection,
        "Q",
        "queue selected",
        "Playlists",
    ),
    on(
        Focus::PlaylistTracks,
        KeyCode::Enter,
        Action::Activate,
        "enter",
        "play from here",
        "Playlists",
    ),
    // -- Settings focus ----------------------------------------------------
    on(
        Focus::SettingsRows,
        KeyCode::Char(' '),
        Action::ToggleSetting,
        "space",
        "toggle",
        "Settings",
    ),
    on(
        Focus::SettingsRows,
        KeyCode::Char('r'),
        Action::ResetRow,
        "r",
        "reset",
        "Settings",
    ),
    on(
        Focus::SettingsRows,
        KeyCode::Left,
        Action::SeekBack,
        "←",
        "adjust",
        "Settings",
    ),
    on(
        Focus::SettingsRows,
        KeyCode::Right,
        Action::SeekForward,
        "→",
        "adjust",
        "Settings",
    ),
    on(
        Focus::SettingsSections,
        KeyCode::Char('r'),
        Action::ResetRow,
        "r",
        "reset",
        "Settings",
    ),
    on(
        Focus::SettingsRows,
        KeyCode::Enter,
        Action::Activate,
        "enter",
        "apply",
        "Settings",
    ),
    on(
        Focus::Library,
        KeyCode::Enter,
        Action::Activate,
        "enter",
        "open/play",
        "Library",
    ),
    // -- Modal overlays ---------------------------------------------------
    in_mode(
        ModeKind::Search,
        KeyCode::Tab,
        Action::FocusNext,
        "tab",
        "cycle search scope",
        "Search",
    ),
    in_mode(
        ModeKind::Search,
        KeyCode::Enter,
        Action::Activate,
        "enter",
        "play result",
        "Search",
    ),
    in_mode(
        ModeKind::Search,
        KeyCode::Char('a'),
        Action::EnqueueSelected,
        "a",
        "queue result",
        "Search",
    ),
    in_mode(
        ModeKind::Search,
        KeyCode::Char('p'),
        Action::AddToPlaylist,
        "p",
        "add result to playlist",
        "Search",
    ),
    in_mode(
        ModeKind::Search,
        KeyCode::Char('o'),
        Action::OpenFolder,
        "o",
        "open folder/list",
        "Search",
    ),
    in_mode(
        ModeKind::Search,
        KeyCode::Up,
        Action::CursorUp,
        "↑",
        "previous result",
        "Search",
    ),
    in_mode(
        ModeKind::Search,
        KeyCode::Down,
        Action::CursorDown,
        "↓",
        "next result",
        "Search",
    ),
    in_mode(
        ModeKind::Search,
        KeyCode::Backspace,
        Action::DeleteChar,
        "bksp",
        "delete text",
        "Search",
    ),
    in_mode(
        ModeKind::Search,
        KeyCode::Esc,
        Action::CloseOverlay,
        "esc",
        "close search",
        "Search",
    ),
    in_mode(
        ModeKind::Move,
        KeyCode::Up,
        Action::CursorUp,
        "↑",
        "previous drop gap",
        "Move",
    ),
    in_mode(
        ModeKind::Move,
        KeyCode::Down,
        Action::CursorDown,
        "↓",
        "next drop gap",
        "Move",
    ),
    in_mode(
        ModeKind::Move,
        KeyCode::Enter,
        Action::Activate,
        "enter",
        "drop here",
        "Move",
    ),
    in_mode(
        ModeKind::Move,
        KeyCode::Esc,
        Action::CloseOverlay,
        "esc",
        "cancel move",
        "Move",
    ),
    in_mode(
        ModeKind::Move,
        KeyCode::Char('q'),
        Action::Quit,
        "q",
        "quit",
        "Move",
    ),
    in_mode(
        ModeKind::Prompt,
        KeyCode::Enter,
        Action::Activate,
        "enter",
        "submit",
        "Input",
    ),
    in_mode(
        ModeKind::Prompt,
        KeyCode::Backspace,
        Action::DeleteChar,
        "bksp",
        "delete text",
        "Input",
    ),
    in_mode(
        ModeKind::Prompt,
        KeyCode::Esc,
        Action::CloseOverlay,
        "esc",
        "cancel input",
        "Input",
    ),
    in_mode(
        ModeKind::QuitConfirm,
        KeyCode::Char('y'),
        Action::ConfirmYes,
        "y",
        "save and quit",
        "Input",
    ),
    in_mode(
        ModeKind::QuitConfirm,
        KeyCode::Char('n'),
        Action::ConfirmNo,
        "n",
        "discard and quit",
        "Input",
    ),
    Binding {
        footer: false,
        ..in_mode(
            ModeKind::QuitConfirm,
            KeyCode::Char('Y'),
            Action::ConfirmYes,
            "Y",
            "save and quit",
            "Input",
        )
    },
    Binding {
        footer: false,
        ..in_mode(
            ModeKind::QuitConfirm,
            KeyCode::Char('N'),
            Action::ConfirmNo,
            "N",
            "discard and quit",
            "Input",
        )
    },
    in_mode(
        ModeKind::QuitConfirm,
        KeyCode::Esc,
        Action::CloseOverlay,
        "esc",
        "cancel quit",
        "Input",
    ),
    in_mode(
        ModeKind::Help,
        KeyCode::Esc,
        Action::CloseOverlay,
        "esc",
        "close help",
        "General",
    ),
    in_mode(
        ModeKind::Help,
        KeyCode::Char('?'),
        Action::CloseOverlay,
        "?",
        "close help",
        "General",
    ),
    in_mode(
        ModeKind::Help,
        KeyCode::Char('q'),
        Action::Quit,
        "q",
        "quit",
        "General",
    ),
    in_mode(
        ModeKind::Help,
        KeyCode::Up,
        Action::CursorUp,
        "↑",
        "scroll help up",
        "General",
    ),
    in_mode(
        ModeKind::Help,
        KeyCode::Down,
        Action::CursorDown,
        "↓",
        "scroll help down",
        "General",
    ),
    in_mode(
        ModeKind::Help,
        KeyCode::PageUp,
        Action::CursorUp,
        "pgup",
        "page up",
        "General",
    ),
    in_mode(
        ModeKind::Help,
        KeyCode::PageDown,
        Action::CursorDown,
        "pgdn",
        "page down",
        "General",
    ),
    in_mode(
        ModeKind::Locations,
        KeyCode::Up,
        Action::CursorUp,
        "↑",
        "previous location",
        "Locations",
    ),
    in_mode(
        ModeKind::Locations,
        KeyCode::Down,
        Action::CursorDown,
        "↓",
        "next location",
        "Locations",
    ),
    in_mode(
        ModeKind::Locations,
        KeyCode::Enter,
        Action::Activate,
        "enter",
        "browse location",
        "Locations",
    ),
    in_mode(
        ModeKind::Locations,
        KeyCode::Char('x'),
        Action::OpenActions,
        "x",
        "path / refresh / map / disconnect / mount",
        "Locations",
    ),
    in_mode(
        ModeKind::Locations,
        KeyCode::Char('m'),
        Action::OpenActions,
        "m",
        "location actions",
        "Locations",
    ),
    in_mode(
        ModeKind::Locations,
        KeyCode::Esc,
        Action::CloseOverlay,
        "esc",
        "close locations",
        "Locations",
    ),
    in_mode(
        ModeKind::Locations,
        KeyCode::Char('q'),
        Action::Quit,
        "q",
        "quit",
        "Locations",
    ),
    in_mode(
        ModeKind::PlaylistPicker,
        KeyCode::Up,
        Action::CursorUp,
        "↑",
        "previous list",
        "Playlist picker",
    ),
    in_mode(
        ModeKind::PlaylistPicker,
        KeyCode::Down,
        Action::CursorDown,
        "↓",
        "next list",
        "Playlist picker",
    ),
    in_mode(
        ModeKind::PlaylistPicker,
        KeyCode::Enter,
        Action::Activate,
        "enter",
        "add here",
        "Playlist picker",
    ),
    in_mode(
        ModeKind::PlaylistPicker,
        KeyCode::Char('n'),
        Action::NewPlaylist,
        "n",
        "new playlist",
        "Playlist picker",
    ),
    in_mode(
        ModeKind::PlaylistPicker,
        KeyCode::Esc,
        Action::CloseOverlay,
        "esc",
        "cancel add",
        "Playlist picker",
    ),
    in_mode(
        ModeKind::PlaylistPicker,
        KeyCode::Char('q'),
        Action::Quit,
        "q",
        "quit",
        "Playlist picker",
    ),
    in_mode(
        ModeKind::Actions,
        KeyCode::Up,
        Action::CursorUp,
        "↑",
        "previous action",
        "Actions",
    ),
    in_mode(
        ModeKind::Actions,
        KeyCode::Down,
        Action::CursorDown,
        "↓",
        "next action",
        "Actions",
    ),
    in_mode(
        ModeKind::Actions,
        KeyCode::Enter,
        Action::Activate,
        "enter",
        "run action",
        "Actions",
    ),
    in_mode(
        ModeKind::Actions,
        KeyCode::Esc,
        Action::CloseOverlay,
        "esc",
        "close actions",
        "Actions",
    ),
    in_mode(
        ModeKind::Actions,
        KeyCode::Char('q'),
        Action::Quit,
        "q",
        "quit",
        "Actions",
    ),
    // -- Global ------------------------------------------------------------
    global(
        KeyCode::Char('1'),
        Action::ShowLibrary,
        "1",
        "library",
        "General",
    ),
    global(
        KeyCode::Char('2'),
        Action::ShowPlaylists,
        "2",
        "playlists",
        "General",
    ),
    global(
        KeyCode::Char('3'),
        Action::ShowSettings,
        "3",
        "settings",
        "General",
    ),
    global(
        KeyCode::Char('/'),
        Action::OpenSearch,
        "/",
        "search",
        "General",
    ),
    global(
        KeyCode::Char('?'),
        Action::ToggleHelp,
        "?",
        "all keys",
        "General",
    ),
    global(KeyCode::Char('q'), Action::Quit, "q", "quit", "General"),
    global(
        KeyCode::Esc,
        Action::BackOrQuit,
        "esc",
        "back / quit from home",
        "General",
    ),
    Binding {
        modifiers: CTRL,
        ..global(
            KeyCode::Char('q'),
            Action::Quit,
            "ctrl+q",
            "quit from text",
            "General",
        )
    },
    global(
        KeyCode::Up,
        Action::CursorUp,
        "↑",
        "previous row",
        "General",
    ),
    global(
        KeyCode::Down,
        Action::CursorDown,
        "↓",
        "next row",
        "General",
    ),
    Binding {
        footer: false,
        ..global(
            KeyCode::Char('k'),
            Action::CursorUp,
            "k",
            "previous row",
            "General",
        )
    },
    Binding {
        footer: false,
        ..global(
            KeyCode::Char('j'),
            Action::CursorDown,
            "j",
            "next row",
            "General",
        )
    },
    global(
        KeyCode::Char('b'),
        Action::ShowLibrary,
        "b",
        "library",
        "General",
    ),
    global(
        KeyCode::Char('l'),
        Action::OpenLocations,
        "l",
        "locations",
        "General",
    ),
    global(
        KeyCode::Char('x'),
        Action::OpenActions,
        "x",
        "actions",
        "General",
    ),
    global(
        KeyCode::Char('m'),
        Action::ShowSettings,
        "m",
        "settings",
        "General",
    ),
    global(
        KeyCode::Char('f'),
        Action::FocusPlaylist,
        "f",
        "focus playlist",
        "Playlists",
    ),
    global(
        KeyCode::Char('p'),
        Action::AddToPlaylist,
        "p",
        "add to playlist",
        "Playlists",
    ),
    global(
        KeyCode::Tab,
        Action::FocusNext,
        "tab",
        "switch pane",
        "General",
    ),
    Binding {
        modifiers: CTRL,
        ..global(
            KeyCode::Char('s'),
            Action::SaveCatalog,
            "ctrl+s",
            "save playlists",
            "Playlists",
        )
    },
    Binding {
        modifiers: CTRL,
        ..global(
            KeyCode::Char(' '),
            Action::TogglePause,
            "ctrl+space",
            "play/pause",
            "Playback",
        )
    },
    global(
        KeyCode::Char(' '),
        Action::TogglePause,
        "space",
        "play/pause",
        "Playback",
    ),
    global(KeyCode::Char('s'), Action::Stop, "s", "stop", "Playback"),
    global(
        KeyCode::Char('n'),
        Action::NextTrack,
        "n",
        "next",
        "Playback",
    ),
    global(
        KeyCode::Left,
        Action::SeekBack,
        "←",
        "seek -10s",
        "Playback",
    ),
    global(
        KeyCode::Right,
        Action::SeekForward,
        "→",
        "seek +10s",
        "Playback",
    ),
    global(
        KeyCode::Char('r'),
        Action::ToggleShuffle,
        "r",
        "shuffle",
        "Playback",
    ),
    global(
        KeyCode::Char('d'),
        Action::OpenOutputSettings,
        "d",
        "output",
        "Settings",
    ),
    global(
        KeyCode::Char('g'),
        Action::ToggleSharedPlayback,
        "g",
        "shared/exclusive",
        "Settings",
    ),
    global(
        KeyCode::Char('a'),
        Action::EnqueueSelected,
        "a",
        "queue item",
        "Library",
    ),
    global(
        KeyCode::Char('A'),
        Action::EnqueueAlbum,
        "A",
        "queue album",
        "Library",
    ),
    global(
        KeyCode::Enter,
        Action::Activate,
        "enter",
        "open/play",
        "General",
    ),
    global(
        KeyCode::Backspace,
        Action::BrowserUp,
        "bksp",
        "up",
        "Library",
    ),
    Binding {
        footer: false,
        ..global(
            KeyCode::Char('c'),
            Action::ToggleFloatToInt,
            "c",
            "F32→integer compatibility (not bit-perfect)",
            "Settings",
        )
    },
    Binding {
        footer: false,
        ..global(
            KeyCode::Char('i'),
            Action::ToggleIntToFloat,
            "i",
            "integer→F32 compatibility (not bit-perfect)",
            "Settings",
        )
    },
];

/// Resolve a key in Normal mode: pane-specific bindings first, then
/// globals. Returns `None` for unbound keys.
pub fn lookup(focus: Focus, key: KeyEvent) -> Option<Action> {
    let mut global_hit = None;
    for binding in BINDINGS {
        if binding.code != key.code || binding.modifiers != key.modifiers {
            continue;
        }
        match binding.scope {
            Scope::On(scope) if scope == focus => return Some(binding.action),
            Scope::On(_) => {}
            Scope::In(_) => {}
            Scope::Global => global_hit = global_hit.or(Some(binding.action)),
        }
    }
    global_hit
}

/// Resolve a special key while an overlay owns input. Printable text keys
/// not listed here remain text in Search/Prompt; the app handles their
/// characters after this lookup.
pub fn lookup_mode(mode: ModeKind, key: KeyEvent) -> Option<Action> {
    BINDINGS
        .iter()
        .find(|binding| {
            binding.scope == Scope::In(mode)
                && binding.code == key.code
                && binding.modifiers == key.modifiers
        })
        .map(|binding| binding.action)
}

/// Footer keycaps for a context: applicable bindings in table order,
/// capped so the line stays short; `? all keys` is appended by the caller.
pub fn footer_pairs(focus: Focus, mode: ModeKind) -> Vec<(&'static str, &'static str)> {
    let mut pairs = Vec::new();
    let mut push = |key: &'static str, label: &'static str| {
        if pairs.len() < 8 && !pairs.contains(&(key, label)) {
            pairs.push((key, label));
        }
    };
    match mode {
        ModeKind::Normal => {
            // Order by local utility, not table position. Labels and keys
            // still come from the dispatch table, including pane overrides.
            let actions: &[Action] = match focus {
                Focus::Library => &[
                    Action::Activate,
                    Action::EnqueueSelected,
                    Action::EnqueueAlbum,
                    Action::AddToPlaylist,
                    Action::OpenSearch,
                    Action::FocusNext,
                    Action::TogglePause,
                    Action::ToggleHelp,
                ],
                Focus::Queue => &[
                    Action::Activate,
                    Action::QueueRemove,
                    Action::MoveStart,
                    Action::FocusNext,
                    Action::TogglePause,
                    Action::ToggleShuffle,
                    Action::OpenSearch,
                    Action::ToggleHelp,
                ],
                Focus::PlaylistNames => &[
                    Action::NewPlaylist,
                    Action::Activate,
                    Action::FocusNext,
                    Action::EnqueueSelected,
                    Action::SaveCatalog,
                    Action::TogglePause,
                    Action::OpenActions,
                    Action::ToggleHelp,
                ],
                Focus::PlaylistTracks => &[
                    Action::ToggleSelect,
                    Action::RangeSelect,
                    Action::MoveStart,
                    Action::RemoveRows,
                    Action::EnqueueSelection,
                    Action::SaveCatalog,
                    Action::TogglePause,
                    Action::ToggleHelp,
                ],
                Focus::SettingsSections | Focus::SettingsRows => &[
                    Action::Activate,
                    Action::ResetRow,
                    Action::ToggleSetting,
                    Action::FocusNext,
                    Action::OpenLocations,
                    Action::TogglePause,
                    Action::ToggleHelp,
                ],
            };
            for action in actions {
                if let Some(binding) = BINDINGS
                    .iter()
                    .find(|binding| {
                        binding.footer
                            && binding.action == *action
                            && matches!(binding.scope, Scope::On(scope) if scope == focus)
                    })
                    .or_else(|| {
                        let pause_key =
                            if matches!(focus, Focus::PlaylistTracks | Focus::SettingsRows) {
                                "ctrl+space"
                            } else {
                                "space"
                            };
                        BINDINGS.iter().find(|binding| {
                            binding.footer
                                && binding.action == *action
                                && binding.scope == Scope::Global
                                && (*action != Action::TogglePause || binding.key == pause_key)
                        })
                    })
                {
                    push(binding.key, binding.label);
                }
            }
        }
        _ => {
            let actions: &[Action] = match mode {
                ModeKind::Search => &[
                    Action::FocusNext,
                    Action::Activate,
                    Action::EnqueueSelected,
                    Action::AddToPlaylist,
                    Action::OpenFolder,
                    Action::CloseOverlay,
                ],
                ModeKind::Move => &[
                    Action::CursorUp,
                    Action::CursorDown,
                    Action::Activate,
                    Action::CloseOverlay,
                ],
                ModeKind::Prompt => &[
                    Action::Activate,
                    Action::DeleteChar,
                    Action::CloseOverlay,
                    Action::Quit,
                ],
                ModeKind::QuitConfirm => {
                    &[Action::ConfirmYes, Action::ConfirmNo, Action::CloseOverlay]
                }
                ModeKind::Help => &[Action::CursorUp, Action::CursorDown, Action::CloseOverlay],
                ModeKind::Locations => {
                    &[Action::Activate, Action::OpenActions, Action::CloseOverlay]
                }
                ModeKind::PlaylistPicker => {
                    &[Action::Activate, Action::NewPlaylist, Action::CloseOverlay]
                }
                ModeKind::Actions => &[Action::Activate, Action::CloseOverlay],
                ModeKind::Normal => unreachable!(),
            };
            for action in actions {
                if let Some(binding) = BINDINGS
                    .iter()
                    .find(|binding| {
                        binding.footer
                            && binding.scope == Scope::In(mode)
                            && binding.action == *action
                    })
                    .or_else(|| {
                        BINDINGS.iter().find(|binding| {
                            mode == ModeKind::Prompt
                                && binding.scope == Scope::Global
                                && binding.key == "ctrl+q"
                                && binding.action == *action
                        })
                    })
                {
                    push(binding.key, binding.label);
                }
            }
        }
    }
    pairs
}

/// Help groups in display order: `(group, (key, label) rows)`. Driven by
/// the same table as dispatch; hidden shortcuts appear here.
pub fn help_groups() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    let mut groups: Vec<(&'static str, Vec<(&'static str, &'static str)>)> = Vec::new();
    for binding in BINDINGS {
        let row = (binding.key, binding.label);
        match groups.iter_mut().find(|(name, _)| *name == binding.group) {
            Some((_, rows)) if !rows.contains(&row) => rows.push(row),
            Some(_) => {}
            None => groups.push((binding.group, vec![row])),
        }
    }
    // Fixed, calm order.
    const ORDER: &[&str] = &[
        "General",
        "Playback",
        "Library",
        "Queue",
        "Search",
        "Playlists",
        "Playlist picker",
        "Move",
        "Locations",
        "Settings",
        "Input",
        "Actions",
    ];
    groups.sort_by_key(|(name, _)| {
        ORDER
            .iter()
            .position(|group| group == name)
            .unwrap_or(ORDER.len())
    });
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn pane_bindings_override_globals() {
        // d removes in Queue/playlist tracks, opens Output elsewhere.
        assert_eq!(
            lookup(Focus::Queue, key(KeyCode::Char('d'))),
            Some(Action::QueueRemove)
        );
        assert_eq!(
            lookup(Focus::PlaylistTracks, key(KeyCode::Char('d'))),
            Some(Action::RemoveRows)
        );
        assert_eq!(
            lookup(Focus::Library, key(KeyCode::Char('d'))),
            Some(Action::OpenOutputSettings)
        );
        // space selects tracks in playlist tracks, pauses elsewhere.
        assert_eq!(
            lookup(Focus::PlaylistTracks, key(KeyCode::Char(' '))),
            Some(Action::ToggleSelect)
        );
        assert_eq!(
            lookup(Focus::Library, key(KeyCode::Char(' '))),
            Some(Action::TogglePause)
        );
        // n creates a playlist in names, is next-track elsewhere.
        assert_eq!(
            lookup(Focus::PlaylistNames, key(KeyCode::Char('n'))),
            Some(Action::NewPlaylist)
        );
        assert_eq!(
            lookup(Focus::Library, key(KeyCode::Char('n'))),
            Some(Action::NextTrack)
        );
        // r renames in tracks, resets in settings, shuffles elsewhere.
        assert_eq!(
            lookup(Focus::PlaylistTracks, key(KeyCode::Char('r'))),
            Some(Action::RenamePlaylist)
        );
        assert_eq!(
            lookup(Focus::SettingsRows, key(KeyCode::Char('r'))),
            Some(Action::ResetRow)
        );
        assert_eq!(
            lookup(Focus::Library, key(KeyCode::Char('r'))),
            Some(Action::ToggleShuffle)
        );
        // m moves in Queue/playlist tracks, opens settings elsewhere.
        assert_eq!(
            lookup(Focus::Queue, key(KeyCode::Char('m'))),
            Some(Action::MoveStart)
        );
        assert_eq!(
            lookup(Focus::Library, key(KeyCode::Char('m'))),
            Some(Action::ShowSettings)
        );
    }

    #[test]
    fn ctrl_space_pauses_even_where_space_selects() {
        let key = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert_eq!(
            lookup(Focus::PlaylistTracks, key),
            Some(Action::TogglePause)
        );
    }

    #[test]
    fn footer_hides_compatibility_shortcuts_but_help_lists_them() {
        for focus in [Focus::Library, Focus::Queue, Focus::PlaylistTracks] {
            let pairs = footer_pairs(focus, ModeKind::Normal);
            assert!(!pairs.iter().any(|(key, _)| *key == "c" || *key == "i"));
            assert!(pairs.len() <= 8);
            assert!(pairs.iter().any(|(_, label)| label.contains("play/pause")));
        }
        let help = help_groups();
        let settings = help.iter().find(|(name, _)| *name == "Settings").unwrap();
        assert!(settings.1.iter().any(|(key, _)| *key == "c"));
        assert!(settings.1.iter().any(|(key, _)| *key == "i"));
    }

    #[test]
    fn every_dispatched_binding_appears_in_help() {
        let help = help_groups();
        for binding in BINDINGS {
            let (_, rows) = help
                .iter()
                .find(|(name, _)| *name == binding.group)
                .expect("group exists");
            assert!(
                rows.contains(&(binding.key, binding.label)),
                "missing help row for {}",
                binding.label
            );
        }
    }
}
