use std::{
    cmp,
    collections::HashMap,
    env, fs,
    io::{self, Read, stdout},
    mem,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, anyhow};
use content_inspector::ContentType;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dirs::config_dir;
use fs_extra::dir::{CopyOptions as DirCopyOptions, copy as copy_dir};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use serde::Deserialize;
use tokio::{
    runtime::{Handle, Runtime},
    sync::mpsc::{UnboundedReceiver, UnboundedSender, error::TryRecvError, unbounded_channel},
};
use toml;

const PREVIEW_MAX_BYTES: usize = 8 * 1024;
const PREVIEW_MAX_LINES: usize = 80;
const PREVIEW_DIR_ENTRIES: usize = 12;

fn main() -> Result<()> {
    let mut terminal = init_terminal().context("failed to init terminal")?;
    let app_result = run_app(&mut terminal);
    cleanup_terminal(&mut terminal).context("failed to restore terminal")?;
    app_result
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen).context("switch to alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("spawn terminal backend")
}

fn cleanup_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode().context("disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let runtime = Runtime::new().context("start async runtime")?;
    let (fs_dispatcher, mut fs_rx) = FsDispatcher::new(&runtime);
    let config = load_config();
    let mut app = App::new(fs_dispatcher, config).context("construct app")?;
    let tick_rate = Duration::from_millis(150);

    loop {
        app.drain_fs_events(&mut fs_rx);
        process_external_commands(&mut app, terminal);
        terminal
            .draw(|frame| render(frame, &app))
            .context("draw frame")?;
        if poll_and_handle_events(&mut app, tick_rate)? {
            break;
        }
    }
    Ok(())
}

fn process_external_commands(app: &mut App, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    while let Some(command) = app.take_external_command() {
        let result = match command {
            ExternalCommand::Edit { path, name } => run_editor(terminal, &path)
                .and_then(|_| app.refresh_with_message(false, format!("Edited {}", name))),
            ExternalCommand::Shell { dir } => run_shell(terminal, &dir)
                .and_then(|_| app.refresh_with_message(false, "Returned from shell")),
        };
        if let Err(err) = result {
            app.status = format!("External command failed: {err:#}");
        }
    }
}

fn poll_and_handle_events(app: &mut App, tick_rate: Duration) -> Result<bool> {
    if event::poll(tick_rate).context("poll for events")? {
        match event::read().context("read event")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if handle_key_event(app, key)? {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

fn handle_key_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    match app.input_mode.clone() {
        InputMode::Normal => handle_normal_mode(app, key),
        InputMode::Search { .. } => handle_search_mode(app, key),
        InputMode::Command { .. } => handle_command_mode(app, key),
        InputMode::Confirm { .. } => handle_confirm_mode(app, key),
    }
}

fn handle_normal_mode(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('j') | KeyCode::Down => {
            app.awaiting_g = false;
            app.move_selection_by_count(1)
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.awaiting_g = false;
            app.move_selection_by_count(-1)
        }
        KeyCode::Char('g') => {
            if app.awaiting_g {
                app.awaiting_g = false;
                let target = app.take_count().unwrap_or(1).saturating_sub(1);
                app.jump_to_index(target);
            } else {
                app.awaiting_g = true;
                app.status = "Press g again to jump to entry".into();
            }
        }
        KeyCode::Char('G') => {
            app.awaiting_g = false;
            if let Some(count) = app.take_count() {
                let target = count.saturating_sub(1);
                app.jump_to_index(target);
            } else {
                app.jump_to_end();
            }
        }
        KeyCode::Char('r') => {
            app.awaiting_g = false;
            handle_refresh(app);
            app.clear_pending_count();
        }
        KeyCode::Char('h') | KeyCode::Left => {
            app.awaiting_g = false;
            if let Err(err) = app.open_parent() {
                app.status = format!("Error: {err:#}");
            }
            app.clear_pending_count();
        }
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
            app.awaiting_g = false;
            if let Err(err) = app.enter_selection() {
                app.status = format!("Error: {err:#}");
            }
            app.clear_pending_count();
        }
        KeyCode::Char('n') => {
            app.awaiting_g = false;
            app.search_next();
            app.clear_pending_count();
        }
        KeyCode::Char('N') => {
            app.awaiting_g = false;
            app.search_prev();
            app.clear_pending_count();
        }
        KeyCode::Char('/') => {
            app.awaiting_g = false;
            app.start_search();
        }
        KeyCode::Char(':') => {
            app.awaiting_g = false;
            app.start_command();
        }
        KeyCode::Char(ch) if ch.is_ascii_digit() => {
            app.accumulate_count(ch);
        }
        _ => {
            app.awaiting_g = false;
            app.clear_pending_count();
        }
    }
    Ok(false)
}

