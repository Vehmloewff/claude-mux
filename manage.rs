//! `claudex manage`: a sidebar of every session plus a live view of the
//! selected session's terminal (rendered from the daemon's libghostty
//! state). Focus the terminal to type into claude directly.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode as CtKey, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color as TColor, Modifier, Style, Stylize};
use ratatui::text::{Line, Span as TSpan};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::client;
use crate::protocol::*;
use crate::{ago, short_path};

const SIDEBAR_WIDTH: u16 = 36;
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

enum AppMsg {
    Stream(StreamEvent),
    Disconnected,
    Flash(String, bool),
    Select(String),
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Focus {
    List,
    Terminal,
}

#[derive(Default)]
struct TextInput {
    text: String,
    /// Cursor position in chars.
    cursor: usize,
}

impl TextInput {
    fn new(text: &str) -> Self {
        TextInput { text: text.to_string(), cursor: text.chars().count() }
    }

    fn byte_pos(&self, char_pos: usize) -> usize {
        self.text.char_indices().nth(char_pos).map(|(i, _)| i).unwrap_or(self.text.len())
    }

    fn insert(&mut self, s: &str) {
        let at = self.byte_pos(self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    /// Returns false if the key was not an editing key.
    fn handle(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            CtKey::Char('u') if ctrl => {
                let at = self.byte_pos(self.cursor);
                self.text.replace_range(..at, "");
                self.cursor = 0;
            }
            CtKey::Char('w') if ctrl => self.delete_word(),
            CtKey::Backspace if alt => self.delete_word(),
            CtKey::Char('a') if ctrl => self.cursor = 0,
            CtKey::Char('e') if ctrl => self.cursor = self.text.chars().count(),
            CtKey::Char(c) if !ctrl => self.insert(&c.to_string()),
            CtKey::Backspace => {
                if self.cursor > 0 {
                    let start = self.byte_pos(self.cursor - 1);
                    let end = self.byte_pos(self.cursor);
                    self.text.replace_range(start..end, "");
                    self.cursor -= 1;
                }
            }
            CtKey::Delete => {
                if self.cursor < self.text.chars().count() {
                    let start = self.byte_pos(self.cursor);
                    let end = self.byte_pos(self.cursor + 1);
                    self.text.replace_range(start..end, "");
                }
            }
            CtKey::Left => self.cursor = self.cursor.saturating_sub(1),
            CtKey::Right => self.cursor = (self.cursor + 1).min(self.text.chars().count()),
            CtKey::Home => self.cursor = 0,
            CtKey::End => self.cursor = self.text.chars().count(),
            _ => return false,
        }
        true
    }

    fn delete_word(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let (a, b) = (self.byte_pos(start), self.byte_pos(self.cursor));
        self.text.replace_range(a..b, "");
        self.cursor = start;
    }
}

enum Modal {
    New { fields: [TextInput; 3], field: usize },
    Send { id: String, input: TextInput },
    Rename { id: String, input: TextInput },
    Confirm { prompt: String, action: Confirmed },
    Help,
}

enum Confirmed {
    Kill(String),
    Remove(String),
}

const NEW_FIELDS: [&str; 3] = ["Context / task", "Directory", "Name (optional)"];

struct App {
    sessions: Vec<SessionInfo>,
    selected: Option<String>,
    screen: Option<(String, Screen)>,
    focus: Focus,
    modal: Option<Modal>,
    flash: Option<(String, bool, Instant)>,
    tx: Sender<AppMsg>,
    stream: UnixStream,
    /// What the daemon was last told to watch.
    watching: Option<(Option<String>, u16, u16)>,
    pane: Rect,
    sidebar: Rect,
    started: Instant,
    quit: bool,
    connected: bool,
}

pub fn run(initial: Option<String>, attach: bool) -> Result<()> {
    let mut conn = client::connect()?;
    conn.send(&Request::Subscribe)?;
    let stream = conn.writer()?;
    let mut reader = conn.into_reader();

    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(ev) = serde_json::from_str::<StreamEvent>(&line)
                            && tx.send(AppMsg::Stream(ev)).is_err() {
                                return;
                            }
                    }
                }
            }
            let _ = tx.send(AppMsg::Disconnected);
        });
    }

    let mut app = App {
        sessions: Vec::new(),
        selected: initial,
        screen: None,
        focus: if attach { Focus::Terminal } else { Focus::List },
        modal: None,
        flash: None,
        tx,
        stream,
        watching: None,
        pane: Rect::default(),
        sidebar: Rect::default(),
        started: Instant::now(),
        quit: false,
        connected: true,
    };

    let mut term = ratatui::init();
    let mut out = std::io::stdout();
    let _ = execute!(out, EnableMouseCapture, EnableBracketedPaste);
    let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let result = app.event_loop(&mut term, rx);
    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

