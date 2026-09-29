mod config;
mod layout;
mod player;
mod scanner;

use config::{load_config, save_config, AppConfig};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use layout::{compute_layout, format_time, render_bar};
use player::Player;
use rand::Rng;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout as RLayout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame, Terminal,
};
use scanner::{scan_music_folder, Track};
use std::{
    io::{self, Stdout},
    path::Path,
    sync::{mpsc::{self, Receiver, TryRecvError}, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const GREEN: Color = Color::Green;
const BRIGHT_GREEN: Color = Color::LightGreen;

/// Verifies that a folder path exists and is a directory.
fn valid_folder(folder: &str) -> bool {
    Path::new(folder).is_dir()
}

/// Standardized rounded panel block for the terminal UI.
fn rounded_block<'a>(title: &'a str) -> Block<'a> {
    Block::default()
        .title(Span::styled(title, Style::default().fg(GREEN).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(GREEN))
}

/// Shortens long text so it fits cleanly inside a fixed-width panel.
fn truncate(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width { return s.to_string(); }
    if width <= 1 { return "…".to_string(); }
    format!("{}…", s.chars().take(width - 1).collect::<String>())
}

/// The startup screen that asks the user for a music library folder.
struct Splash {
    value: String,
    cursor: usize,
    error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splash_inserts_char_at_cursor() {
        let mut splash = Splash::new(String::new());
        splash.handle_key(KeyCode::Char('a'), false);
        splash.handle_key(KeyCode::Char('b'), false);
        splash.handle_key(KeyCode::Left, false);
        splash.handle_key(KeyCode::Char('x'), false);
        assert_eq!(splash.value, "axb");
        assert_eq!(splash.cursor, 2);
    }

    #[test]
    fn splash_backspace_removes_before_cursor() {
        let mut splash = Splash::new("abcd".into());
        splash.cursor = 3;
        splash.handle_key(KeyCode::Backspace, false);
        assert_eq!(splash.value, "acd");
        assert_eq!(splash.cursor, 2);
    }
}

impl Splash {
    /// Creates a new folder prompt with an initial value.
    fn new(value: String) -> Self { Self { value: value.clone(), cursor: value.chars().count(), error: None } }

    fn insert_char(&mut self, ch: char) {
        let mut chars: Vec<char> = self.value.chars().collect();
        let index = self.cursor.min(chars.len());
        chars.insert(index, ch);
        self.value = chars.into_iter().collect();
        self.cursor = index + 1;
        self.error = None;
    }

    fn insert_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.insert_char(ch);
        }
    }

    fn delete_before_cursor(&mut self) {
        if self.cursor == 0 { return; }
        let mut chars: Vec<char> = self.value.chars().collect();
        let remove_at = self.cursor.saturating_sub(1).min(chars.len());
        chars.remove(remove_at);
        self.value = chars.into_iter().collect();
        self.cursor = remove_at;
        self.error = None;
    }

    fn delete_at_cursor(&mut self) {
        let mut chars: Vec<char> = self.value.chars().collect();
        if self.cursor < chars.len() {
            chars.remove(self.cursor);
            self.value = chars.into_iter().collect();
            self.error = None;
        }
    }

    fn handle_key(&mut self, code: KeyCode, can_cancel: bool) -> SplashAction {
        match code {
            KeyCode::Enter => {
                let folder = self.value.trim();
                if folder.is_empty() {
                    self.error = Some("Enter a folder path".into());
                } else if !valid_folder(folder) {
                    self.error = Some("That folder doesn't exist".into());
                } else {
                    return SplashAction::Submit(folder.to_string());
                }
            }
            KeyCode::Esc if can_cancel => return SplashAction::Cancel,
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.value.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.chars().count(),
            KeyCode::Backspace => self.delete_before_cursor(),
            KeyCode::Delete => self.delete_at_cursor(),
            KeyCode::Char(c) => self.insert_char(c),
            _ => {}
        }
        SplashAction::None
    }
}

/// Result of handling a key while the user is typing a music folder path.
enum SplashAction { None, Submit(String), Cancel }

/// Messages sent from the scan thread to the UI thread.
enum ScanMessage {
    Progress(usize, usize),
    Finished(Vec<Track>),
}

type ScanReceiver = Receiver<ScanMessage>;