fn handle_search_mode(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.cancel_overlay();
            app.status = "Search canceled".into();
        }
        KeyCode::Enter => {
            if let InputMode::Search { buffer, .. } = &app.input_mode {
                if buffer.is_empty() {
                    app.set_overlay_feedback("Enter a search query");
                } else {
                    let query = buffer.clone();
                    app.cancel_overlay();
                    app.apply_search(&query);
                }
            }
        }
        KeyCode::Backspace => {
            if let InputMode::Search { buffer, .. } = &mut app.input_mode {
                buffer.pop();
            }
            app.clear_overlay_feedback();
        }
        KeyCode::Char(ch) if !ch.is_control() => {
            if let InputMode::Search { buffer, .. } = &mut app.input_mode {
                buffer.push(ch);
            }
            app.clear_overlay_feedback();
        }
        _ => {}
    }
    Ok(false)
}

fn handle_command_mode(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.cancel_overlay();
            app.status = "Command canceled".into();
        }
        KeyCode::Enter => {
            if let InputMode::Command { buffer, .. } = &app.input_mode {
                if buffer.trim().is_empty() {
                    app.set_overlay_feedback("Enter a command");
                } else {
                    let command = buffer.clone();
                    app.cancel_overlay();
                    app.run_command(command);
                }
            }
        }
        KeyCode::Backspace => {
            if let InputMode::Command { buffer, .. } = &mut app.input_mode {
                buffer.pop();
            }
            app.clear_overlay_feedback();
        }
        KeyCode::Char(ch) if !ch.is_control() => {
            if let InputMode::Command { buffer, .. } = &mut app.input_mode {
                buffer.push(ch);
            }
            app.clear_overlay_feedback();
        }
        _ => {}
    }
    Ok(false)
}

fn handle_confirm_mode(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
            app.cancel_overlay();
            app.status = "Action canceled".into();
        }
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
            if let InputMode::Confirm { action, .. } =
                mem::replace(&mut app.input_mode, InputMode::Normal)
            {
                match app.execute_confirm_action(action) {
                    Ok(_) => {}
                    Err(err) => app.status = format!("Action failed: {err:#}"),
                }
            }
            app.clear_pending_count();
        }
        _ => {}
    }
    Ok(false)
}

fn handle_refresh(app: &mut App) {
    if let Err(err) = app.refresh_async(false) {
        app.status = format!("Error: {err:#}");
    }
}

fn run_editor(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, path: &Path) -> Result<()> {
    suspend_terminal(terminal)?;
    let editor = resolve_editor();
    let status_result = Command::new(&editor)
        .arg(path)
        .status()
        .with_context(|| format!("launching {} for {}", editor, path.display()));
    let resume_result = resume_terminal(terminal);
    let status = status_result?;
    resume_result?;
    if !status.success() {
        return Err(anyhow!(
            "Editor exited with status {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".into())
        ));
    }
    Ok(())
}

fn resolve_editor() -> String {
    env::var("EDITOR")
        .or_else(|_| env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".into())
}

fn run_shell(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, dir: &Path) -> Result<()> {
    suspend_terminal(terminal)?;
    let shell = resolve_shell();
    let status_result = Command::new(&shell)
        .current_dir(dir)
        .status()
        .with_context(|| format!("launching shell {} in {}", shell, dir.display()));
    let resume_result = resume_terminal(terminal);
    let status = status_result?;
    resume_result?;
    if !status.success() {
        return Err(anyhow!(
            "Shell exited with status {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".into())
        ));
    }
    Ok(())
}

fn resolve_shell() -> String {
    env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode().context("disable raw mode for external command")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("leave alternate screen for external command")?;
    terminal.show_cursor().ok();
    Ok(())
}

fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    enable_raw_mode().context("enable raw mode after external command")?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)
        .context("re-enter alternate screen after external command")?;
    terminal.hide_cursor().ok();
    terminal.clear().context("clear terminal after resume")?;
    Ok(())
}
fn render(frame: &mut Frame, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(frame.size());

    draw_header(frame, layout[0], app);
    draw_body(frame, layout[1], app);
    draw_footer(frame, layout[2], app);
    draw_overlay(frame, app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let title = Span::styled(
        "Wayfinder",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    let path = Span::styled(
        app.current_dir.display().to_string(),
        Style::default().fg(Color::Cyan),
    );
    let line = Line::from(vec![title, Span::raw(" - "), path]);
    let widget = Paragraph::new(line).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Current Directory"),
    );
    frame.render_widget(widget, area);
}

fn draw_body(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let list_items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|entry| {
            let icon = if entry.is_dir { "[D]" } else { "[F]" };
            let line = Line::from(vec![
                Span::styled(icon, Style::default().fg(Color::LightBlue)),
                Span::raw(" "),
                Span::raw(&entry.name),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(list_items)
        .block(Block::default().borders(Borders::ALL).title("Files"))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut list_state = app.list_state();
    frame.render_stateful_widget(list, chunks[0], &mut list_state);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
        .split(chunks[1]);

    let detail = Paragraph::new(app.describe_selection())
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL).title("Details"));
    frame.render_widget(detail, right[0]);

    let preview = Paragraph::new(app.preview.body.as_str())
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.preview.title.as_str()),
        );
    frame.render_widget(preview, right[1]);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let footer = Paragraph::new(app.footer_text())
        .style(Style::default().fg(Color::Gray))
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, area);
}

