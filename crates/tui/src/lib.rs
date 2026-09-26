use std::io;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use sointty_core::{DeviceId, PlayerCommand, PlayerEvent, QueueEntry, TrackId, TrackTags};

mod browser;

use browser::{Browser, BrowserItemKind};

/// Lists selectable output devices as `(id, display name)` pairs; invoked
/// each time the device pane opens so hot-plugged endpoints appear.
pub type DeviceProvider = Box<dyn FnMut() -> Vec<(DeviceId, String)>>;
/// Persists a device selection; the error text is shown in the status line.
pub type DeviceSaver = Box<dyn FnMut(&DeviceId) -> Result<(), String>>;
/// Persists an explicit non-bit-perfect F32-to-integer compatibility choice.
pub type FloatCompatibilitySaver = Box<dyn FnMut(bool) -> Result<(), String>>;

/// Seek step for the Left/Right arrow keys.
const SEEK_SECONDS: u64 = 10;

pub struct TuiState {
    pub status: String,
    pub output: String,
    pub frame: u64,
    pub rate_hz: u32,
    pub paused: bool,
    pub tags: TrackTags,
    current_track: TrackId,
    /// Active output device; updated from `Playing` events and the picker.
    device: DeviceId,
    float_to_int: bool,
    /// Whether the last established output used F32-to-integer conversion.
    converted: bool,
}