/// The main music library state and player controls.
struct Library {
    music_folder: String,
    tracks: Vec<Track>,
    selected: usize,
    current_index: Option<usize>,
    current_track: Option<Track>,
    artists: Vec<(String, usize)>,
    albums: Vec<(String, usize)>,
    shuffle: bool,
    repeat: RepeatMode,
    player: Player,
    scan_rx: Option<ScanReceiver>,
    scan_progress: Option<(usize, usize)>,
    status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepeatMode { Off, All, One }

impl RepeatMode {
    fn label(self) -> &'static str {
        match self { Self::Off => "off", Self::All => "all", Self::One => "one" }
    }
}

impl Library {
    /// Builds a new library from a selected folder and starts the scan immediately.
    fn new(folder: String) -> Self {
        let mut app = Self {
            music_folder: folder,
            tracks: Vec::new(),
            selected: 0,
            current_index: None,
            current_track: None,
            artists: Vec::new(),
            albums: Vec::new(),
            shuffle: false,
            repeat: RepeatMode::Off,
            player: Player::new(),
            scan_rx: None,
            scan_progress: None,
            status: None,
        };
        app.start_scan();
        app
    }

    /// Starts a background scan of the music folder.
    /// Progress is reported back to the UI through a channel.
    fn start_scan(&mut self) {
        let folder = self.music_folder.clone();
        let (tx, rx) = mpsc::channel();
        let tx_progress = tx.clone();
        self.scan_rx = Some(rx);
        self.scan_progress = None;
        self.tracks.clear();
        self.artists.clear();
        self.albums.clear();

        thread::spawn(move || {
            let last = Arc::new(Mutex::new(Instant::now()));
            let tracks = scan_music_folder(&folder, {
                let last = Arc::clone(&last);
                let tx_progress = tx_progress.clone();
                move |done, total| {
                    let mut last = last.lock().unwrap();
                    if done == total || last.elapsed() >= Duration::from_millis(100) {
                        let _ = tx_progress.send(ScanMessage::Progress(done, total));
                        *last = Instant::now();
                    }
                }
            });
            let _ = tx.send(ScanMessage::Finished(tracks));
        });
    }