fn draw_overlay(frame: &mut Frame, app: &App) {
    if let Some((title, content)) = app.overlay_prompt() {
        let area = overlay_area(frame.size());
        frame.render_widget(Clear, area);
        let widget =
            Paragraph::new(content).block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(widget, area);
    }
}

fn overlay_area(area: Rect) -> Rect {
    let height = 3u16;
    let width = area.width.saturating_sub(2);
    let x = area.x + 1;
    let y = area.y + area.height.saturating_sub(height + 1);
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[derive(Clone)]
enum InputMode {
    Normal,
    Search {
        buffer: String,
        feedback: Option<String>,
    },
    Command {
        buffer: String,
        feedback: Option<String>,
    },
    Confirm {
        message: String,
        action: ConfirmAction,
    },
}

#[derive(Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    command_aliases: HashMap<String, String>,
}

#[derive(Clone)]
struct Config {
    command_aliases: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        let mut aliases = HashMap::new();
        aliases.insert("rm".into(), "delete".into());
        aliases.insert("cp".into(), "copy".into());
        aliases.insert("mv".into(), "move".into());
        Self {
            command_aliases: aliases,
        }
    }
}

fn load_config() -> Config {
    let mut config = Config::default();
    if let Some(mut dir) = config_dir() {
        dir.push("wayfinder");
        let path = dir.join("config.toml");
        if let Ok(contents) = fs::read_to_string(&path) {
            match toml::from_str::<RawConfig>(&contents) {
                Ok(raw) => {
                    for (alias, command) in raw.command_aliases {
                        config
                            .command_aliases
                            .insert(alias.to_lowercase(), command.to_lowercase());
                    }
                }
                Err(err) => eprintln!("Failed to parse config {}: {err}", path.display()),
            }
        }
    }
    config
}

fn split_command(input: &str) -> (&str, &str) {
    if let Some((cmd, rest)) = input.split_once(char::is_whitespace) {
        (cmd, rest.trim_start())
    } else {
        (input, "")
    }
}

#[derive(Clone)]
enum ConfirmAction {
    Delete { entry: FileEntry, path: PathBuf },
}

#[derive(Clone)]
enum ExternalCommand {
    Edit { path: PathBuf, name: String },
    Shell { dir: PathBuf },
}

#[derive(Clone)]
struct PreviewPane {
    title: String,
    body: String,
}

impl PreviewPane {
    fn new<T: Into<String>, B: Into<String>>(title: T, body: B) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
        }
    }

    fn empty() -> Self {
        Self::new("Preview", "No file selected")
    }

    fn loading() -> Self {
        Self::new("Preview", "Loading preview...")
    }

    fn error(message: impl Into<String>) -> Self {
        Self::new("Preview", message)
    }
}

struct App {
    current_dir: PathBuf,
    entries: Vec<FileEntry>,
    selected: usize,
    status: String,
    fs: FsDispatcher,
    pending_token: Option<u64>,
    next_token: u64,
    is_loading: bool,
    input_mode: InputMode,
    pending_count: Option<usize>,
    last_search: Option<String>,
    last_action_message: Option<String>,
    pending_external: Option<ExternalCommand>,
    preview: PreviewPane,
    awaiting_g: bool,
    command_aliases: HashMap<String, String>,
}

impl App {
    const HELP_LINE: &'static str = "j/k navigate | h/l change dirs | q quit";

    fn new(fs: FsDispatcher, config: Config) -> Result<Self> {
        let current_dir = std::env::current_dir().context("read current dir")?;
        let mut app = Self {
            current_dir,
            entries: Vec::new(),
            selected: 0,
            status: String::new(),
            fs,
            pending_token: None,
            next_token: 0,
            is_loading: false,
            input_mode: InputMode::Normal,
            pending_count: None,
            last_search: None,
            last_action_message: None,
            pending_external: None,
            preview: PreviewPane::loading(),
            awaiting_g: false,
            command_aliases: config.command_aliases,
        };
        app.refresh_async(true)?;
        Ok(app)
    }

    fn refresh_async(&mut self, clear_entries: bool) -> Result<()> {
        if clear_entries {
            self.entries.clear();
            self.selected = 0;
            self.preview = PreviewPane::loading();
        }
        let token = self.next_token;
        self.next_token += 1;
        let path = self.current_dir.clone();
        self.fs
            .request_directory_scan(path.clone(), token)
            .context("queue directory scan")?;

        self.pending_token = Some(token);
        self.is_loading = true;
        self.status = format!("Loading {} ...", path.display());
        Ok(())
    }