impl TuiState {
    fn new(device: DeviceId, float_to_int: bool) -> Self {
        Self {
            status: "idle".to_owned(),
            output: "not configured".to_owned(),
            frame: 0,
            rate_hz: 44_100,
            paused: false,
            tags: TrackTags::default(),
            current_track: 0,
            device,
            float_to_int,
            converted: false,
        }
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

/// At most one pane is open at a time; opening one replaces the other.
enum Pane {
    Browser(Browser),
    Devices(DevicePane),
}

pub fn run(
    commands: Sender<PlayerCommand>,
    events: Receiver<PlayerEvent>,
    open_browser: bool,
    device_provider: DeviceProvider,
    current_device: DeviceId,
    on_device_selected: DeviceSaver,
    float_to_int: bool,
    on_float_to_int_selected: FloatCompatibilitySaver,
) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = event_loop(
        &mut terminal,
        commands,
        events,
        open_browser,
        device_provider,
        current_device,
        on_device_selected,
        float_to_int,
        on_float_to_int_selected,
    );
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    commands: Sender<PlayerCommand>,
    events: Receiver<PlayerEvent>,
    open_browser: bool,
    mut device_provider: DeviceProvider,
    current_device: DeviceId,
    mut on_device_selected: DeviceSaver,
    float_to_int: bool,
    mut on_float_to_int_selected: FloatCompatibilitySaver,
) -> io::Result<()> {
    let mut state = TuiState::new(current_device, float_to_int);
    let mut pane = if open_browser {
        Browser::open(std::env::current_dir()?).ok().map(Pane::Browser)
    } else {
        None
    };
    let mut dirty = true;
    loop {
        while let Ok(event) = events.try_recv() {
            apply_event(&mut state, event);
            dirty = true;
        }
        if dirty {
            terminal.draw(|frame| draw(frame, &state, pane.as_ref()))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if handle_key(
                key.code,
                &mut state,
                &mut pane,
                &commands,
                &mut device_provider,
                &mut on_device_selected,
                &mut on_float_to_int_selected,
            )? {
                return Ok(());
            }
            dirty = true;
        }
    }
}

/// Handle one key press. Returns `true` when the UI should quit.
///
/// Playback keys are global: they work no matter which pane is open, so the
/// browser no longer swallows play/pause/seek/stop/next.
fn handle_key(
    key: KeyCode,
    state: &mut TuiState,
    pane: &mut Option<Pane>,
    commands: &Sender<PlayerCommand>,
    device_provider: &mut DeviceProvider,
    on_device_selected: &mut DeviceSaver,
    on_float_to_int_selected: &mut FloatCompatibilitySaver,
) -> io::Result<bool> {
    match key {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Esc => {
            // Esc closes an open pane first; with no pane it quits.
            if pane.take().is_none() {
                return Ok(true);
            }
            return Ok(false);
        }
        KeyCode::Char(' ') => {
            let command = if state.paused {
                PlayerCommand::Play
            } else {
                PlayerCommand::Pause
            };
            state.paused = !state.paused;
            let _ = commands.send(command);
            return Ok(false);
        }
        KeyCode::Char('s') => {
            state.paused = true;
            let _ = commands.send(PlayerCommand::Stop);
            return Ok(false);
        }
        KeyCode::Char('n') => {
            let _ = commands.send(PlayerCommand::Next);
            return Ok(false);
        }
        KeyCode::Left => {
            seek_by(state, commands, -(SEEK_SECONDS as i64));
            return Ok(false);
        }
        KeyCode::Right => {
            seek_by(state, commands, SEEK_SECONDS as i64);
            return Ok(false);
        }
        KeyCode::Char('b') => {
            toggle_browser(state, pane)?;
            return Ok(false);
        }
        KeyCode::Char('d') => {
            toggle_devices(state, pane, device_provider);
            return Ok(false);
        }
        KeyCode::Char('c') => {
            let enabled = !state.float_to_int;
            let _ = commands.send(PlayerCommand::SetFloatToInt(enabled));
            state.float_to_int = enabled;
            if let Err(error) = on_float_to_int_selected(enabled) {
                state.status = format!("F32 compatibility not saved: {error}");
            }
            return Ok(false);
        }
        _ => {}
    }
    let Some(active) = pane else {
        return Ok(false);
    };
    match active {
        Pane::Browser(browser) => match key {
            KeyCode::Up | KeyCode::Char('k') => browser.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => browser.move_selection(1),
            KeyCode::Backspace => {
                if let Err(error) = browser.ascend() {
                    state.status = format!("sointty: {error}");
                }
            }
            KeyCode::Enter => {
                if let Some(item) = browser.selected_item() {
                    match item.kind {
                        BrowserItemKind::Dir => {
                            if let Err(error) = browser.descend(item.path.clone()) {
                                state.status = format!("sointty: {error}");
                            }
                        }
                        BrowserItemKind::Audio => {
                            let _ = commands.send(PlayerCommand::Enqueue(QueueEntry::from(
                                item.path.clone(),
                            )));
                            let _ = commands.send(PlayerCommand::Play);
                            state.paused = false;
                            state.status = format!("queued {}", item.name);
                        }
                        BrowserItemKind::Playlist => match sointty_playlist::read(&item.path) {
                            Ok(entries) => {
                                let count = entries.len();
                                for entry in entries {
                                    let _ = commands.send(PlayerCommand::Enqueue(QueueEntry {
                                        path: entry.path,
                                        cue_range: entry.cue_range,
                                    }));
                                }
                                let _ = commands.send(PlayerCommand::Play);
                                state.paused = false;
                                state.status = format!("queued {count} track(s)");
                            }
                            Err(error) => {
                                state.status = format!("sointty: {error}");
                            }
                        },
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
                    let _ = commands.send(PlayerCommand::SelectDevice(id.clone()));
                    match on_device_selected(&id) {
                        Ok(()) => state.status = format!("output device: {name}"),
                        Err(error) => {
                            state.status = format!("device selected but not saved: {error}");
                        }
                    }
                    state.device = id;
                    *pane = None;
                }
            }
            _ => {}
        },
    }
    Ok(false)
}

fn seek_by(state: &TuiState, commands: &Sender<PlayerCommand>, seconds: i64) {
    let target = state.frame as i64 + seconds * i64::from(state.rate_hz);
    let _ = commands.send(PlayerCommand::SeekFrame(target.max(0) as u64));
}

fn toggle_browser(state: &mut TuiState, pane: &mut Option<Pane>) -> io::Result<()> {
    if matches!(pane, Some(Pane::Browser(_))) {
        *pane = None;
        return Ok(());
    }
    match Browser::open(std::env::current_dir()?) {
        Ok(browser) => *pane = Some(Pane::Browser(browser)),
        Err(error) => state.status = format!("sointty: {error}"),
    }
    Ok(())
}

fn toggle_devices(
    state: &mut TuiState,
    pane: &mut Option<Pane>,
    device_provider: &mut DeviceProvider,
) {
    if matches!(pane, Some(Pane::Devices(_))) {
        *pane = None;
        return;
    }
    let devices = device_provider();
    if devices.is_empty() {
        state.status = "no output devices found".to_owned();
        return;
    }
    let selected = devices
        .iter()
        .position(|(id, _)| *id == state.device)
        .unwrap_or(0);
    *pane = Some(Pane::Devices(DevicePane { devices, selected }));
}

fn apply_event(state: &mut TuiState, event: PlayerEvent) {
    match event {
        PlayerEvent::Playing { track, output, converted } => {
            state.status = if converted {
                "playing (F32 converted; not bit-perfect)".to_owned()
            } else {
                "playing (bit-perfect)".to_owned()
            };
            state.paused = false;
            state.tags = TrackTags::default();
            state.current_track = track;
            state.rate_hz = output.rate_hz;
            state.converted = converted;
            state.device = output.device.clone();
            state.output = format!(
                "{} Hz / {} ch / {:?} ({} valid bits)",
                output.rate_hz, output.layout.channels, output.format, output.valid_bits
            );
        }
        PlayerEvent::Tags { track, tags } => {
            if track == state.current_track {
                state.tags = tags;
            }
        }
        PlayerEvent::Position { frame, .. } => state.frame = frame,
        PlayerEvent::Paused => {
            state.status = "paused".to_owned();
            state.paused = true;
        }
        PlayerEvent::Reconfiguring => {
            state.status = "reconfiguring device".to_owned();
            state.tags = TrackTags::default();
            state.output = "not configured".to_owned();
            state.converted = false;
        }
        PlayerEvent::Underrun { .. } => state.status = "underrun".to_owned(),
        PlayerEvent::Stalled { .. } => state.status = "stalled".to_owned(),
        PlayerEvent::Error { kind, .. } => {
            state.status = format!("error: {kind}");
            // No stream is playing after a command-path failure. Space must
            // start the next queued track, not send another Pause.
            state.paused = true;
        }
        PlayerEvent::EndOfQueue => {
            state.status = "end of queue".to_owned();
            state.paused = false;
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

fn draw(frame: &mut ratatui::Frame<'_>, state: &TuiState, pane: Option<&Pane>) {
    let bottom = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(8),
            Constraint::Min(4),
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

    let track = metadata_line(&state.tags);
    let fidelity = if state.output == "not configured" {
        "not configured"
    } else if state.converted {
        "converted F32→integer (NOT bit-perfect)"
    } else {
        "decoder PCM preserved (bit-perfect)"
    };
    let output = Paragraph::new(format!(
        "status: {}{}\ndevice: {}\noutput: {}\nfidelity: {}\nF32→integer compatibility: {} (c toggles)",
        state.status,
        if track.is_empty() {
            String::new()
        } else {
            format!("\ntrack: {track}")
        },
        state.device,
        state.output,
        fidelity,
        if state.float_to_int { "ON (non-bit-perfect when used)" } else { "OFF (strict)" }
    ))
    .block(Block::default().title("Playback").borders(Borders::ALL));
    frame.render_widget(output, bottom[1]);

    let seconds = state.frame / u64::from(state.rate_hz);
    let text = format!(
        "{} frames (~{}:{:02})\nspace play/pause  ←/→ seek {SEEK_SECONDS}s  s stop  n next\nb browser  d output  c F32→int  q quit (Esc closes pane)",
        state.frame,
        seconds / 60,
        seconds % 60
    );
    let position =
        Paragraph::new(text).block(Block::default().title("Position").borders(Borders::ALL));

    match pane {
        Some(Pane::Browser(browser)) => {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                .split(bottom[2]);
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
                            "Browser — {} (Enter: play, Backspace: up)",
                            browser.current_dir.display()
                        ))
                        .borders(Borders::ALL),
                )
                .highlight_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("> ");
            let mut list_state = ListState::default();
            if !browser.items.is_empty() {
                list_state.select(Some(browser.selected));
            }
            frame.render_stateful_widget(list, columns[0], &mut list_state);
            frame.render_widget(position, columns[1]);
        }
        Some(Pane::Devices(devices)) => {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                .split(bottom[2]);
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
                .highlight_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("> ");
            let mut list_state = ListState::default();
            if !devices.devices.is_empty() {
                list_state.select(Some(devices.selected));
            }
            frame.render_stateful_widget(list, columns[0], &mut list_state);
            frame.render_widget(position, columns[1]);
        }
        None => {
            frame.render_widget(position, bottom[2]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Harness {
        state: TuiState,
        pane: Option<Pane>,
        commands: Sender<PlayerCommand>,
        sent: Receiver<PlayerCommand>,
        saved: Arc<Mutex<Vec<DeviceId>>>,
        saver: DeviceSaver,
        provider: DeviceProvider,
        saved_float: Arc<Mutex<Vec<bool>>>,
        float_saver: FloatCompatibilitySaver,
    }

    fn harness(devices: Vec<(DeviceId, String)>) -> Harness {
        let (commands, sent) = crossbeam_channel::unbounded();
        let saved = Arc::new(Mutex::new(Vec::new()));
        let saved_sink = Arc::clone(&saved);
        let saved_float = Arc::new(Mutex::new(Vec::new()));
        let float_sink = Arc::clone(&saved_float);
        Harness {
            state: TuiState::new("dev1".to_owned(), false),
            pane: None,
            commands,
            sent,
            saved,
            saved_float,
            float_saver: Box::new(move |enabled| {
                float_sink.lock().unwrap().push(enabled);
                Ok(())
            }),
            saver: Box::new(move |device| {
                saved_sink.lock().unwrap().push(device.clone());
                Ok(())
            }),
            provider: Box::new(move || devices.clone()),
        }
    }

    impl Harness {
        fn key(&mut self, key: KeyCode) -> bool {
            handle_key(
                key,
                &mut self.state,
                &mut self.pane,
                &self.commands,
                &mut self.provider,
                &mut self.saver,
                &mut self.float_saver,
            )
            .unwrap()
        }
    }

    fn audio_browser() -> (std::path::PathBuf, Browser) {
        let dir = std::env::temp_dir().join(format!("sointty-tui-test-{}", std::process::id()));
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
        h.pane = Some(Pane::Browser(browser));
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
        h.pane = Some(Pane::Browser(browser));
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
        h.state.frame = 60 * 44_100;
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
        h.state.frame = 5 * 44_100;
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
        assert_eq!(h.saved.lock().unwrap().as_slice(), &["dev2".to_owned()]);
        assert_eq!(h.state.device, "dev2");
        assert!(h.pane.is_none());
    }

    #[test]
    fn esc_closes_pane_before_quitting() {
        let mut h = harness(vec![("dev1".to_owned(), "DAC".to_owned())]);
        h.key(KeyCode::Char('d'));
        assert!(h.pane.is_some());
        assert!(!h.key(KeyCode::Esc));
        assert!(h.pane.is_none());
        assert!(h.key(KeyCode::Esc));
    }

    #[test]
    fn saver_error_is_reported_but_selection_still_applies() {
        let (commands, sent) = crossbeam_channel::unbounded();
        let mut state = TuiState::new("dev1".to_owned(), false);
        let mut pane = None;
        let mut provider: DeviceProvider =
            Box::new(|| vec![("dev2".to_owned(), "USB DAC".to_owned())]);
        let mut saver: DeviceSaver = Box::new(|_| Err("disk full".to_owned()));
        let mut float_saver: FloatCompatibilitySaver = Box::new(|_| Ok(()));
        handle_key(
            KeyCode::Char('d'),
            &mut state,
            &mut pane,
            &commands,
            &mut provider,
            &mut saver,
            &mut float_saver,
        )
        .unwrap();
        handle_key(
            KeyCode::Enter,
            &mut state,
            &mut pane,
            &commands,
            &mut provider,
            &mut saver,
            &mut float_saver,
        )
        .unwrap();
        let command = sent.recv().unwrap();
        assert!(matches!(&command, PlayerCommand::SelectDevice(id) if id == "dev2"));
        assert_eq!(state.device, "dev2");
        assert!(state.status.contains("not saved"));
    }

    #[test]
    fn space_retries_queue_after_playback_error() {
        let mut h = harness(Vec::new());
        apply_event(&mut h.state, PlayerEvent::Error {
            track: Some(1),
            kind: sointty_core::PlayerError::DeviceBusy,
        });
        h.key(KeyCode::Char(' '));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::Play));
    }

    #[test]
    fn compatibility_toggle_is_explicit_and_saved() {
        let mut h = harness(Vec::new());
        assert!(!h.state.float_to_int);
        h.key(KeyCode::Char('c'));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::SetFloatToInt(true)));
        assert!(h.state.float_to_int);
        h.key(KeyCode::Char('c'));
        assert!(matches!(h.sent.recv().unwrap(), PlayerCommand::SetFloatToInt(false)));
        assert!(!h.state.float_to_int);
        assert_eq!(*h.saved_float.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn converted_playback_is_labeled_non_bit_perfect() {
        let mut state = TuiState::new("dev1".to_owned(), true);
        apply_event(
            &mut state,
            PlayerEvent::Playing {
                track: 1,
                output: sointty_core::OutputSpec {
                    device: "dev1".to_owned(),
                    rate_hz: 44_100,
                    layout: sointty_core::ChannelLayout::discrete(2),
                    format: sointty_core::DeviceFormat::S32Le,
                    valid_bits: 32,
                },
                converted: true,
            },
        );
        assert!(state.status.contains("not bit-perfect"));
        apply_event(&mut state, PlayerEvent::EndOfQueue);
        assert!(state.converted, "last output must still be labeled converted");
    }
}