impl App {
    fn event_loop(&mut self, term: &mut DefaultTerminal, rx: Receiver<AppMsg>) -> Result<()> {
        let mut last_draw = Instant::now() - Duration::from_secs(1);
        let mut dirty = true;
        while !self.quit {
            while let Ok(msg) = rx.try_recv() {
                self.on_msg(msg);
                dirty = true;
            }
            // Keep spinners and relative times moving.
            if last_draw.elapsed() >= Duration::from_millis(100) {
                dirty = true;
            }
            if dirty {
                term.draw(|f| self.draw(f))?;
                self.sync_watch();
                last_draw = Instant::now();
                dirty = false;
            }
            if event::poll(Duration::from_millis(16))? {
                loop {
                    let ev = event::read()?;
                    self.on_event(ev);
                    if self.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
                dirty = true;
            }
        }
        Ok(())
    }

    fn on_msg(&mut self, msg: AppMsg) {
        match msg {
            AppMsg::Stream(StreamEvent::Sessions { mut sessions }) => {
                sessions.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
                self.sessions = sessions;
                let still_there = self.selected.as_ref().is_some_and(|id| self.sessions.iter().any(|s| &s.id == id));
                if !still_there {
                    self.selected = self.sessions.last().map(|s| s.id.clone());
                    if self.selected.is_none() {
                        self.focus = Focus::List;
                    }
                }
            }
            AppMsg::Stream(StreamEvent::Screen { id, screen }) => self.screen = Some((id, screen)),
            AppMsg::Disconnected => {
                self.connected = false;
                self.flash("lost connection to the daemon", true);
            }
            AppMsg::Flash(text, err) => self.flash(&text, err),
            AppMsg::Select(id) => {
                self.selected = Some(id);
                self.focus = Focus::Terminal;
            }
        }
    }

    fn flash(&mut self, text: &str, err: bool) {
        self.flash = Some((text.to_string(), err, Instant::now()));
    }

    fn send_event(&mut self, ev: &ClientEvent) {
        let Ok(mut line) = serde_json::to_vec(ev) else { return };
        line.push(b'\n');
        let _ = self.stream.write_all(&line);
    }

    /// Tell the daemon which session we show and at what size.
    fn sync_watch(&mut self) {
        let want = (self.selected.clone(), self.pane.width, self.pane.height);
        if self.watching.as_ref() != Some(&want) && want.1 > 0 && want.2 > 0 {
            self.send_event(&ClientEvent::Watch { id: want.0.clone(), cols: want.1, rows: want.2 });
            if self.watching.as_ref().map(|w| &w.0) != Some(&want.0) {
                self.screen = None;
            }
            self.watching = Some(want);
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.sessions.iter().position(|s| &s.id == id)
    }

    fn selected_session(&self) -> Option<&SessionInfo> {
        self.selected_index().map(|i| &self.sessions[i])
    }

    fn move_selection(&mut self, delta: isize) {
        if self.sessions.is_empty() {
            return;
        }
        let cur = self.selected_index().map(|i| i as isize).unwrap_or(-1);
        let next = (cur + delta).clamp(0, self.sessions.len() as isize - 1) as usize;
        self.selected = Some(self.sessions[next].id.clone());
    }

    /// Run a daemon request off the UI thread.
    fn command(&self, req: Request, ok_text: impl Into<String>, select_created: bool) {
        let tx = self.tx.clone();
        let ok_text = ok_text.into();
        std::thread::spawn(move || {
            let msg = match client::request(&req) {
                Ok(Response::Session { session }) if select_created => {
                    let _ = tx.send(AppMsg::Select(session.id.clone()));
                    AppMsg::Flash(format!("{ok_text} {}", session.id), false)
                }
                Ok(_) => AppMsg::Flash(ok_text, false),
                Err(e) => AppMsg::Flash(format!("{e:#}"), true),
            };
            let _ = tx.send(msg);
        });
    }

    // -----------------------------------------------------------------------
    // Input

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if self.modal.is_some() {
                    self.on_modal_key(key);
                } else if self.focus == Focus::Terminal {
                    self.on_terminal_key(key);
                } else {
                    self.on_list_key(key);
                }
            }
            Event::Paste(text) => {
                if let Some(modal) = self.modal.as_mut() {
                    let input = match modal {
                        Modal::New { fields, field } => Some(&mut fields[*field]),
                        Modal::Send { input, .. } | Modal::Rename { input, .. } => Some(input),
                        _ => None,
                    };
                    if let Some(input) = input {
                        input.insert(&text);
                    }
                } else if self.focus == Focus::Terminal
                    && let Some(id) = self.selected.clone() {
                        self.send_event(&ClientEvent::Paste { id, text });
                    }
            }
            Event::Mouse(m) => {
                let pos = Position::new(m.column, m.row);
                match m.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if self.pane.contains(pos) => {
                        if let Some(id) = self.selected.clone() {
                            let delta = if m.kind == MouseEventKind::ScrollUp { -3 } else { 3 };
                            let (col, row) = (m.column - self.pane.x, m.row - self.pane.y);
                            self.send_event(&ClientEvent::Scroll { id, delta, col, row });
                        }
                    }
                    MouseEventKind::ScrollUp if self.sidebar.contains(pos) => self.move_selection(-1),
                    MouseEventKind::ScrollDown if self.sidebar.contains(pos) => self.move_selection(1),
                    MouseEventKind::Down(MouseButton::Left) if self.modal.is_none() => {
                        if self.sidebar.contains(pos) {
                            let idx = (m.row.saturating_sub(self.sidebar.y + 1) / 3) as usize;
                            if let Some(s) = self.sessions.get(idx) {
                                self.selected = Some(s.id.clone());
                            }
                            self.focus = Focus::List;
                        } else if self.pane.contains(pos) && self.selected.is_some() {
                            self.focus = Focus::Terminal;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn is_detach(key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-] arrives as Ctrl-5 from terminals without the kitty protocol.
        ctrl && matches!(key.code, CtKey::Char(']') | CtKey::Char('5'))
    }

    fn on_terminal_key(&mut self, key: KeyEvent) {
        if Self::is_detach(&key) {
            self.focus = Focus::List;
            return;
        }
        let Some(id) = self.selected.clone() else {
            self.focus = Focus::List;
            return;
        };
        let m = key.modifiers;
        let code = match key.code {
            CtKey::Char(c) => KeyCode::Char(c),
            CtKey::Enter => KeyCode::Enter,
            CtKey::Tab => KeyCode::Tab,
            CtKey::BackTab => KeyCode::BackTab,
            CtKey::Backspace => KeyCode::Backspace,
            CtKey::Esc => KeyCode::Escape,
            CtKey::Up => KeyCode::Up,
            CtKey::Down => KeyCode::Down,
            CtKey::Left => KeyCode::Left,
            CtKey::Right => KeyCode::Right,
            CtKey::Home => KeyCode::Home,
            CtKey::End => KeyCode::End,
            CtKey::PageUp => KeyCode::PageUp,
            CtKey::PageDown => KeyCode::PageDown,
            CtKey::Insert => KeyCode::Insert,
            CtKey::Delete => KeyCode::Delete,
            CtKey::F(n) => KeyCode::F(n),
            _ => return,
        };
        let input = KeyInput {
            code,
            shift: m.contains(KeyModifiers::SHIFT),
            ctrl: m.contains(KeyModifiers::CONTROL),
            alt: m.contains(KeyModifiers::ALT),
            sup: m.contains(KeyModifiers::SUPER),
        };
        self.send_event(&ClientEvent::Key { id, key: input });
    }

    fn on_list_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let selected = self.selected_session().cloned();
        match key.code {
            CtKey::Char('q') => self.quit = true,
            CtKey::Char('c') if ctrl => self.quit = true,
            CtKey::Char('j') | CtKey::Down => self.move_selection(1),
            CtKey::Char('k') | CtKey::Up => self.move_selection(-1),
            CtKey::Char('g') | CtKey::Home => self.move_selection(-(self.sessions.len() as isize)),
            CtKey::Char('G') | CtKey::End => self.move_selection(self.sessions.len() as isize),
            CtKey::Char(c @ '1'..='9') => {
                if let Some(s) = self.sessions.get((c as u8 - b'1') as usize) {
                    self.selected = Some(s.id.clone());
                }
            }
            CtKey::Enter | CtKey::Char('i') | CtKey::Char('l') | CtKey::Right | CtKey::Tab => {
                if self.selected.is_some() {
                    self.focus = Focus::Terminal;
                }
            }
            CtKey::Char('n') | CtKey::Char('c') => {
                let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                self.modal = Some(Modal::New {
                    fields: [TextInput::default(), TextInput::new(&cwd), TextInput::default()],
                    field: 0,
                });
            }
            CtKey::Char('?') => self.modal = Some(Modal::Help),
            CtKey::PageUp | CtKey::PageDown => {
                if let Some(id) = self.selected.clone() {
                    let page = self.pane.height.max(2) as i32 - 1;
                    let delta = if key.code == CtKey::PageUp { -page } else { page };
                    self.send_event(&ClientEvent::Scroll { id, delta, col: 0, row: 0 });
                }
            }
            CtKey::Esc => {
                if let Some(id) = self.selected.clone() {
                    self.send_event(&ClientEvent::ScrollReset { id });
                }
            }
            _ => {}
        }
        let Some(s) = selected else { return };
        match key.code {
            CtKey::Char('s') | CtKey::Char('m') => {
                if s.status.is_live() {
                    self.modal = Some(Modal::Send { id: s.id.clone(), input: TextInput::default() });
                } else {
                    self.flash("session is not running (press r to resume)", true);
                }
            }
            CtKey::Char('x') if s.status.is_live() => {
                self.modal = Some(Modal::Confirm {
                    prompt: format!("Kill session {}? It can be resumed later.", s.id),
                    action: Confirmed::Kill(s.id.clone()),
                });
            }
            CtKey::Char('r') => {
                if s.status.is_live() {
                    self.flash("session is already running", true);
                } else {
                    self.command(Request::Resume { id: s.id.clone() }, format!("resumed {}", s.id), false);
                }
            }
            CtKey::Char('d') | CtKey::Delete => {
                let warn = if s.status.is_live() { " This kills the running claude." } else { "" };
                self.modal = Some(Modal::Confirm {
                    prompt: format!("Remove session {}?{warn}", s.id),
                    action: Confirmed::Remove(s.id.clone()),
                });
            }
            CtKey::Char('R') => {
                let current = s.name.clone().unwrap_or_default();
                self.modal = Some(Modal::Rename { id: s.id.clone(), input: TextInput::new(&current) });
            }
            _ => {}
        }
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.modal.take() else { return };
        let esc = key.code == CtKey::Esc || (key.code == CtKey::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL));
        if esc {
            return;
        }
        let keep = match &mut modal {
            Modal::Help => false,
            Modal::Confirm { action, .. } => {
                if matches!(key.code, CtKey::Char('y') | CtKey::Char('Y') | CtKey::Enter) {
                    match action {
                        Confirmed::Kill(id) => self.command(Request::Kill { id: id.clone() }, format!("killed {id}"), false),
                        Confirmed::Remove(id) => {
                            self.command(Request::Remove { id: id.clone() }, format!("removed {id}"), false)
                        }
                    }
                }
                false
            }
            Modal::Send { id, input } => match key.code {
                CtKey::Enter if !input.text.trim().is_empty() => {
                    let req = Request::Send { id: id.clone(), message: input.text.clone(), now: false };
                    self.command(req, format!("sent to {id}"), false);
                    false
                }
                _ => {
                    input.handle(&key);
                    true
                }
            },
            Modal::Rename { id, input } => match key.code {
                CtKey::Enter => {
                    let req = Request::Rename { id: id.clone(), name: Some(input.text.trim().to_string()) };
                    self.command(req, format!("renamed {id}"), false);
                    false
                }
                _ => {
                    input.handle(&key);
                    true
                }
            },
            Modal::New { fields, field } => match key.code {
                CtKey::Tab | CtKey::Down => {
                    *field = (*field + 1) % fields.len();
                    true
                }
                CtKey::BackTab | CtKey::Up => {
                    *field = (*field + fields.len() - 1) % fields.len();
                    true
                }
                CtKey::Enter => {
                    let cwd = fields[1].text.trim();
                    let cwd = match cwd.strip_prefix('~') {
                        Some(rest) => format!("{}{rest}", std::env::var("HOME").unwrap_or_default()),
                        None => cwd.to_string(),
                    };
                    let name = fields[2].text.trim();
                    let req = Request::Create(CreateRequest {
                        id: None,
                        name: (!name.is_empty()).then(|| name.to_string()),
                        cwd,
                        context: fields[0].text.trim().to_string(),
                        claude_args: Vec::new(),
                        trust: false,
                    });
                    self.command(req, "created", true);
                    false
                }
                _ => {
                    fields[*field].handle(&key);
                    true
                }
            },
        };
        if keep {
            self.modal = Some(modal);
        }
    }

    // -----------------------------------------------------------------------
    // Drawing

    fn draw(&mut self, f: &mut Frame) {
        let [main, bar] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
        let sidebar_w = SIDEBAR_WIDTH.min(main.width / 2);
        let [side, right] = Layout::horizontal([Constraint::Length(sidebar_w), Constraint::Min(10)]).areas(main);
        self.sidebar = side;
        self.draw_sidebar(f, side);
        self.draw_pane(f, right);
        self.draw_bar(f, bar);
        if self.modal.is_some() {
            self.draw_modal(f);
        }
    }

    fn status_icon(&self, status: Status) -> (String, Style) {
        let tick = (self.started.elapsed().as_millis() / 100) as usize;
        match status {
            Status::Starting => ("◌".into(), Style::new().fg(TColor::DarkGray)),
            Status::Working => (SPINNER[tick % SPINNER.len()].into(), Style::new().fg(TColor::Yellow)),
            Status::NeedsInput => ("◆".into(), Style::new().fg(TColor::Magenta).bold()),
            Status::Background => (["◴", "◷", "◶", "◵"][tick / 3 % 4].into(), Style::new().fg(TColor::Blue)),
            Status::Idle => ("●".into(), Style::new().fg(TColor::Green)),
            Status::Exited => ("✕".into(), Style::new().fg(TColor::DarkGray)),
            Status::Stopped => ("◼".into(), Style::new().fg(TColor::DarkGray)),
        }
    }

    fn draw_sidebar(&self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::List && self.modal.is_none();
        let counts = |pred: fn(Status) -> bool| self.sessions.iter().filter(|s| pred(s.effective_status())).count();
        let working = counts(|s| matches!(s, Status::Working | Status::Starting));
        let waiting = counts(|s| s == Status::NeedsInput);
        let mut title = vec![TSpan::raw(" claudex ").bold()];
        if working > 0 {
            title.push(TSpan::raw(format!("{working}⠿ ")).fg(TColor::Yellow));
        }
        if waiting > 0 {
            title.push(TSpan::raw(format!("{waiting}◆ ")).fg(TColor::Magenta));
        }
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(if focused { Style::new().fg(TColor::Cyan) } else { Style::new().fg(TColor::DarkGray) })
            .title(Line::from(title));
        let inner = block.inner(area);
        f.render_widget(block, area);

        if self.sessions.is_empty() {
            let msg = Paragraph::new(vec![
                Line::raw(""),
                Line::raw("No sessions yet.").fg(TColor::Gray),
                Line::raw(""),
                Line::from(vec![TSpan::raw("Press "), TSpan::raw("n").bold().fg(TColor::Cyan), TSpan::raw(" to start one,")]),
                Line::raw("or run:").fg(TColor::Gray),
                Line::raw("claudex create \"…\"").fg(TColor::Cyan),
            ])
            .wrap(Wrap { trim: true })
            .centered();
            f.render_widget(msg, inner);
            return;
        }

        let width = inner.width as usize;
        for (i, s) in self.sessions.iter().enumerate() {
            let y = inner.y + (i as u16) * 3;
            if y + 1 >= inner.y + inner.height {
                break;
            }
            let selected = self.selected.as_deref() == Some(&s.id);
            let row = Rect::new(inner.x, y, inner.width, 2);
            let base = if selected {
                Style::new().bg(if focused { TColor::Indexed(237) } else { TColor::Indexed(235) })
            } else {
                Style::new()
            };
            f.buffer_mut().set_style(row, base);

            let status = s.effective_status();
            let (icon, icon_style) = self.status_icon(status);
            let label = status.label();
            let num = if i < 9 { format!("{}", i + 1) } else { " ".into() };
            let name_w = width.saturating_sub(label.len() + 6);
            let name = fit(s.display_name(), name_w);
            let pad = width.saturating_sub(4 + display_width(&name) + label.len() + 1);
            let label_style = match status {
                Status::NeedsInput => Style::new().fg(TColor::Magenta),
                Status::Working => Style::new().fg(TColor::Yellow),
                Status::Background => Style::new().fg(TColor::Blue),
                Status::Idle => Style::new().fg(TColor::Green),
                _ => Style::new().fg(TColor::DarkGray),
            };
            let line1 = Line::from(vec![
                TSpan::styled(num, Style::new().fg(TColor::DarkGray)),
                TSpan::raw(" "),
                TSpan::styled(icon, icon_style),
                TSpan::raw(" "),
                TSpan::styled(name, if selected { Style::new().bold() } else { Style::new() }),
                TSpan::raw(" ".repeat(pad)),
                TSpan::styled(label, label_style),
            ]);
            let detail = match (&s.status_detail, status) {
                (Some(d), Status::NeedsInput | Status::Background | Status::Exited | Status::Stopped) => d.clone(),
                _ => {
                    let queued = if s.queued > 0 { format!(" · {} queued", s.queued) } else { String::new() };
                    let fixed = format!("{} ·  · {}{}", s.id, ago(s.updated_at), queued);
                    let room = width.saturating_sub(4 + display_width(&fixed));
                    format!("{} · {} · {}{}", s.id, fit_left(&short_path(&s.cwd), room), ago(s.updated_at), queued)
                }
            };
            let line2 = Line::from(TSpan::styled(
                format!("    {}", fit(&detail, width.saturating_sub(4))),
                Style::new().fg(TColor::DarkGray),
            ));
            f.render_widget(Paragraph::new(vec![line1, line2]).style(base), row);
        }
    }

    fn draw_pane(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Terminal && self.modal.is_none();
        let session = self.selected_session().cloned();
        let border = if focused { Style::new().fg(TColor::Cyan) } else { Style::new().fg(TColor::DarkGray) };
        let mut block = Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(border);
        if let Some(s) = &session {
            let (icon, icon_style) = self.status_icon(s.effective_status());
            let mut title = vec![
                TSpan::raw(" "),
                TSpan::styled(icon, icon_style),
                TSpan::raw(" "),
                TSpan::raw(s.display_name().to_string()).bold(),
                TSpan::raw(format!(" · {} ", s.id)).fg(TColor::DarkGray),
            ];
            if let Some((_, screen)) = &self.screen
                && screen.scrolled {
                    title.push(TSpan::raw("[scrollback · esc to return] ").fg(TColor::Yellow));
                }
            let used: usize = title.iter().map(|t| display_width(&t.content)).sum();
            let room = (area.width as usize).saturating_sub(used + 6);
            block = block.title(Line::from(title));
            if room > 8 {
                let cwd = format!(" {} ", fit_left(&short_path(&s.cwd), room - 2));
                block = block.title_top(Line::raw(cwd).fg(TColor::DarkGray).right_aligned());
            }
        } else {
            block = block.title(" no session ");
        }
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.pane = inner;

        let Some(s) = session else { return };
        let screen = self.screen.as_ref().filter(|(id, _)| id == &s.id).map(|(_, sc)| sc);
        match screen {
            Some(screen) if screen.cols > 0 => {
                render_screen(f.buffer_mut(), inner, screen);
                if focused
                    && let Some((x, y)) = screen.cursor
                        && x < inner.width && y < inner.height && !screen.scrolled {
                            f.set_cursor_position(Position::new(inner.x + x, inner.y + y));
                        }
            }
            _ => {
                let text = match s.status {
                    Status::Stopped | Status::Exited => format!(
                        "Session is {}.\n\nPress r to resume it, or d to remove it.",
                        s.status.label()
                    ),
                    _ => "Connecting…".into(),
                };
                let p = Paragraph::new(text).fg(TColor::Gray).centered().wrap(Wrap { trim: true });
                let mid = Rect::new(inner.x, inner.y + inner.height / 3, inner.width, inner.height.saturating_sub(inner.height / 3));
                f.render_widget(p, mid);
            }
        }
    }

    fn draw_bar(&mut self, f: &mut Frame, area: Rect) {
        if let Some((text, err, at)) = &self.flash
            && at.elapsed() < Duration::from_secs(4) {
                let style = if *err { Style::new().fg(TColor::Red) } else { Style::new().fg(TColor::Green) };
                f.render_widget(Paragraph::new(format!(" {text}")).style(style), area);
                return;
            }
        let key = |k: &'static str| TSpan::raw(k).fg(TColor::Cyan).bold();
        let txt = |t: &'static str| TSpan::raw(t).fg(TColor::Gray);
        let line = if !self.connected {
            Line::from(vec![TSpan::raw(" daemon disconnected — restart manage ").fg(TColor::Red), key("q"), txt(" quit")])
        } else if self.focus == Focus::Terminal {
            Line::from(vec![
                TSpan::raw(" TERMINAL ").bold().fg(TColor::Black).bg(TColor::Cyan),
                txt("  typing goes to claude · "),
                key("ctrl-]"),
                txt(" back to sessions"),
            ])
        } else {
            Line::from(vec![
                TSpan::raw(" SESSIONS ").bold().fg(TColor::Black).bg(TColor::Green),
                TSpan::raw("  "),
                key("enter"),
                txt(" focus  "),
                key("n"),
                txt(" new  "),
                key("s"),
                txt(" send  "),
                key("x"),
                txt(" kill  "),
                key("r"),
                txt(" resume  "),
                key("d"),
                txt(" delete  "),
                key("?"),
                txt(" help  "),
                key("q"),
                txt(" quit"),
            ])
        };
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_modal(&self, f: &mut Frame) {
        let area = f.area();
        let Some(modal) = &self.modal else { return };
        let w = area.width.saturating_sub(8).min(84);
        let (title, h) = match modal {
            Modal::New { .. } => (" New session ", 16),
            Modal::Send { .. } => (" Send message ", 9),
            Modal::Rename { .. } => (" Rename session ", 6),
            Modal::Confirm { .. } => (" Confirm ", 5),
            Modal::Help => (" Keys ", 22),
        };
        let h = h.min(area.height.saturating_sub(2));
        let rect = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height.saturating_sub(h)) / 3, w, h);
        f.render_widget(Clear, rect);
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(TColor::Cyan))
            .title(Line::raw(title).bold());
        let inner = block.inner(rect).inner(ratatui::layout::Margin::new(1, 0));
        f.render_widget(block, rect);
        let hint = |text: &'static str| Line::raw(text).fg(TColor::DarkGray);