    fn refresh_with_message<S: Into<String>>(
        &mut self,
        clear_entries: bool,
        message: S,
    ) -> Result<()> {
        self.last_action_message = Some(message.into());
        self.refresh_async(clear_entries)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected = 0;
            self.preview = PreviewPane::empty();
            return;
        }
        let len = self.entries.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len);
        self.selected = next as usize;
        self.update_preview();
    }

    fn move_selection_by_count(&mut self, delta: isize) {
        let count = self.consume_count_or(1);
        let scaled = delta.saturating_mul(count as isize);
        self.move_selection(scaled);
    }

    fn accumulate_count(&mut self, digit: char) {
        if let Some(value) = digit.to_digit(10) {
            let next = self
                .pending_count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(value as usize);
            let capped = next.min(9_999);
            self.pending_count = Some(capped);
            self.status = format!("Count: {capped}");
        }
    }

    fn consume_count_or(&mut self, default: usize) -> usize {
        self.pending_count.take().unwrap_or(default)
    }

    fn take_count(&mut self) -> Option<usize> {
        self.pending_count.take()
    }

    fn clear_pending_count(&mut self) {
        self.pending_count = None;
    }

    fn resolve_command_alias(&self, cmd: &str) -> String {
        let key = cmd.to_lowercase();
        self.command_aliases.get(&key).cloned().unwrap_or(key)
    }

    fn jump_to_index(&mut self, index: usize) {
        if self.entries.is_empty() {
            self.selected = 0;
            self.preview = PreviewPane::empty();
            return;
        }
        let max_idx = self.entries.len().saturating_sub(1);
        self.selected = index.min(max_idx);
        self.update_preview();
    }

    fn jump_to_end(&mut self) {
        if !self.entries.is_empty() {
            self.selected = self.entries.len() - 1;
            self.update_preview();
        }
    }

    fn enter_selection(&mut self) -> Result<()> {
        if let Some(entry) = self.entries.get(self.selected).cloned() {
            if entry.is_dir {
                let previous = self.current_dir.clone();
                self.current_dir.push(&entry.name);
                if let Err(err) = self.refresh_async(true) {
                    self.current_dir = previous;
                    return Err(err);
                }
                self.reset_search_state();
            } else {
                self.status = format!("'{}' is not a directory", entry.name);
            }
        }
        Ok(())
    }

    fn open_parent(&mut self) -> Result<()> {
        let previous = self.current_dir.clone();
        if self.current_dir.pop() {
            if let Err(err) = self.refresh_async(true) {
                self.current_dir = previous;
                return Err(err);
            }
            self.reset_search_state();
        }
        Ok(())
    }

    fn start_search(&mut self) {
        self.clear_pending_count();
        let buffer = self.last_search.clone().unwrap_or_default();
        self.input_mode = InputMode::Search {
            buffer,
            feedback: None,
        };
        self.status = "Search: type to filter, Enter to apply".into();
    }

    fn start_command(&mut self) {
        self.clear_pending_count();
        self.input_mode = InputMode::Command {
            buffer: String::new(),
            feedback: None,
        };
        self.status = "Command: Enter to run, Esc to cancel".into();
    }

    fn cancel_overlay(&mut self) {
        self.input_mode = InputMode::Normal;
        self.clear_pending_count();
    }

    fn set_overlay_feedback(&mut self, message: impl Into<String>) {
        let text = Some(message.into());
        match &mut self.input_mode {
            InputMode::Search { feedback, .. } => *feedback = text,
            InputMode::Command { feedback, .. } => *feedback = text,
            _ => {}
        }
    }

    fn clear_overlay_feedback(&mut self) {
        match &mut self.input_mode {
            InputMode::Search { feedback, .. } => *feedback = None,
            InputMode::Command { feedback, .. } => *feedback = None,
            _ => {}
        }
    }

    fn list_state(&self) -> ratatui::widgets::ListState {
        let mut state = ratatui::widgets::ListState::default();
        if !self.entries.is_empty() {
            state.select(Some(self.selected));
        }
        state
    }

    fn selected_entry(&self) -> Option<&FileEntry> {
        self.entries.get(self.selected)
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.selected_entry()
            .map(|entry| self.current_dir.join(&entry.name))
    }

    fn take_external_command(&mut self) -> Option<ExternalCommand> {
        self.pending_external.take()
    }

    fn describe_selection(&self) -> String {
        if self.is_loading {
            "Loading directory...".into()
        } else {
            self.entries
                .get(self.selected)
                .map(|entry| entry.describe())
                .unwrap_or_else(|| "No entries".into())
        }
    }

    fn overlay_prompt(&self) -> Option<(String, String)> {
        match &self.input_mode {
            InputMode::Normal => None,
            InputMode::Search { buffer, feedback } => {
                let mut content = format!("/{}", buffer);
                if let Some(msg) = feedback {
                    content.push('\n');
                    content.push_str(msg);
                }
                Some(("Search".into(), content))
            }
            InputMode::Command { buffer, feedback } => {
                let mut content = format!(":{}", buffer);
                if let Some(msg) = feedback {
                    content.push('\n');
                    content.push_str(msg);
                }
                Some(("Command".into(), content))
            }
            InputMode::Confirm { message, .. } => {
                Some(("Confirm".into(), format!("{message} [y/n]")))
            }
        }
    }

    fn clamp_selection(&mut self) {
        if self.selected >= self.entries.len() {
            self.selected = self.entries.len().saturating_sub(1);
        }
        self.update_preview();
    }

    fn drain_fs_events(&mut self, rx: &mut UnboundedReceiver<FsEvent>) {
        loop {
            match rx.try_recv() {
                Ok(event) => self.handle_fs_event(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.status = "Filesystem worker disconnected".into();
                    self.is_loading = false;
                    break;
                }
            }
        }
    }

    fn handle_fs_event(&mut self, event: FsEvent) {
        match event {
            FsEvent::DirectoryLoaded {
                path,
                token,
                result,
            } => {
                if Some(token) != self.pending_token {
                    return;
                }
                self.pending_token = None;
                self.is_loading = false;
                match result {
                    Ok(entries) => {
                        self.entries = entries;
                        self.clamp_selection();
                        if let Some(message) = self.last_action_message.take() {
                            self.status = message;
                        } else {
                            self.status = format!(
                                "Loaded {} entries from {}",
                                self.entries.len(),
                                path.display()
                            );
                        }
                    }
                    Err(err) => {
                        self.entries.clear();
                        self.selected = 0;
                        self.last_action_message = None;
                        self.status = format!("Error loading {}: {}", path.display(), err);
                    }
                }
            }
        }
    }

    fn footer_text(&self) -> String {
        let mut segments: Vec<String> = Vec::new();
        if !self.status.is_empty() {
            segments.push(self.status.clone());
        }
        if let Some(count) = self.pending_count {
            segments.push(format!("count {}", count));
        }
        segments.push(Self::HELP_LINE.into());
        segments.join(" | ")
    }

    fn search_next(&mut self) {
        if self.entries.is_empty() {
            self.status = "No entries to search".into();
            return;
        }
        let query = match self.last_search.clone() {
            Some(q) => q,
            None => {
                self.status = "No previous search".into();
                return;
            }
        };
        let start = (self.selected + 1) % self.entries.len();
        if let Some(index) = self.find_match(&query, start) {
            self.selected = index;
            self.status = format!("Match: {}", self.entries[index].name);
            self.update_preview();
        } else {
            self.status = format!("No more matches for '{query}'");
        }
    }

    fn search_prev(&mut self) {
        if self.entries.is_empty() {
            self.status = "No entries to search".into();
            return;
        }
        let query = match self.last_search.clone() {
            Some(q) => q,
            None => {
                self.status = "No previous search".into();
                return;
            }
        };
        let len = self.entries.len();
        let start = if len == 0 {
            0
        } else {
            (self.selected + len - 1) % len
        };
        if let Some(index) = self.find_match_reverse(&query, start) {
            self.selected = index;
            self.status = format!("Match: {}", self.entries[index].name);
            self.update_preview();
        } else {
            self.status = format!("No previous matches for '{query}'");
        }
    }

    fn find_match(&self, query: &str, start_index: usize) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let needle = query.to_lowercase();
        let len = self.entries.len();
        for offset in 0..len {
            let index = (start_index + offset) % len;
            if self.entries[index].name.to_lowercase().contains(&needle) {
                return Some(index);
            }
        }
        None
    }

    fn find_match_reverse(&self, query: &str, start_index: usize) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let needle = query.to_lowercase();
        let len = self.entries.len();
        let mut index = start_index % len;
        for _ in 0..len {
            if self.entries[index].name.to_lowercase().contains(&needle) {
                return Some(index);
            }
            if index == 0 {
                index = len - 1;
            } else {
                index -= 1;
            }
        }
        None
    }

    fn apply_search(&mut self, query: &str) {
        if self.entries.is_empty() {
            self.status = "No entries to search".into();
            return;
        }
        let start = self.selected;
        self.last_search = Some(query.to_string());
        if let Some(index) = self.find_match(query, start) {
            self.selected = index;
            self.status = format!("Match: {}", self.entries[index].name);
            self.update_preview();
        } else {
            self.status = format!("No match for '{query}'");
        }
    }

    fn reset_search_state(&mut self) {
        self.last_search = None;
        if let InputMode::Search { buffer, .. } = &mut self.input_mode {
            buffer.clear();
        }
    }

    fn run_command(&mut self, input: String) {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            self.status = "Empty command".into();
            return;
        }
        let (cmd, args) = split_command(trimmed);
        let command = self.resolve_command_alias(cmd);
        match command.as_str() {
            "pwd" => self.status = format!("{}", self.current_dir.display()),
            "refresh" => {
                if let Err(err) = self.refresh_async(false) {
                    self.status = format!("Refresh failed: {err:#}");
                } else {
                    self.status = "Refresh requested".into();
                }
            }
            "q" | "quit" => {
                self.status = "Use 'q' in normal mode to quit".into();
            }
            "rename" => {
                if args.is_empty() {
                    self.status = "Usage: :rename <new_name>".into();
                } else if let Err(err) = self.command_rename(args) {
                    self.status = format!("Rename failed: {err:#}");
                }
            }
            "delete" => {
                if let Err(err) = self.request_delete_confirmation() {
                    self.status = format!("Delete failed: {err:#}");
                }
            }
            "mkdir" => {
                if args.is_empty() {
                    self.status = "Usage: :mkdir <name>".into();
                } else if let Err(err) = self.command_mkdir(args) {
                    self.status = format!("mkdir failed: {err:#}");
                }
            }
            "touch" => {
                if args.is_empty() {
                    self.status = "Usage: :touch <name>".into();
                } else if let Err(err) = self.command_touch(args) {
                    self.status = format!("touch failed: {err:#}");
                }
            }
            "copy" => {
                if args.is_empty() {
                    self.status = "Usage: :copy <destination>".into();
                } else if let Err(err) = self.command_copy(args) {
                    self.status = format!("copy failed: {err:#}");
                }
            }
            "move" => {
                if args.is_empty() {
                    self.status = "Usage: :move <destination>".into();
                } else if let Err(err) = self.command_move(args) {
                    self.status = format!("move failed: {err:#}");
                }
            }
            "sh" => {
                if let Err(err) = self.command_shell() {
                    self.status = format!("shell failed: {err:#}");
                }
            }
            "edit" => {
                if let Err(err) = self.command_edit() {
                    self.status = format!("edit failed: {err:#}");
                }
            }
            "cd" => {
                if args.is_empty() {
                    self.status = "Usage: :cd <path>".into();
                } else if let Err(err) = self.command_cd(args) {
                    self.status = format!("cd failed: {err:#}");
                }
            }
            "help" => {
                self.status = "Commands: pwd, refresh, rename, delete, mkdir, touch, copy, move, edit, sh, cd, help".into();
            }
            other => {
                self.status = format!("Unknown command: {other}");
            }
        }
    }

    fn command_rename(&mut self, new_name: &str) -> Result<()> {
        let entry = self
            .selected_entry()
            .cloned()
            .ok_or_else(|| anyhow!("No selection to rename"))?;
        let new_name = self.validate_new_name(new_name, &entry.name)?;
        let src = self
            .selected_path()
            .ok_or_else(|| anyhow!("No selection to rename"))?;
        let dest = self.current_dir.join(&new_name);
        if dest.exists() {
            return Err(anyhow!("A file named '{}' already exists", new_name));
        }
        fs::rename(&src, &dest)
            .with_context(|| format!("renaming {} -> {}", entry.name, new_name))?;
        self.refresh_with_message(true, format!("Renamed {} -> {}", entry.name, new_name))?;
        Ok(())
    }

    fn request_delete_confirmation(&mut self) -> Result<()> {
        let entry = self
            .selected_entry()
            .cloned()
            .ok_or_else(|| anyhow!("No selection to delete"))?;
        let path = self
            .selected_path()
            .ok_or_else(|| anyhow!("No selection to delete"))?;
        let message = format!("Delete '{}'?", entry.name);
        self.input_mode = InputMode::Confirm {
            message,
            action: ConfirmAction::Delete { entry, path },
        };
        self.status = "Confirm delete with y/n".into();
        Ok(())
    }

    fn command_delete(&mut self, entry: FileEntry, path: PathBuf) -> Result<()> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.name == entry.name)
            .cloned()
            .unwrap_or(entry);
        if entry.is_dir {
            fs::remove_dir_all(&path)
                .with_context(|| format!("removing directory {}", entry.name))?;
        } else {
            fs::remove_file(&path).with_context(|| format!("removing file {}", entry.name))?;
        }
        self.refresh_with_message(true, format!("Deleted {}", entry.name))?;
        Ok(())
    }

    fn command_mkdir(&mut self, name: &str) -> Result<()> {
        let name = self.validate_new_name(name, "")?;
        let path = self.current_dir.join(&name);
        fs::create_dir(&path).with_context(|| format!("creating directory {}", name))?;
        self.refresh_with_message(false, format!("Created directory {}", name))?;
        Ok(())
    }

    fn command_touch(&mut self, name: &str) -> Result<()> {
        let name = self.validate_new_name(name, "")?;
        let path = self.current_dir.join(&name);
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("creating file {}", name))?;
        self.refresh_with_message(false, format!("Touched {}", name))?;
        Ok(())
    }

    fn command_edit(&mut self) -> Result<()> {
        let entry = self
            .selected_entry()
            .cloned()
            .ok_or_else(|| anyhow!("No selection to edit"))?;
        if entry.is_dir {
            return Err(anyhow!("Cannot edit a directory"));
        }
        let path = self
            .selected_path()
            .ok_or_else(|| anyhow!("No selection to edit"))?;
        self.pending_external = Some(ExternalCommand::Edit {
            path,
            name: entry.name.clone(),
        });
        self.status = format!("Launching editor for {}", entry.name);
        Ok(())
    }

    fn command_shell(&mut self) -> Result<()> {
        let dir = self.current_dir.clone();
        self.pending_external = Some(ExternalCommand::Shell { dir: dir.clone() });
        self.status = format!("Launching shell in {}", dir.display());
        Ok(())
    }

    fn command_cd(&mut self, target: &str) -> Result<()> {
        let target = target.trim();
        if target.is_empty() {
            return Err(anyhow!("Usage: :cd <path>"));
        }
        let path = Path::new(target);
        let mut resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.current_dir.join(path)
        };
        resolved = fs::canonicalize(&resolved)
            .with_context(|| format!("resolving directory {}", target))?;
        if !resolved.is_dir() {
            return Err(anyhow!("{} is not a directory", resolved.display()));
        }
        self.current_dir = resolved;
        self.reset_search_state();
        self.refresh_with_message(true, "Changed directory")?;
        Ok(())
    }

    fn execute_confirm_action(&mut self, action: ConfirmAction) -> Result<()> {
        match action {
            ConfirmAction::Delete { entry, path } => self.command_delete(entry, path),
        }
    }

    fn update_preview(&mut self) {
        if self.is_loading {
            self.preview = PreviewPane::loading();
            return;
        }
        if self.entries.is_empty() {
            self.preview = PreviewPane::empty();
            return;
        }
        if let Some(entry) = self.selected_entry().cloned() {
            let path = self.current_dir.join(&entry.name);
            match build_preview(&entry, &path) {
                Ok(preview) => self.preview = preview,
                Err(err) => self.preview = PreviewPane::error(format!("Preview error: {err:#}")),
            }
        } else {
            self.preview = PreviewPane::empty();
        }
    }

    fn compute_destination(&self, target: &str, entry_name: &str) -> Result<PathBuf> {
        let trimmed = target.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("Destination path required"));
        }
        let mut dest = PathBuf::from(trimmed);
        if dest.is_relative() {
            dest = self.current_dir.join(dest);
        }
        let hint_dir = trimmed.ends_with('/') || trimmed.ends_with('\\');
        if hint_dir || dest.is_dir() {
            dest.push(entry_name);
        }
        Ok(dest)
    }

    fn validate_new_name(&self, input: &str, current: &str) -> Result<String> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("Name cannot be empty"));
        }
        if !current.is_empty() && trimmed == current {
            return Err(anyhow!("Name is unchanged"));
        }
        if trimmed == "." || trimmed == ".." {
            return Err(anyhow!("Invalid name '{}'", trimmed));
        }
        if trimmed.chars().any(|ch| ch == '/' || ch == '\\') {
            return Err(anyhow!("Name cannot contain path separators"));
        }
        Ok(trimmed.to_string())
    }

    fn command_copy(&mut self, target: &str) -> Result<()> {
        let entry = self
            .selected_entry()
            .cloned()
            .ok_or_else(|| anyhow!("No selection to copy"))?;
        let src = self
            .selected_path()
            .ok_or_else(|| anyhow!("No selection to copy"))?;
        let dest = self.compute_destination(target, &entry.name)?;
        if dest.exists() {
            return Err(anyhow!("Destination {} already exists", dest.display()));
        }
        if entry.is_dir {
            copy_directory(&src, &dest)?;
        } else {
            ensure_parent_dir(&dest)?;
            fs::copy(&src, &dest)
                .with_context(|| format!("copying {} to {}", entry.name, dest.display()))?;
        }
        self.refresh_with_message(
            false,
            format!("Copied {} to {}", entry.name, dest.display()),
        )?;
        Ok(())
    }

    fn command_move(&mut self, target: &str) -> Result<()> {
        let entry = self
            .selected_entry()
            .cloned()
            .ok_or_else(|| anyhow!("No selection to move"))?;
        let src = self
            .selected_path()
            .ok_or_else(|| anyhow!("No selection to move"))?;
        let dest = self.compute_destination(target, &entry.name)?;
        if dest.exists() {
            return Err(anyhow!("Destination {} already exists", dest.display()));
        }
        if let Err(err) = fs::rename(&src, &dest) {
            eprintln!(
                "rename failed {}; falling back to copy/remove: {err}",
                entry.name
            );
            if entry.is_dir {
                copy_directory(&src, &dest)?;
                fs::remove_dir_all(&src).with_context(|| format!("removing {}", entry.name))?;
            } else {
                ensure_parent_dir(&dest)?;
                fs::copy(&src, &dest)
                    .with_context(|| format!("copying {} to {}", entry.name, dest.display()))?;
                fs::remove_file(&src).with_context(|| format!("removing {}", entry.name))?;
            }
        }

        self.refresh_with_message(true, format!("Moved {} to {}", entry.name, dest.display()))?;
        Ok(())
    }
}

