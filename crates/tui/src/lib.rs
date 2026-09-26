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
use sointty_core::{PlayerCommand, PlayerEvent, QueueEntry, TrackId, TrackTags};

mod browser;

use browser::{Browser, BrowserItemKind};

pub struct TuiState {
    pub status: String,
    pub output: String,
    pub frame: u64,
    pub rate_hz: u32,
    pub paused: bool,
    pub tags: TrackTags,
    current_track: TrackId,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            status: "idle".to_owned(),
            output: "not configured".to_owned(),
            frame: 0,
            rate_hz: 44_100,
            paused: false,
            tags: TrackTags::default(),
            current_track: 0,
        }
    }
}

pub fn run(
    commands: Sender<PlayerCommand>,
    events: Receiver<PlayerEvent>,
    open_browser: bool,
) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = event_loop(&mut terminal, commands, events, open_browser);
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
) -> io::Result<()> {
    let mut state = TuiState::default();
    let mut browser = if open_browser {
        Browser::open(std::env::current_dir()?).ok()
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
            terminal.draw(|frame| draw(frame, &state, browser.as_ref()))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(pane) = browser.as_mut() {
                match key.code {
                    KeyCode::Char('q') => {
                        let _ = commands.send(PlayerCommand::Quit);
                        return Ok(());
                    }
                    KeyCode::Up | KeyCode::Char('k') => pane.move_selection(-1),
                    KeyCode::Down | KeyCode::Char('j') => pane.move_selection(1),
                    KeyCode::Enter => {
                        if let Some(item) = pane.selected_item() {
                            match item.kind {
                                BrowserItemKind::Dir => {
                                    if let Err(error) = pane.descend(item.path.clone()) {
                                        state.status = format!("sointty: {error}");
                                    }
                                }
                                BrowserItemKind::Audio => {
                                    let _ = commands.send(PlayerCommand::Enqueue(
                                        QueueEntry::from(item.path.clone()),
                                    ));
                                }
                                BrowserItemKind::Playlist => {
                                    match sointty_playlist::read(&item.path) {
                                        Ok(entries) => {
                                            let count = entries.len();
                                            for entry in entries {
                                                let _ = commands.send(
                                                    PlayerCommand::Enqueue(QueueEntry {
                                                        path: entry.path,
                                                        cue_range: entry.cue_range,
                                                    }),
                                                );
                                            }
                                            state.status = format!("enqueued {count} track(s)");
                                        }
                                        Err(error) => {
                                            state.status = format!("sointty: {error}");
                                        }
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        if let Err(error) = pane.ascend() {
                            state.status = format!("sointty: {error}");
                        }
                    }
                    KeyCode::Char('b') | KeyCode::Esc => browser = None,
                    _ => {}
                }
                dirty = true;
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    let _ = commands.send(PlayerCommand::Quit);
                    return Ok(());
                }
                KeyCode::Char(' ') => {
                    let command = if state.paused {
                        PlayerCommand::Play
                    } else {
                        PlayerCommand::Pause
                    };
                    state.paused = !state.paused;
                    let _ = commands.send(command);
                }
                KeyCode::Char('n') => {
                    let _ = commands.send(PlayerCommand::Next);
                }
                KeyCode::Char('b') => {
                    match Browser::open(std::env::current_dir()?) {
                        Ok(pane) => browser = Some(pane),
                        Err(error) => state.status = format!("sointty: {error}"),
                    }
                }
                _ => {}
            }
            dirty = true;
        }
    }
}

fn apply_event(state: &mut TuiState, event: PlayerEvent) {
    match event {
        PlayerEvent::Playing { track, output } => {
            state.status = "playing".to_owned();
            state.paused = false;
            state.tags = TrackTags::default();
            state.current_track = track;
            state.rate_hz = output.rate_hz;
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
        }
        PlayerEvent::Underrun { .. } => state.status = "underrun".to_owned(),
        PlayerEvent::Stalled { .. } => state.status = "stalled".to_owned(),
        PlayerEvent::Error { kind, .. } => state.status = format!("error: {kind}"),
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

fn draw(frame: &mut ratatui::Frame<'_>, state: &TuiState, browser: Option<&Browser>) {
    let bottom = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Min(3),
        ])
        .split(frame.area());
    let title = Paragraph::new("sointty — bit-perfect terminal player")
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, bottom[0]);

    let track = metadata_line(&state.tags);
    let output = Paragraph::new(format!(
        "status: {}{}\noutput: {}",
        state.status,
        if track.is_empty() {
            String::new()
        } else {
            format!("\ntrack: {track}")
        },
        state.output
    ))
    .block(Block::default().title("Playback").borders(Borders::ALL));
    frame.render_widget(output, bottom[1]);

    let seconds = state.frame / u64::from(state.rate_hz);
    let text = format!(
        "{} frames (~{}:{:02})\nspace: pause/play   n: next   b: browser   q/Esc: quit",
        state.frame,
        seconds / 60,
        seconds % 60
    );
    let position =
        Paragraph::new(text).block(Block::default().title("Position").borders(Borders::ALL));

    if let Some(pane) = browser {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(bottom[2]);
        let items: Vec<ListItem> = pane
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
                    .title(format!("Browser — {}", pane.current_dir.display()))
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        let mut list_state = ListState::default();
        if !pane.items.is_empty() {
            list_state.select(Some(pane.selected));
        }
        frame.render_stateful_widget(list, columns[0], &mut list_state);
        frame.render_widget(position, columns[1]);
    } else {
        frame.render_widget(position, bottom[2]);
    }
}