        match modal {
            Modal::New { fields, field } => {
                let heights = [6u16, 1, 1];
                let mut y = inner.y;
                for (i, input) in fields.iter().enumerate() {
                    let active = i == *field;
                    let label_style = if active { Style::new().fg(TColor::Cyan).bold() } else { Style::new().fg(TColor::Gray) };
                    f.render_widget(Paragraph::new(NEW_FIELDS[i]).style(label_style), Rect::new(inner.x, y, inner.width, 1));
                    y += 1;
                    let r = Rect::new(inner.x, y, inner.width, heights[i]);
                    draw_input(f, r, input, active);
                    y += heights[i] + 1;
                }
                f.render_widget(
                    Paragraph::new(hint("tab next field · enter create · esc cancel")),
                    Rect::new(inner.x, inner.y + inner.height.saturating_sub(1), inner.width, 1),
                );
            }
            Modal::Send { id, input } => {
                f.render_widget(
                    Paragraph::new(format!("To {id} (delivered when claude is idle)")).fg(TColor::Gray),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                draw_input(f, Rect::new(inner.x, inner.y + 1, inner.width, 4), input, true);
                f.render_widget(
                    Paragraph::new(hint("enter send · esc cancel")),
                    Rect::new(inner.x, inner.y + inner.height.saturating_sub(1), inner.width, 1),
                );
            }
            Modal::Rename { input, .. } => {
                draw_input(f, Rect::new(inner.x, inner.y + 1, inner.width, 1), input, true);
                f.render_widget(
                    Paragraph::new(hint("enter save (empty clears) · esc cancel")),
                    Rect::new(inner.x, inner.y + inner.height.saturating_sub(1), inner.width, 1),
                );
            }
            Modal::Confirm { prompt, .. } => {
                let p = Paragraph::new(vec![
                    Line::raw(prompt.as_str()),
                    Line::from(vec![TSpan::raw("y").bold().fg(TColor::Cyan), TSpan::raw(" yes   "), TSpan::raw("n/esc").bold().fg(TColor::Cyan), TSpan::raw(" no")]),
                ])
                .wrap(Wrap { trim: true });
                f.render_widget(p, inner.inner(ratatui::layout::Margin::new(0, 1)));
            }
            Modal::Help => {
                let rows: &[(&str, &str)] = &[
                    ("j / k, ↑ / ↓", "select session"),
                    ("1-9", "jump to session"),
                    ("enter / tab", "focus the terminal (type into claude)"),
                    ("ctrl-]", "leave the terminal, back to the list"),
                    ("n", "new session"),
                    ("s", "send a message (queued until idle)"),
                    ("x", "kill session (resumable)"),
                    ("r", "resume a stopped/exited session"),
                    ("d", "remove session"),
                    ("R", "rename session"),
                    ("pgup / pgdn", "scroll the terminal"),
                    ("mouse wheel", "scroll · click to select/focus"),
                    ("esc", "back to live output"),
                    ("q", "quit (sessions keep running)"),
                ];
                let mut lines: Vec<Line> = rows
                    .iter()
                    .map(|(k, v)| Line::from(vec![TSpan::raw(format!("{k:<16}")).fg(TColor::Cyan).bold(), TSpan::raw(*v)]))
                    .collect();
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    TSpan::raw("◌ ").fg(TColor::DarkGray), TSpan::raw("starting  "),
                    TSpan::raw("⠋ ").fg(TColor::Yellow), TSpan::raw("working  "),
                    TSpan::raw("◆ ").fg(TColor::Magenta), TSpan::raw("needs input  "),
                    TSpan::raw("◷ ").fg(TColor::Blue), TSpan::raw("background tasks"),
                ]));
                lines.push(Line::from(vec![
                    TSpan::raw("● ").fg(TColor::Green), TSpan::raw("idle      "),
                    TSpan::raw("✕ ").fg(TColor::DarkGray), TSpan::raw("exited   "),
                    TSpan::raw("◼ ").fg(TColor::DarkGray), TSpan::raw("stopped"),
                ]));
                f.render_widget(Paragraph::new(lines), inner.inner(ratatui::layout::Margin::new(0, 1)));
            }
        }
    }
}