    /// Consumes messages from the scan thread and updates the in-memory library state.
    fn consume_scan(&mut self) {
        let Some(rx) = &self.scan_rx else { return; };
        loop {
            match rx.try_recv() {
                Ok(ScanMessage::Progress(done, total)) => self.scan_progress = Some((done, total)),
                Ok(ScanMessage::Finished(tracks)) => {
                    self.scan_progress = None;
                    self.tracks = tracks;
                    self.rebuild_groups();
                    self.scan_rx = None;
                    if self.selected >= self.tracks.len() { self.selected = self.tracks.len().saturating_sub(1); }
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => { self.scan_rx = None; break; }
            }
        }
    }

    fn rebuild_groups(&mut self) {
        let mut artists = std::collections::HashMap::<String, usize>::new();
        let mut albums = std::collections::HashMap::<String, usize>::new();
        for track in &self.tracks {
            *artists.entry(track.artist.clone()).or_default() += 1;
            *albums.entry(track.album.clone()).or_default() += 1;
        }
        self.artists = artists.into_iter().collect();
        self.albums = albums.into_iter().collect();
        self.artists.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        self.albums.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    }

    fn next_index(&self, from: usize) -> Option<usize> {
        if self.tracks.is_empty() { return None; }
        if self.repeat == RepeatMode::One { return Some(from); }
        if self.shuffle {
            if self.tracks.len() == 1 { return Some(from); }
            let mut rng = rand::rng();
            let mut idx = rng.random_range(0..self.tracks.len());
            while idx == from { idx = rng.random_range(0..self.tracks.len()); }
            return Some(idx);
        }
        let next = from + 1;
        if next >= self.tracks.len() {
            if self.repeat == RepeatMode::All { Some(0) } else { None }
        } else { Some(next) }
    }

    fn prev_index(&self, from: usize) -> Option<usize> {
        if self.tracks.is_empty() { return None; }
        Some(if from == 0 { self.tracks.len() - 1 } else { from - 1 })
    }

    fn play_index(&mut self, index: usize) {
        let Some(track) = self.tracks.get(index).cloned() else { return; };
        self.selected = index;
        self.current_index = Some(index);
        self.current_track = Some(track.clone());
        self.status = if self.player.play(&track.path) {
            None
        } else {
            Some("Could not start mpv. Make sure mpv is installed and on PATH.".into())
        };
    }

    fn handle_track_end(&mut self) {
        if !self.player.poll_track_end() { return; }
        let Some(current) = self.current_index else {
            self.current_track = None;
            return;
        };
        if let Some(next) = self.next_index(current) {
            self.play_index(next);
        } else {
            self.current_track = None;
            self.current_index = None;
        }
    }

    /// Handles keyboard shortcuts while the library screen is active.
    fn handle_key(&mut self, code: KeyCode) -> LibraryAction {
        self.consume_scan();
        match code {
            KeyCode::Down => if !self.tracks.is_empty() { self.selected = (self.selected + 1).min(self.tracks.len() - 1); },
            KeyCode::Up => if !self.tracks.is_empty() { self.selected = self.selected.saturating_sub(1); },
            KeyCode::Enter => self.play_index(self.selected),
            KeyCode::Char(' ') => if self.current_track.is_some() { self.player.pause(); },
            KeyCode::Char('n') | KeyCode::Char('N') => {
                if let Some(next) = self.next_index(self.selected) { self.play_index(next); }
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                if let Some(prev) = self.prev_index(self.selected) { self.play_index(prev); }
            }
            KeyCode::Char('s') | KeyCode::Char('S') => self.shuffle = !self.shuffle,
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.repeat = match self.repeat { RepeatMode::Off => RepeatMode::All, RepeatMode::All => RepeatMode::One, RepeatMode::One => RepeatMode::Off };
            }
            KeyCode::Char('+') | KeyCode::Char('=') => { let v = self.player.set_volume(self.player.volume() as i16 + 5); self.status = Some(format!("Volume: {v}%")); }
            KeyCode::Char('-') | KeyCode::Char('_') => { let v = self.player.set_volume(self.player.volume() as i16 - 5); self.status = Some(format!("Volume: {v}%")); }
            KeyCode::Char('f') | KeyCode::Char('F') => return LibraryAction::ChangeFolder,
            KeyCode::Char('q') | KeyCode::Char('Q') => return LibraryAction::Quit,
            _ => {}
        }
        LibraryAction::None
    }

    fn tick(&mut self) {
        self.consume_scan();
        self.handle_track_end();
    }
}

enum LibraryAction { None, ChangeFolder, Quit }

/// Top-level app state.
/// It owns the current screen and persistent app config.
struct App {
    screen: Screen,
    config: AppConfig,
    can_cancel_splash: bool,
}

enum Screen {
    Splash(Splash),
    Library(Library),
}

impl App {
    /// Initializes the app, restoring the saved music folder if available.
    fn new() -> Self {
        let config = load_config();
        if let Some(folder) = config.music_folder.clone().filter(|f| valid_folder(f)) {
            Self { screen: Screen::Library(Library::new(folder)), config, can_cancel_splash: false }
        } else {
            let default = config.music_folder.clone().unwrap_or_else(|| {
                std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
                    .map(|p| Path::new(&p).join("Music").to_string_lossy().to_string())
                    .unwrap_or_else(|| "Music".into())
            });
            Self { screen: Screen::Splash(Splash::new(default)), config, can_cancel_splash: false }
        }
    }

    fn submit_folder(&mut self, folder: String) {
        self.config.music_folder = Some(folder.clone());
        let _ = save_config(&self.config);
        self.can_cancel_splash = true;
        self.screen = Screen::Library(Library::new(folder));
    }

    fn change_folder(&mut self) {
        let current = match &self.screen {
            Screen::Library(lib) => lib.music_folder.clone(),
            Screen::Splash(splash) => splash.value.clone(),
        };
        self.screen = Screen::Splash(Splash::new(current));
    }

    fn tick(&mut self) {
        if let Screen::Library(lib) = &mut self.screen { lib.tick(); }
    }
}