#[derive(Clone)]
struct FileEntry {
    name: String,
    is_dir: bool,
    size: Option<u64>,
    modified: Option<SystemTime>,
}

impl FileEntry {
    fn describe(&self) -> String {
        let kind = if self.is_dir { "Directory" } else { "File" };
        let size = self
            .size
            .map(|s| format!("{s} bytes"))
            .unwrap_or_else(|| "—".into());
        let modified = self
            .modified
            .and_then(|time| time.elapsed().ok())
            .map(|elapsed| format!("{:?} ago", elapsed))
            .unwrap_or_else(|| "unknown".into());
        format!(
            "{kind}\nName: {}\nSize: {}\nModified: {}",
            self.name, size, modified
        )
    }
}

type FsResult<T> = std::result::Result<T, String>;

enum FsEvent {
    DirectoryLoaded {
        path: PathBuf,
        token: u64,
        result: FsResult<Vec<FileEntry>>,
    },
}

#[derive(Clone)]
struct FsDispatcher {
    handle: Handle,
    event_tx: UnboundedSender<FsEvent>,
}

impl FsDispatcher {
    fn new(runtime: &Runtime) -> (Self, UnboundedReceiver<FsEvent>) {
        let (event_tx, event_rx) = unbounded_channel();
        let dispatcher = Self {
            handle: runtime.handle().clone(),
            event_tx,
        };
        (dispatcher, event_rx)
    }