fn draw_input(f: &mut Frame, area: Rect, input: &TextInput, active: bool) {
    let bg = if active { TColor::Indexed(236) } else { TColor::Indexed(234) };
    f.buffer_mut().set_style(area, Style::new().bg(bg));
    // Simple character wrapping so the cursor position is easy to compute.
    let width = area.width.max(1) as usize;
    let chars: Vec<char> = input.text.chars().collect();
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    let mut cursor_pos = (0u16, 0u16);
    for (i, c) in chars.iter().enumerate() {
        if i == input.cursor {
            cursor_pos = (cur_w as u16, lines.len() as u16);
        }
        let cw = unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0);
        if *c == '\n' || cur_w + cw > width {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
            if *c == '\n' {
                continue;
            }
            if i == input.cursor {
                cursor_pos = (0, lines.len() as u16);
            }
        }
        cur.push(*c);
        cur_w += cw;
    }
    if input.cursor >= chars.len() {
        if cur_w >= width {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cursor_pos = (cur_w as u16, lines.len() as u16);
    }
    lines.push(cur);
    let skip = (cursor_pos.1 + 1).saturating_sub(area.height) as usize;
    let visible: Vec<Line> = lines.into_iter().skip(skip).map(Line::raw).collect();
    f.render_widget(Paragraph::new(visible).style(Style::new().bg(bg)), area);
    if active {
        f.set_cursor_position(Position::new(area.x + cursor_pos.0, area.y + cursor_pos.1 - skip as u16));
    }
}