/// Entry point for the terminal UI app.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// Main event loop.
/// It redraws the screen, ticks the library, and responds to input events.
fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new();

    loop {
        terminal.draw(|frame| draw(frame, &app))?;
        app.tick();

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    match &mut app.screen {
                        Screen::Splash(splash) => {
                            let can_cancel = app.can_cancel_splash;
                            match splash.handle_key(key.code, can_cancel) {
                                SplashAction::Submit(folder) => app.submit_folder(folder),
                                SplashAction::Cancel => {
                                    if let Some(folder) = app.config.music_folder.clone().filter(|f| valid_folder(f)) {
                                        app.screen = Screen::Library(Library::new(folder));
                                    }
                                }
                                SplashAction::None => {}
                            }
                        }
                        Screen::Library(lib) => match lib.handle_key(key.code) {
                            LibraryAction::ChangeFolder => app.change_folder(),
                            LibraryAction::Quit => return Ok(()),
                            LibraryAction::None => {}
                        }
                    }
                }
                Event::Paste(text) => {
                    if let Screen::Splash(splash) = &mut app.screen {
                        splash.insert_text(&text);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Renders the active screen into the terminal frame.
fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    match &app.screen {
        Screen::Splash(splash) => draw_splash(frame, splash, app.can_cancel_splash, area),
        Screen::Library(lib) => draw_library(frame, lib, area),
    }
}

/// Draws the initial folder prompt screen.
fn draw_splash(frame: &mut Frame, splash: &Splash, can_cancel: bool, area: Rect) {
    let width = area.width.max(30).min(64).min(area.width.saturating_sub(4).max(30));
    let height = 11;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let box_area = Rect::new(x, y, width, height.min(area.height));

    let logo = vec![
        Line::from("        ♪").fg(BRIGHT_GREEN),
        Line::from("  <(o )___   ♫").fg(BRIGHT_GREEN),
        Line::from("   ( ._> /").fg(BRIGHT_GREEN),
        Line::from("    `---'").fg(BRIGHT_GREEN),
        Line::from(""),
        Line::from(Span::styled("S O N G B I R D", Style::default().fg(BRIGHT_GREEN).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled("your terminal music player", Style::default().fg(Color::DarkGray))),
        Line::from(""),
        Line::from(vec![
            Span::styled("Music folder: ", Style::default().fg(GREEN)),
            Span::styled(format!("{}█", truncate(&splash.value, width.saturating_sub(17) as usize)), Style::default().fg(BRIGHT_GREEN).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(if let Some(error) = &splash.error {
            Span::styled(error, Style::default().fg(Color::Red))
        } else {
            Span::styled(format!("ENTER Confirm{}", if can_cancel { "   ESC Cancel" } else { "" }), Style::default().fg(Color::DarkGray))
        }),
    ];

    frame.render_widget(Paragraph::new(Text::from(logo)).alignment(ratatui::layout::Alignment::Center), box_area);
}

/// Draws the full library view: header, playlist, controls, and status bar.
fn draw_library(frame: &mut Frame, lib: &Library, area: Rect) {
    let layout = compute_layout(area.width);

    let vertical = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5), Constraint::Length(3), Constraint::Length(3)])
        .split(area);

    // Header
    let title = "♫ SONGBIRD";
    let stats = format!("{} tracks · {} artists · {} albums", lib.tracks.len(), lib.artists.len(), lib.albums.len());
    // Pad from the real widths so the stats stay flush with the right border
    // instead of being clipped by a fixed guess.
    let gap = (vertical[0].width as usize)
        .saturating_sub(2)
        .saturating_sub(title.chars().count())
        .saturating_sub(stats.chars().count());
    let header = Line::from(vec![
        Span::styled(title, Style::default().fg(BRIGHT_GREEN).add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(gap)),
        Span::styled(stats, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(header).block(rounded_block("")), vertical[0]);

    let main = vertical[1];
    if layout.side_by_side {
        let mut constraints = Vec::new();
        if layout.show_sidebar { constraints.push(Constraint::Length(layout.sidebar_width)); }
        constraints.push(Constraint::Length(layout.playlist_width));
        constraints.push(Constraint::Length(layout.right_width));
        let cols = RLayout::default().direction(Direction::Horizontal).constraints(constraints).split(main);
        let mut i = 0;
        if layout.show_sidebar { draw_sidebar(frame, lib, cols[i]); i += 1; }
        draw_playlist(frame, lib, cols[i]);
        draw_right_column(frame, lib, cols[i + 1]);
    } else {
        let rows = RLayout::default().direction(Direction::Vertical).constraints([Constraint::Min(8), Constraint::Length(12)]).split(main);
        draw_playlist(frame, lib, rows[0]);
        draw_right_column(frame, lib, rows[1]);
    }

    let footer_text = "▲▼ Select   ENTER Play   SPACE Pause   N/P Next/Prev   S Shuffle   R Repeat   +/- Vol   F Folder   Q Quit";
    let footer = Paragraph::new(Line::from(Span::styled(footer_text, Style::default().fg(BRIGHT_GREEN).add_modifier(Modifier::BOLD)))).block(rounded_block(""));
    frame.render_widget(footer, vertical[2]);

    let right = if let Some(message) = &lib.status {
        message.clone()
    } else {
        lib.current_track.as_ref().map(|t| format!("[{}] [{}] [{}kbps]", if t.lossless { "lossless" } else { "lossy" }, t.codec, t.bitrate)).unwrap_or_default()
    };
    let shuffle_label = if lib.shuffle { "on" } else { "off" };
    let repeat_label = lib.repeat.label();
    let left_text = format!("[shuffle: {shuffle_label}] [repeat: {repeat_label}]");
    let dim = Style::default().fg(Color::DarkGray);
    let active = Style::default().fg(BRIGHT_GREEN).add_modifier(Modifier::BOLD);
    let shuffle_style = if lib.shuffle { active } else { dim };
    let repeat_style = if lib.repeat == RepeatMode::Off { dim } else { active };
    let gap = vertical[3].width.saturating_sub(left_text.chars().count() as u16 + right.chars().count() as u16 + 2) as usize;
    let status_line = Line::from(vec![
        Span::styled("[shuffle: ", dim),
        Span::styled(shuffle_label, shuffle_style),
        Span::styled("] [repeat: ", dim),
        Span::styled(repeat_label, repeat_style),
        Span::styled("]", dim),
        Span::raw(" ".repeat(gap)),
        Span::styled(right, active),
    ]);
    frame.render_widget(Paragraph::new(status_line).block(rounded_block("")), vertical[3]);
}

fn draw_sidebar(frame: &mut Frame, lib: &Library, area: Rect) {
    let artist_lines = lib.artists.len().min(10) + if lib.artists.len() > 10 { 1 } else { 0 };
    let album_lines = lib.albums.len().min(8) + if lib.albums.len() > 8 { 1 } else { 0 };
    let artist_height = (artist_lines as u16 + 2).min(area.height);
    let remaining = area.height.saturating_sub(artist_height);
    let album_height = (album_lines as u16 + 2).min(remaining);

    let rows = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(artist_height), Constraint::Length(album_height), Constraint::Min(0)])
        .split(area);
    draw_artist_box(frame, lib, rows[0]);
    draw_album_box(frame, lib, rows[1]);
}

fn draw_artist_box(frame: &mut Frame, lib: &Library, area: Rect) {
    let inner_width = area.width.saturating_sub(3) as usize;
    let mut lines = Vec::new();
    for (name, _) in lib.artists.iter().take(10) {
        lines.push(Line::from(format!(" {}", truncate(name, inner_width))));
    }
    if lib.artists.len() > 10 { lines.push(Line::from(format!(" +{} more", lib.artists.len() - 10))); }
    frame.render_widget(Paragraph::new(Text::from(lines)).block(rounded_block("ARTISTS")), area);
}

fn draw_album_box(frame: &mut Frame, lib: &Library, area: Rect) {
    let inner_width = area.width.saturating_sub(3) as usize;
    let mut lines = Vec::new();
    for (name, _) in lib.albums.iter().take(8) {
        lines.push(Line::from(format!(" {}", truncate(name, inner_width))));
    }
    if lib.albums.len() > 8 { lines.push(Line::from(format!(" +{} more", lib.albums.len() - 8))); }
    frame.render_widget(Paragraph::new(Text::from(lines)).block(rounded_block("ALBUMS")), area);
}

fn draw_playlist(frame: &mut Frame, lib: &Library, area: Rect) {
    let inner_width = area.width.saturating_sub(2) as usize;
    let title = format!("PLAYLIST · {} songs", lib.tracks.len());
    let block = rounded_block(&title);
    let content_rows = area.height.saturating_sub(2) as usize;
    let needs_scroll_indicator = lib.tracks.len() > content_rows;
    let visible_rows = if needs_scroll_indicator { content_rows.saturating_sub(1) } else { content_rows }.max(1);
    let start = if lib.tracks.is_empty() { 0 } else { lib.selected.saturating_sub(visible_rows / 2).min(lib.tracks.len().saturating_sub(visible_rows)) };
    let mut lines = Vec::new();

    if lib.tracks.is_empty() {
        let text = if let Some((done, total)) = lib.scan_progress { format!(" Scanning music... {done}/{total}") } else { " Scanning music...".into() };
        lines.push(Line::from(Span::styled(text, Style::default().fg(Color::DarkGray))));
    } else {
        for (offset, track) in lib.tracks.iter().skip(start).take(visible_rows).enumerate() {
            let index = start + offset;
            let selected = index == lib.selected;
            let playing = lib.current_index == Some(index);
            let prefix = if selected { ">" } else { " " };
            let play_icon = if playing { "▶" } else { "·" };
            let text = format!("{prefix} {:>3} {play_icon} {}", index + 1, track.title);
            let style = if selected {
                Style::default().fg(Color::Black).bg(GREEN)
            } else if playing {
                Style::default().fg(BRIGHT_GREEN).add_modifier(Modifier::BOLD)
            } else { Style::default() };
            lines.push(Line::from(Span::styled(truncate(&text, inner_width), style)));
        }
        if lib.tracks.len() > visible_rows {
            lines.push(Line::from(Span::styled(format!(" {}{}/{}{}", if start > 0 { "↑ " } else { "  " }, lib.selected + 1, lib.tracks.len(), if start + visible_rows < lib.tracks.len() { " ↓" } else { "  " }), Style::default().fg(Color::DarkGray))));
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn draw_right_column(frame: &mut Frame, lib: &Library, area: Rect) {
    let rows = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Length(7), Constraint::Min(0)])
        .split(area);
    draw_meta_box(frame, lib, rows[0]);
    draw_now_playing_box(frame, lib, rows[1]);
}

fn draw_meta_box(frame: &mut Frame, lib: &Library, area: Rect) {
    let width = area.width.saturating_sub(12) as usize;
    let (artist, album, year) = match lib.current_track.as_ref() {
        Some(track) => (track.artist.clone(), track.album.clone(), track.year.map(|y| y.to_string()).unwrap_or_else(|| "—".into())),
        None => ("—".to_string(), "—".to_string(), "—".to_string()),
    };
    let lines = vec![
        Line::from(vec![Span::styled("Artist: ", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)), Span::raw(truncate(&artist, width))]),
        Line::from(vec![Span::styled("Album:  ", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)), Span::raw(truncate(&album, width))]),
        Line::from(vec![Span::styled("Year:   ", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)), Span::raw(year)]),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)).block(rounded_block("")), area);
}

fn draw_now_playing_box(frame: &mut Frame, lib: &Library, area: Rect) {
    let Some(track) = lib.current_track.as_ref() else {
        frame.render_widget(Paragraph::new(Span::styled("Nothing playing — press ENTER on a track", Style::default().fg(Color::DarkGray))).block(rounded_block("")), area);
        return;
    };

    let elapsed = lib.player.elapsed_seconds();
    let duration = track.duration;
    let bar_width = area.width.saturating_sub(6).max(10) as usize;
    let progress = render_bar(if duration > 0.0 { elapsed / duration } else { 0.0 }, bar_width);
    let pause_icon = if lib.player.paused() { "❚❚" } else { "▶" };
    let title = truncate(&track.title, area.width.saturating_sub(6) as usize);
    let lines = vec![
        Line::from(Span::styled("Now Playing", Style::default().fg(GREEN).add_modifier(Modifier::BOLD))),
        Line::from(format!("{pause_icon} {title}")),
        Line::from(""),
        Line::from(progress),
        Line::from(Span::styled(format!("{} / {}   [VOL {}%]", format_time(elapsed), format_time(duration), lib.player.volume()), Style::default().fg(Color::DarkGray))),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)).block(rounded_block("")), area);
}