    fn request_directory_scan(&self, path: PathBuf, token: u64) -> Result<()> {
        let tx = self.event_tx.clone();
        self.handle.spawn_blocking(move || {
            let result = read_directory(&path).map_err(|err| format!("{err:#}"));
            let _ = tx.send(FsEvent::DirectoryLoaded {
                path,
                token,
                result,
            });
        });
        Ok(())
    }
}

fn read_directory(dir: &Path) -> Result<Vec<FileEntry>> {
    let mut entries: Vec<FileEntry> = fs::read_dir(dir)
        .with_context(|| format!("read dir {}", dir.display()))?
        .filter_map(|res| match res {
            Ok(entry) => Some(entry),
            Err(err) => {
                eprintln!("Skipping entry: {err}");
                None
            }
        })
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata().ok()?;
            let size = (!meta.is_dir()).then_some(meta.len());
            Some(FileEntry {
                name,
                is_dir: meta.is_dir(),
                size,
                modified: meta.modified().ok(),
            })
        })
        .collect();

    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => cmp::Ordering::Less,
        (false, true) => cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    Ok(entries)
}

fn build_preview(entry: &FileEntry, path: &Path) -> Result<PreviewPane> {
    if entry.is_dir {
        return preview_directory(path);
    }
    preview_file(entry, path)
}