fn display_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Truncate to a display width, adding an ellipsis when cut.
fn fit(s: &str, width: usize) -> String {
    let s = s.lines().next().unwrap_or("");
    if display_width(s) <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Truncate from the left (for paths), keeping the end visible.
fn fit_left(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    let mut out: Vec<char> = Vec::new();
    let mut w = 0;
    for c in s.chars().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out.into_iter().rev().collect()
}

fn to_color(c: Color) -> TColor {
    match c {
        Color::Indexed(i) => TColor::Indexed(i),
        Color::Rgb([r, g, b]) => TColor::Rgb(r, g, b),
    }
}

fn to_style(cs: &CellStyle) -> Style {
    let mut style = Style::new();
    if let Some(fg) = cs.fg {
        style = style.fg(to_color(fg));
    }
    if let Some(bg) = cs.bg {
        style = style.bg(to_color(bg));
    }
    let mut m = Modifier::empty();
    if cs.a & ATTR_BOLD != 0 {
        m |= Modifier::BOLD;
    }
    if cs.a & ATTR_ITALIC != 0 {
        m |= Modifier::ITALIC;
    }
    if cs.a & ATTR_UNDERLINE != 0 {
        m |= Modifier::UNDERLINED;
    }
    if cs.a & ATTR_INVERSE != 0 {
        m |= Modifier::REVERSED;
    }
    if cs.a & ATTR_FAINT != 0 {
        m |= Modifier::DIM;
    }
    if cs.a & ATTR_STRIKE != 0 {
        m |= Modifier::CROSSED_OUT;
    }
    if cs.a & ATTR_INVISIBLE != 0 {
        m |= Modifier::HIDDEN;
    }
    style.add_modifier(m)
}

/// Paint a libghostty screen snapshot into the ratatui buffer.
fn render_screen(buf: &mut Buffer, area: Rect, screen: &Screen) {
    for (y, spans) in screen.lines.iter().enumerate() {
        if y as u16 >= area.height {
            break;
        }
        for span in spans {
            if span.x >= area.width {
                continue;
            }
            let style = screen.styles.get(span.s as usize).map(to_style).unwrap_or_default();
            let max = (area.width - span.x) as usize;
            buf.set_stringn(area.x + span.x, area.y + y as u16, &span.t, max, style);
        }
    }
}
