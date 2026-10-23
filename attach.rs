//! `claude-mux attach`: one session's terminal, full screen, rendered from
//! the daemon's emulator state. Keys go straight to claude; ctrl-esc
//! detaches.

use anyhow::Result;
use futures_util::StreamExt;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode as CtKey, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color as TColor, Modifier, Style, Stylize};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::client;
use crate::protocol::*;

/// Why `attach` returned, for the line printed once the screen is restored.
pub enum Detach {
    /// The user pressed the detach key.
    User(String),
    /// The session stopped running: exited, stopped or removed.
    Ended(String),
    Disconnected,
}

impl Detach {
    pub fn message(&self) -> String {
        match self {
            Detach::User(id) => format!("detached from {}", short_id(id)),
            Detach::Ended(why) => why.clone(),
            Detach::Disconnected => "lost connection to the daemon".into(),
        }
    }
}

struct View {
    id: String,
    screen: Option<Screen>,
    events: mpsc::UnboundedSender<Vec<u8>>,
    /// The size the daemon was last told to draw at.
    size: Option<(u16, u16)>,
    done: Option<Detach>,
}

pub async fn run(id: String) -> Result<Detach> {
    let mut conn = client::connect().await?;
    conn.send(&Request::Subscribe).await?;
    let (reader, writer) = conn.into_split();

    let (stream_tx, mut stream) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(ev) = serde_json::from_str::<StreamEvent>(&line)
                && stream_tx.send(ev).is_err()
            {
                return;
            }
        }
    });
    // Client events go through a channel so input handling never blocks.
    let (events, mut event_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        let mut writer = writer;
        while let Some(bytes) = event_rx.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
            let _ = writer.flush().await;
        }
    });

    let mut view = View {
        id,
        screen: None,
        events,
        size: None,
        done: None,
    };

    let mut term = ratatui::init();
    let mut out = std::io::stdout();
    let _ = execute!(out, EnableMouseCapture, EnableBracketedPaste);
    // Ctrl-esc is only told apart from esc under the kitty keyboard protocol.
    let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let result = view.event_loop(&mut term, &mut stream).await;
    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

impl View {
    async fn event_loop(
        &mut self,
        term: &mut DefaultTerminal,
        stream: &mut mpsc::UnboundedReceiver<StreamEvent>,
    ) -> Result<Detach> {
        let mut input = EventStream::new();
        loop {
            term.draw(|f| self.draw(f))?;
            if let Some(done) = self.done.take() {
                return Ok(done);
            }
            tokio::select! {
                ev = stream.recv() => match ev {
                    Some(StreamEvent::Sessions { sessions }) => self.on_sessions(&sessions),
                    Some(StreamEvent::Screen { id, screen }) if id == self.id => {
                        self.screen = Some(screen);
                    }
                    Some(StreamEvent::Screen { .. }) => {}
                    None => return Ok(Detach::Disconnected),
                },
                ev = input.next() => match ev {
                    Some(Ok(ev)) => self.on_event(ev),
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(Detach::User(self.id.clone())),
                },
            }
        }
    }

    fn send(&self, ev: &ClientEvent) {
        let Ok(mut line) = serde_json::to_vec(ev) else {
            return;
        };
        line.push(b'\n');
        let _ = self.events.send(line);
    }

    /// Tell the daemon to draw the session at the terminal's size.
    fn watch(&mut self, cols: u16, rows: u16) {
        if self.size != Some((cols, rows)) && cols > 0 && rows > 0 {
            self.send(&ClientEvent::Watch {
                id: Some(self.id.clone()),
                cols,
                rows,
            });
            self.size = Some((cols, rows));
        }
    }

    fn on_sessions(&mut self, sessions: &[SessionInfo]) {
        let Some(s) = sessions.iter().find(|s| s.id == self.id) else {
            self.done = Some(Detach::Ended(format!(
                "session {} was removed",
                short_id(&self.id)
            )));
            return;
        };
        if s.status.is_live() {
            return;
        }
        // Follow the claude process when a `/clear` moves it to a new session.
        if let Some(next) = &s.continued_as
            && sessions.iter().any(|n| &n.id == next && n.status.is_live())
        {
            self.id = next.clone();
            self.screen = None;
            if let Some((cols, rows)) = self.size.take() {
                self.watch(cols, rows);
            }
            return;
        }
        let why = match s.exit_code {
            Some(code) if s.status == Status::Exited => format!("exited with code {code}"),
            _ => s.status.label().to_string(),
        };
        self.done = Some(Detach::Ended(format!(
            "session {} {why}; `claude-mux resume -s {}` restarts it",
            s.short_id(),
            s.short_id()
        )));
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Paste(text) => self.send(&ClientEvent::Paste {
                id: self.id.clone(),
                text,
            }),
            Event::Mouse(m) => {
                let delta = match m.kind {
                    MouseEventKind::ScrollUp => -3,
                    MouseEventKind::ScrollDown => 3,
                    _ => return,
                };
                self.send(&ClientEvent::Scroll {
                    id: self.id.clone(),
                    delta,
                    col: m.column,
                    row: m.row,
                });
            }
            _ => {}
        }
    }

    /// Ctrl-esc, or ctrl-] in terminals that can't report ctrl-esc.
    fn is_detach(key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-] arrives as Ctrl-5 from terminals without the kitty protocol.
        ctrl && matches!(key.code, CtKey::Esc | CtKey::Char(']') | CtKey::Char('5'))
    }

    fn on_key(&mut self, key: KeyEvent) {
        if Self::is_detach(&key) {
            self.done = Some(Detach::User(self.id.clone()));
            return;
        }
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
        let key = KeyInput {
            code,
            shift: m.contains(KeyModifiers::SHIFT),
            ctrl: m.contains(KeyModifiers::CONTROL),
            alt: m.contains(KeyModifiers::ALT),
            sup: m.contains(KeyModifiers::SUPER),
        };
        self.send(&ClientEvent::Key {
            id: self.id.clone(),
            key,
        });
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        self.watch(area.width, area.height);
        match &self.screen {
            Some(screen) if screen.cols > 0 => {
                render_screen(f.buffer_mut(), area, screen);
                if let Some((x, y)) = screen.cursor
                    && x < area.width
                    && y < area.height
                    && !screen.scrolled
                {
                    f.set_cursor_position(Position::new(x, y));
                }
            }
            _ => {
                let mid = Rect::new(0, area.height / 2, area.width, 1.min(area.height));
                f.render_widget(
                    Paragraph::new("Connecting…").fg(TColor::Gray).centered(),
                    mid,
                );
            }
        }
    }
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

/// Paint a screen snapshot into the ratatui buffer.
fn render_screen(buf: &mut Buffer, area: Rect, screen: &Screen) {
    for (y, spans) in screen.lines.iter().enumerate() {
        if y as u16 >= area.height {
            break;
        }
        for span in spans {
            if span.x >= area.width {
                continue;
            }
            let style = screen
                .styles
                .get(span.s as usize)
                .map(to_style)
                .unwrap_or_default();
            let max = (area.width - span.x) as usize;
            buf.set_stringn(area.x + span.x, area.y + y as u16, &span.t, max, style);
        }
    }
}