fn preview_directory(path: &Path) -> Result<PreviewPane> {
    let mut rows = Vec::new();
    let mut entries =
        fs::read_dir(path).with_context(|| format!("reading directory {}", path.display()))?;
    for item in entries.by_ref().flatten().take(PREVIEW_DIR_ENTRIES) {
        let name = item.file_name().to_string_lossy().into_owned();
        let is_dir = item.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        rows.push(format!("{} {}", if is_dir { "[D]" } else { "[F]" }, name));
    }
    let mut body = if rows.is_empty() {
        "Directory is empty".to_string()
    } else {
        rows.join("\n")
    };
    if entries.next().is_some() {
        if !body.is_empty() {
            body.push_str("\n...");
        } else {
            body = "...".into();
        }
    }
    Ok(PreviewPane::new("Preview", body))
}

fn preview_file(entry: &FileEntry, path: &Path) -> Result<PreviewPane> {
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", entry.name))?;
    let mut buffer = Vec::new();
    file.by_ref()
        .take(PREVIEW_MAX_BYTES as u64)
        .read_to_end(&mut buffer)
        .with_context(|| format!("reading {}", entry.name))?;

    if buffer.is_empty() {
        return Ok(PreviewPane::new("Preview", "<empty file>"));
    }

    if is_text_data(&buffer) {
        let mut body = String::new();
        for (idx, line) in String::from_utf8_lossy(&buffer).lines().enumerate() {
            if idx >= PREVIEW_MAX_LINES {
                body.push_str("\n...");
                break;
            }
            if idx > 0 {
                body.push('\n');
            }
            body.push_str(line);
        }
        return Ok(PreviewPane::new("Preview", body));
    }

    let file_type = describe_file_type(path);
    Ok(PreviewPane::new(
        "Preview",
        format!("Non-text file\nType: {}", file_type),
    ))
}

fn is_text_data(buffer: &[u8]) -> bool {
    !matches!(content_inspector::inspect(buffer), ContentType::BINARY)
}

fn describe_file_type(path: &Path) -> String {
    match infer::get_from_path(path) {
        Ok(Some(kind)) => format!("{} ({})", kind.mime_type(), kind.extension()),
        Ok(None) => "Unknown type".into(),
        Err(_) => "Unknown type".into(),
    }
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent {}", parent.display()))?;
    }
    Ok(())
}

fn copy_directory(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        return Err(anyhow!("Destination {} already exists", dest.display()));
    }
    ensure_parent_dir(dest)?;
    fs::create_dir(dest).with_context(|| format!("creating directory {}", dest.display()))?;
    let mut options = DirCopyOptions::new();
    options.copy_inside = true;
    copy_dir(src, dest, &options)
        .map(|_| ())
        .with_context(|| format!("copying {} to {}", src.display(), dest.display()))
}
