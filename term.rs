//! An alacritty_terminal emulator for one session.
//!
//! Each session's terminal lives on its own thread and is driven through a
//! channel (`TermHandle`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{self, NamedColor, Processor, Rgb, StdSyncHandler};

use crate::protocol::{self, CellStyle, Color, KeyCode, KeyInput, Screen, Span};

pub type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Something the terminal noticed that the session manager cares about.
#[derive(Debug)]
pub enum TermEvent {
    Title(String),
    /// A blocking dialog is (Some) or is no longer (None) on screen.
    Prompt(Option<String>),
    /// Claude's footer does (true) or does not (false) show running
    /// background tasks.
    Background(bool),
    /// Screen contents changed.
    Dirty,
}

enum Msg {
    Output(Vec<u8>),
    Resize(u16, u16),
    Key(KeyInput),
    Paste(String),
    Scroll { delta: i32, col: u16, row: u16 },
    ScrollReset,
    Snapshot(Sender<Screen>),
    Text { scrollback: bool, ansi: bool, reply: Sender<String> },
}

#[derive(Clone)]
pub struct TermHandle {
    tx: Sender<Msg>,
}

impl TermHandle {
    pub fn spawn(
        cols: u16,
        rows: u16,
        writer: PtyWriter,
        on_event: impl Fn(TermEvent) + Send + 'static,
    ) -> TermHandle {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("claude-mux-term".into())
            .spawn(move || run(cols, rows, writer, rx, on_event))
            .expect("spawn terminal thread");
        TermHandle { tx }
    }

    pub fn output(&self, data: Vec<u8>) {
        let _ = self.tx.send(Msg::Output(data));
    }
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(Msg::Resize(cols, rows));
    }
    pub fn key(&self, key: KeyInput) {
        let _ = self.tx.send(Msg::Key(key));
    }
    pub fn paste(&self, text: String) {
        let _ = self.tx.send(Msg::Paste(text));
    }
    pub fn scroll(&self, delta: i32, col: u16, row: u16) {
        let _ = self.tx.send(Msg::Scroll { delta, col, row });
    }
    pub fn scroll_reset(&self) {
        let _ = self.tx.send(Msg::ScrollReset);
    }
    pub fn snapshot(&self) -> Option<Screen> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Msg::Snapshot(tx)).ok()?;
        rx.recv_timeout(Duration::from_secs(2)).ok()
    }
    pub fn text(&self, scrollback: bool, ansi: bool) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Msg::Text { scrollback, ansi, reply: tx }).ok()?;
        rx.recv_timeout(Duration::from_secs(2)).ok()
    }
}

/// Collects the terminal's events so they can be handled once it's done
/// parsing (some need the terminal itself to answer).
#[derive(Clone, Default)]
struct Events(Rc<RefCell<Vec<Event>>>);

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        self.0.borrow_mut().push(event);
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

struct Emu {
    term: Term<Events>,
    parser: Processor<StdSyncHandler>,
    events: Events,
    writer: PtyWriter,
    /// Set when the title changed since it was last reported.
    title: Option<String>,
}

impl Emu {
    fn new(cols: u16, rows: u16, scrollback: usize, writer: PtyWriter) -> Emu {
        let config = Config { scrolling_history: scrollback, kitty_keyboard: true, ..Config::default() };
        let size = Size { cols: cols as usize, rows: rows as usize };
        let events = Events::default();
        Emu {
            term: Term::new(config, &size, events.clone()),
            parser: Processor::new(),
            events,
            writer,
            title: None,
        }
    }

    fn vt_write(&mut self, data: &[u8]) {
        self.parser.advance(&mut self.term, data);
        self.handle_events();
    }

    /// When a synchronized update that hasn't finished must be flushed.
    fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    fn flush_sync(&mut self) {
        self.parser.stop_sync(&mut self.term);
        self.handle_events();
    }

    fn handle_events(&mut self) {
        let events = std::mem::take(&mut *self.events.0.borrow_mut());
        for event in events {
            match event {
                // Replies to queries (device attributes, cursor position, ...).
                Event::PtyWrite(text) => write_pty(self, text.as_bytes()),
                Event::ColorRequest(index, format) => {
                    let color = self.term.colors()[index].unwrap_or_else(|| default_color(index));
                    write_pty(self, format(color).as_bytes());
                }
                Event::TextAreaSizeRequest(format) => {
                    let size = WindowSize {
                        num_lines: self.term.screen_lines() as u16,
                        num_cols: self.term.columns() as u16,
                        cell_width: 8,
                        cell_height: 16,
                    };
                    write_pty(self, format(size).as_bytes());
                }
                Event::Title(title) => self.title = Some(title),
                Event::ResetTitle => self.title = Some(String::new()),
                _ => {}
            }
        }
    }
}

fn run(cols: u16, rows: u16, writer: PtyWriter, rx: Receiver<Msg>, on_event: impl Fn(TermEvent)) {
    let mut emu = Emu::new(cols, rows, 10_000, writer);

    let mut last_scan = Instant::now() - Duration::from_secs(1);
    let mut scan_pending = false;
    let mut prompt: Option<String> = None;
    let mut background = false;

    loop {
        let now = Instant::now();
        let mut deadline = now + Duration::from_secs(3600);
        if scan_pending {
            deadline = deadline.min(now + Duration::from_millis(150));
        }
        if let Some(sync) = emu.sync_deadline() {
            deadline = deadline.min(sync);
        }
        match rx.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok(Msg::Output(data)) => {
                emu.vt_write(&data);
                // Drain any further queued output before notifying.
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        Msg::Output(more) => emu.vt_write(&more),
                        other => {
                            handle(&mut emu, other);
                        }
                    }
                }
                if let Some(title) = emu.title.take() {
                    on_event(TermEvent::Title(clean_title(&title)));
                }
                on_event(TermEvent::Dirty);
                scan_pending = true;
            }
            Ok(msg) => {
                if handle(&mut emu, msg) {
                    on_event(TermEvent::Dirty);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        // A synchronized update that never ended: show what we have.
        if emu.sync_deadline().is_some_and(|t| t <= Instant::now()) {
            emu.flush_sync();
            on_event(TermEvent::Dirty);
            scan_pending = true;
        }

        // Look for blocking dialogs, throttled since it walks the screen.
        if scan_pending && last_scan.elapsed() >= Duration::from_millis(150) {
            scan_pending = false;
            last_scan = Instant::now();
            let text = viewport_text(&emu);
            let found = detect_prompt(&text);
            if found != prompt {
                prompt = found.clone();
                on_event(TermEvent::Prompt(found));
            }
            let bg = detect_background(&text);
            if bg != background {
                background = bg;
                on_event(TermEvent::Background(bg));
            }
        }
    }
}

/// Claude prefixes its title with a spinner glyph; drop it.
fn clean_title(title: &str) -> String {
    title
        .trim_start_matches(|c: char| !c.is_alphanumeric() && !c.is_ascii_punctuation())
        .trim()
        .to_string()
}

/// Returns true if the screen may have changed.
fn handle(emu: &mut Emu, msg: Msg) -> bool {
    match msg {
        Msg::Output(data) => {
            emu.vt_write(&data);
            true
        }
        Msg::Resize(cols, rows) => {
            let cols = cols.max(10) as usize;
            let rows = rows.max(3) as usize;
            if emu.term.columns() != cols || emu.term.screen_lines() != rows {
                emu.term.resize(Size { cols, rows });
            }
            true
        }
        Msg::Key(input) => {
            let bytes = encode_key(*emu.term.mode(), &input);
            emu.term.scroll_display(Scroll::Bottom);
            write_pty(emu, &bytes);
            true
        }
        Msg::Paste(text) => {
            let bytes = encode_paste(&text, emu.term.mode().contains(TermMode::BRACKETED_PASTE));
            emu.term.scroll_display(Scroll::Bottom);
            write_pty(emu, &bytes);
            true
        }
        Msg::Scroll { delta, col, row } => {
            let mode = *emu.term.mode();
            if mode.intersects(TermMode::MOUSE_MODE) {
                let bytes = encode_wheel(mode, delta, col, row);
                write_pty(emu, &bytes);
            } else {
                // Positive deltas scroll alacritty's viewport up.
                emu.term.scroll_display(Scroll::Delta(-delta));
            }
            true
        }
        Msg::ScrollReset => {
            emu.term.scroll_display(Scroll::Bottom);
            true
        }
        Msg::Snapshot(reply) => {
            let _ = reply.send(snapshot(emu));
            false
        }
        Msg::Text { scrollback, ansi, reply } => {
            let text = if scrollback || ansi { format_screen(emu, scrollback, ansi) } else { viewport_text(emu) };
            let _ = reply.send(text);
            false
        }
    }
}

fn write_pty(emu: &Emu, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Ok(mut w) = emu.writer.lock() {
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }
}

/// The xterm modifier parameter: 1 + shift(1) | alt(2) | ctrl(4) | super(8).
fn mod_param(input: &KeyInput) -> u8 {
    1 + input.shift as u8 + 2 * input.alt as u8 + 4 * input.ctrl as u8 + 8 * input.sup as u8
}

/// Encode a key press the way xterm does, or with the kitty keyboard
/// protocol when the application has turned it on.
fn encode_key(mode: TermMode, input: &KeyInput) -> Vec<u8> {
    let mut input = input.clone();
    if input.code == KeyCode::BackTab {
        input.code = KeyCode::Tab;
        input.shift = true;
    }
    let kitty = mode.contains(TermMode::DISAMBIGUATE_ESC_CODES);
    let all_as_esc = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    let m = mod_param(&input);
    // Modifiers other than shift, which only changes the text of a char.
    let modified = input.ctrl || input.alt || input.sup;
    let csi_u = |code: u32, m: u8| {
        if m == 1 { format!("\x1b[{code}u") } else { format!("\x1b[{code};{m}u") }.into_bytes()
    };
    let esc = |mut bytes: Vec<u8>, alt: bool| {
        if alt {
            bytes.insert(0, 0x1b);
        }
        bytes
    };

    match input.code {
        KeyCode::Char(c) => {
            let base = unshifted(c);
            if all_as_esc || (kitty && modified) {
                return csi_u(base as u32, m);
            }
            if input.ctrl
                && let Some(b) = ctrl_byte(base)
            {
                return esc(vec![b], input.alt);
            }
            esc(c.to_string().into_bytes(), input.alt)
        }
        KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Escape => {
            let (code, plain) = match input.code {
                KeyCode::Enter => (13, b'\r'),
                KeyCode::Tab => (9, b'\t'),
                KeyCode::Backspace => (127, 0x7f),
                _ => (27, 0x1b),
            };
            if all_as_esc || (kitty && (m != 1 || input.code == KeyCode::Escape)) {
                return csi_u(code, m);
            }
            match input.code {
                KeyCode::Tab if input.shift => b"\x1b[Z".to_vec(),
                KeyCode::Backspace if input.ctrl => esc(vec![0x08], input.alt),
                _ => esc(vec![plain], input.alt),
            }
        }
        KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left | KeyCode::Home | KeyCode::End => {
            let letter = match input.code {
                KeyCode::Up => 'A',
                KeyCode::Down => 'B',
                KeyCode::Right => 'C',
                KeyCode::Left => 'D',
                KeyCode::Home => 'H',
                _ => 'F',
            };
            if m != 1 {
                format!("\x1b[1;{m}{letter}").into_bytes()
            } else if mode.contains(TermMode::APP_CURSOR) {
                format!("\x1bO{letter}").into_bytes()
            } else {
                format!("\x1b[{letter}").into_bytes()
            }
        }
        KeyCode::PageUp | KeyCode::PageDown | KeyCode::Insert | KeyCode::Delete => {
            let n = match input.code {
                KeyCode::Insert => 2,
                KeyCode::Delete => 3,
                KeyCode::PageUp => 5,
                _ => 6,
            };
            tilde(n, m)
        }
        KeyCode::F(n @ 1..=4) => {
            let letter = (b'P' + n - 1) as char;
            if m != 1 { format!("\x1b[1;{m}{letter}") } else { format!("\x1bO{letter}") }.into_bytes()
        }
        KeyCode::F(n @ 5..=12) => {
            const CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            tilde(CODES[(n - 5) as usize], m)
        }
        KeyCode::F(_) | KeyCode::BackTab => Vec::new(),
    }
}

fn tilde(n: u8, m: u8) -> Vec<u8> {
    if m != 1 { format!("\x1b[{n};{m}~") } else { format!("\x1b[{n}~") }.into_bytes()
}

/// The control code a ctrl+key chord produces in a legacy terminal.
fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        '@' | ' ' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '/' | '7' => 0x1f,
        '8' => 0x7f,
        _ => return None,
    })
}

/// The character the same key gives without shift on a US layout.
fn unshifted(c: char) -> char {
    if c.is_ascii_uppercase() {
        return c.to_ascii_lowercase();
    }
    const SHIFTED: &str = "~!@#$%^&*()_+{}|:\"<>?";
    const BASE: &str = "`1234567890-=[]\\;',./";
    SHIFTED.find(c).and_then(|i| BASE.chars().nth(i)).unwrap_or(c)
}

fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        // Drop escapes so the text can't end the paste early.
        let clean: String = text.chars().filter(|&c| c != '\x1b').collect();
        format!("\x1b[200~{clean}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

fn encode_wheel(mode: TermMode, delta: i32, col: u16, row: u16) -> Vec<u8> {
    let button: u32 = if delta < 0 { 64 } else { 65 };
    let (x, y) = (col as u32 + 1, row as u32 + 1);
    let event = if mode.contains(TermMode::SGR_MOUSE) {
        format!("\x1b[<{button};{x};{y}M").into_bytes()
    } else if mode.contains(TermMode::UTF8_MOUSE) {
        let mut s = String::from("\x1b[M");
        for v in [button, x, y] {
            s.push(char::from_u32(32 + v.min(2015)).unwrap_or(' '));
        }
        s.into_bytes()
    } else {
        let byte = |v: u32| (32 + v).min(255) as u8;
        vec![0x1b, b'[', b'M', byte(button), byte(x), byte(y)]
    };
    event.repeat(delta.unsigned_abs().min(10) as usize)
}

/// Default colors, reported when an application asks for them.
fn default_color(index: usize) -> Rgb {
    const ANSI: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00), (0xcd, 0x00, 0x00), (0x00, 0xcd, 0x00), (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee), (0xcd, 0x00, 0xcd), (0x00, 0xcd, 0xcd), (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f), (0xff, 0x00, 0x00), (0x00, 0xff, 0x00), (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff), (0xff, 0x00, 0xff), (0x00, 0xff, 0xff), (0xff, 0xff, 0xff),
    ];
    let (r, g, b) = match index {
        0..16 => ANSI[index],
        16..232 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            (level(i / 36), level(i / 6 % 6), level(i % 6))
        }
        232..256 => {
            let v = (8 + (index - 232) * 10) as u8;
            (v, v, v)
        }
        i if i == NamedColor::Background as usize => (0x28, 0x2c, 0x34),
        _ => (0xff, 0xff, 0xff),
    };
    Rgb { r, g, b }
}

fn color_of(c: ansi::Color) -> Option<Color> {
    match c {
        ansi::Color::Spec(rgb) => Some(Color::Rgb([rgb.r, rgb.g, rgb.b])),
        ansi::Color::Indexed(i) => Some(Color::Indexed(i)),
        ansi::Color::Named(n) => {
            let n = n as usize;
            let dim = NamedColor::DimBlack as usize;
            match n {
                0..16 => Some(Color::Indexed(n as u8)),
                _ if (dim..dim + 8).contains(&n) => Some(Color::Indexed((n - dim) as u8)),
                _ => None,
            }
        }
    }
}

fn style_of(cell: &Cell) -> CellStyle {
    const ATTRS: [(Flags, u8); 7] = [
        (Flags::BOLD, protocol::ATTR_BOLD),
        (Flags::ITALIC, protocol::ATTR_ITALIC),
        (Flags::ALL_UNDERLINES, protocol::ATTR_UNDERLINE),
        (Flags::INVERSE, protocol::ATTR_INVERSE),
        (Flags::DIM, protocol::ATTR_FAINT),
        (Flags::STRIKEOUT, protocol::ATTR_STRIKE),
        (Flags::HIDDEN, protocol::ATTR_INVISIBLE),
    ];
    let a = ATTRS.iter().filter(|(f, _)| cell.flags.intersects(*f)).fold(0, |a, (_, bit)| a | bit);
    CellStyle { fg: color_of(cell.fg), bg: color_of(cell.bg), a }
}

/// The cells of one line: (column, grapheme, cell), skipping wide-char
/// spacers.
fn line_cells(emu: &Emu, line: Line) -> impl Iterator<Item = (u16, String, &Cell)> {
    let row = &emu.term.grid()[line];
    (0..emu.term.columns()).filter_map(move |x| {
        let cell = &row[Column(x)];
        if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            return None;
        }
        let mut grapheme = String::from(cell.c);
        grapheme.extend(cell.zerowidth().into_iter().flatten());
        Some((x as u16, grapheme, cell))
    })
}

/// The grid lines currently in the viewport, top to bottom.
fn viewport_lines(emu: &Emu) -> impl Iterator<Item = Line> {
    let offset = emu.term.grid().display_offset() as i32;
    (0..emu.term.screen_lines() as i32).map(move |y| Line(y - offset))
}

fn snapshot(emu: &Emu) -> Screen {
    let cols = emu.term.columns() as u16;
    let rows = emu.term.screen_lines() as u16;
    let offset = emu.term.grid().display_offset();
    let cursor = if emu.term.mode().contains(TermMode::SHOW_CURSOR) {
        let point = emu.term.grid().cursor.point;
        let y = point.line.0 + offset as i32;
        (0..rows as i32).contains(&y).then_some((point.column.0 as u16, y as u16))
    } else {
        None
    };

    let mut styles: Vec<CellStyle> = vec![CellStyle::default()];
    let mut style_ids: HashMap<CellStyle, u16> = HashMap::from([(CellStyle::default(), 0)]);
    let mut lines = Vec::with_capacity(rows as usize);

    for line in viewport_lines(emu) {
        let mut spans: Vec<Span> = Vec::new();
        for (col, grapheme, cell) in line_cells(emu, line) {
            let cs = style_of(cell);
            let sid = *style_ids.entry(cs).or_insert_with(|| {
                styles.push(cs);
                (styles.len() - 1) as u16
            });
            match spans.last_mut() {
                Some(last) if last.s == sid && !is_wide(&grapheme) && last_is_narrow(last) => {
                    last.t.push_str(&grapheme)
                }
                _ => spans.push(Span { x: col, t: grapheme, s: sid }),
            }
        }
        // Drop trailing unstyled whitespace to keep frames small.
        if let Some(last) = spans.last_mut()
            && last.s == 0
        {
            let trimmed = last.t.trim_end().len();
            last.t.truncate(trimmed);
            if last.t.is_empty() {
                spans.pop();
            }
        }
        lines.push(spans);
    }
    Screen { cols, rows, styles, lines, cursor, scrolled: offset > 0 }
}

fn is_wide(g: &str) -> bool {
    unicode_width::UnicodeWidthStr::width(g) != 1
}

/// Spans are kept narrow-only so that column positions stay exact even if
/// the viewer's width tables disagree with the emulator's.
fn last_is_narrow(span: &Span) -> bool {
    span.t.chars().last().map(|c| !is_wide(&c.to_string())).unwrap_or(true)
}

/// The visible screen as plain text, one line per row.
fn viewport_text(emu: &Emu) -> String {
    let mut out = String::new();
    for line in viewport_lines(emu) {
        let text: String = line_cells(emu, line).map(|(_, g, _)| g).collect();
        out.push_str(text.trim_end());
        out.push('\n');
    }
    finish_text(out)
}

/// The screen (and optionally the scrollback) with soft-wrapped lines
/// joined, as plain text or with SGR escapes.
fn format_screen(emu: &Emu, scrollback: bool, ansi: bool) -> String {
    let lines: Vec<Line> = if scrollback {
        let grid = emu.term.grid();
        (grid.topmost_line().0..=grid.bottommost_line().0).map(Line).collect()
    } else {
        viewport_lines(emu).collect()
    };
    let mut out = String::new();
    let mut logical: Vec<(String, CellStyle)> = Vec::new();
    for line in lines {
        let mut wrapped = false;
        for (_, grapheme, cell) in line_cells(emu, line) {
            wrapped = cell.flags.contains(Flags::WRAPLINE);
            logical.push((grapheme, if ansi { style_of(cell) } else { CellStyle::default() }));
        }
        if wrapped {
            continue;
        }
        while logical.last().is_some_and(|(g, s)| g == " " && *s == CellStyle::default()) {
            logical.pop();
        }
        let mut current = CellStyle::default();
        for (grapheme, style) in logical.drain(..) {
            if style != current {
                out.push_str(&sgr(&style));
                current = style;
            }
            out.push_str(&grapheme);
        }
        if current != CellStyle::default() {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }
    finish_text(out)
}

/// Drop trailing blank lines, leaving a single final newline.
fn finish_text(mut out: String) -> String {
    let trimmed = out.trim_end_matches('\n').len();
    out.truncate(trimmed);
    out.push('\n');
    out
}

/// The escape sequence that switches to `style` from any other style.
fn sgr(style: &CellStyle) -> String {
    let mut s = String::from("\x1b[0");
    const ATTRS: [(u8, u8); 7] = [
        (protocol::ATTR_BOLD, 1),
        (protocol::ATTR_FAINT, 2),
        (protocol::ATTR_ITALIC, 3),
        (protocol::ATTR_UNDERLINE, 4),
        (protocol::ATTR_INVERSE, 7),
        (protocol::ATTR_INVISIBLE, 8),
        (protocol::ATTR_STRIKE, 9),
    ];
    for (bit, code) in ATTRS {
        if style.a & bit != 0 {
            let _ = write!(s, ";{code}");
        }
    }
    for (color, base, extended) in [(style.fg, 30, 38), (style.bg, 40, 48)] {
        match color {
            Some(Color::Indexed(i @ 0..8)) => _ = write!(s, ";{}", base + i),
            Some(Color::Indexed(i @ 8..16)) => _ = write!(s, ";{}", base + 60 + i - 8),
            Some(Color::Indexed(i)) => _ = write!(s, ";{extended};5;{i}"),
            Some(Color::Rgb([r, g, b])) => _ = write!(s, ";{extended};2;{r};{g};{b}"),
            None => {}
        }
    }
    s.push('m');
    s
}

/// Whether Claude's footer lists running background work, e.g.
/// "⏵⏵ bypass permissions on · 1 shell · esc to interrupt".
pub fn detect_background(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].iter().any(|line| {
        line.split('·').any(|part| {
            let mut words = part.split_whitespace();
            let count = words.next().is_some_and(|w| w.parse::<u32>().is_ok_and(|n| n > 0));
            let what = words.next().unwrap_or("");
            count
                && (what.starts_with("shell")
                    || what.starts_with("background")
                    || what.starts_with("agent")
                    || what.starts_with("task")
                    || what.starts_with("monitor"))
        })
    })
}

pub const TRUST_PROMPT: &str = "Folder trust prompt";

/// Recognise Claude Code dialogs that block until a human answers.
pub fn detect_prompt(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let tail = lines[lines.len().saturating_sub(25)..].join("\n");
    let has_choice = tail.contains("1. Yes") || tail.contains("Yes, I");
    let has_hint = tail.contains("Esc to") || tail.contains("Enter to confirm");
    if !(has_choice && has_hint) {
        return None;
    }
    if tail.contains("trust") {
        Some(TRUST_PROMPT.into())
    } else if tail.contains("Bypass Permissions") {
        Some("Bypass permissions confirmation".into())
    } else {
        tail.lines()
            .map(str::trim)
            .find(|l| l.starts_with("Do you want") || l.ends_with('?'))
            .map(|l| l.trim_matches(|c: char| c == '│' || c.is_whitespace()).to_string())
            .or_else(|| Some("Waiting for a choice".into()))
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn test_emu() -> (Emu, Arc<Mutex<Vec<u8>>>) {
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer: PtyWriter = Arc::new(Mutex::new(Box::new(Sink(buf.clone()))));
        (Emu::new(40, 6, 100, writer), buf)
    }

    fn key(code: KeyCode, ctrl: bool, alt: bool, shift: bool) -> KeyInput {
        KeyInput { code, ctrl, alt, shift, sup: false }
    }

    #[test]
    fn encodes_keys() {
        let legacy = TermMode::default();
        assert_eq!(encode_key(legacy, &key(KeyCode::Char('a'), false, false, false)), b"a");
        assert_eq!(encode_key(legacy, &key(KeyCode::Char('A'), false, false, true)), b"A");
        assert_eq!(encode_key(legacy, &key(KeyCode::Char('c'), true, false, false)), b"\x03");
        assert_eq!(encode_key(legacy, &key(KeyCode::Char('b'), false, true, false)), b"\x1bb");
        assert_eq!(encode_key(legacy, &key(KeyCode::Enter, false, false, false)), b"\r");
        assert_eq!(encode_key(legacy, &key(KeyCode::Up, false, false, false)), b"\x1b[A");
        assert_eq!(encode_key(legacy, &key(KeyCode::Char('é'), false, false, false)), "é".as_bytes());
        assert_eq!(encode_key(legacy, &key(KeyCode::Backspace, false, false, false)), b"\x7f");
        assert_eq!(encode_key(legacy, &key(KeyCode::BackTab, false, false, false)), b"\x1b[Z");
        assert_eq!(encode_key(legacy, &key(KeyCode::Left, true, false, false)), b"\x1b[1;5D");
        assert_eq!(encode_key(legacy, &key(KeyCode::F(5), false, false, false)), b"\x1b[15~");
        let app = legacy | TermMode::APP_CURSOR;
        assert_eq!(encode_key(app, &key(KeyCode::Up, false, false, false)), b"\x1bOA");
    }

    #[test]
    fn encodes_kitty_keys() {
        let (mut emu, _) = test_emu();
        emu.vt_write(b"\x1b[>1u");
        let mode = *emu.term.mode();
        assert!(mode.contains(TermMode::DISAMBIGUATE_ESC_CODES));
        assert_eq!(encode_key(mode, &key(KeyCode::Enter, false, false, true)), b"\x1b[13;2u");
        assert_eq!(encode_key(mode, &key(KeyCode::Enter, false, false, false)), b"\r");
        assert_eq!(encode_key(mode, &key(KeyCode::Escape, false, false, false)), b"\x1b[27u");
        assert_eq!(encode_key(mode, &key(KeyCode::Char('c'), true, false, false)), b"\x1b[99;5u");
        assert_eq!(encode_key(mode, &key(KeyCode::Char('A'), false, false, true)), b"A");
    }

    #[test]
    fn snapshots_styles() {
        let (mut emu, _) = test_emu();
        emu.vt_write(b"plain \x1b[1;31mred\x1b[0m\r\n\xe4\xbd\xa0\xe5\xa5\xbd!");
        let s = snapshot(&emu);
        assert_eq!(s.cols, 40);
        assert_eq!(s.lines[0][0].t, "plain ");
        assert_eq!(s.lines[0][1].t, "red");
        assert_eq!(s.lines[0][1].x, 6);
        let red = s.styles[s.lines[0][1].s as usize];
        assert_eq!(red.fg, Some(Color::Indexed(1)));
        assert!(red.a & protocol::ATTR_BOLD != 0);
        let texts: Vec<_> = s.lines[1].iter().map(|sp| (sp.x, sp.t.as_str())).collect();
        assert_eq!(texts, vec![(0, "你"), (2, "好"), (4, "!")]);
        assert_eq!(s.cursor, Some((5, 1)));
        assert_eq!(viewport_text(&emu), "plain red\n你好!\n");
    }

    #[test]
    fn keeps_erased_background() {
        let (mut emu, _) = test_emu();
        emu.vt_write(b"\x1b[44m\x1b[K\x1b[0m");
        let s = snapshot(&emu);
        let bg = s.styles[s.lines[0][0].s as usize].bg;
        assert_eq!(bg, Some(Color::Indexed(4)));
    }

    #[test]
    fn answers_queries() {
        let (mut emu, out) = test_emu();
        emu.vt_write(b"\x1b[6n");
        assert_eq!(out.lock().unwrap().as_slice(), b"\x1b[1;1R");
        out.lock().unwrap().clear();
        emu.vt_write(b"\x1b]11;?\x07");
        assert_eq!(out.lock().unwrap().as_slice(), b"\x1b]11;rgb:2828/2c2c/3434\x07");
    }

    #[test]
    fn reports_title() {
        let (mut emu, _) = test_emu();
        emu.vt_write(b"\x1b]0;\xe2\x9c\xb3 Fix bug\x07");
        assert_eq!(emu.title.take().map(|t| clean_title(&t)).as_deref(), Some("Fix bug"));
    }

    #[test]
    fn scrolls_and_formats_history() {
        let (mut emu, _) = test_emu();
        for i in 0..10 {
            emu.vt_write(format!("line {i}\r\n").as_bytes());
        }
        emu.vt_write(&[b'x'; 45]);
        assert!(viewport_text(&emu).starts_with("line 6\n"));
        handle(&mut emu, Msg::Scroll { delta: -3, col: 0, row: 0 });
        assert!(snapshot(&emu).scrolled);
        assert!(viewport_text(&emu).starts_with("line 3\n"));
        handle(&mut emu, Msg::ScrollReset);
        let all = format_screen(&emu, true, false);
        assert!(all.starts_with("line 0\n"));
        assert!(all.ends_with(&format!("line 9\n{}\n", "x".repeat(45))));
        emu.vt_write(b"\r\n\x1b[32mok\x1b[0m");
        assert!(format_screen(&emu, false, true).ends_with("\x1b[0;32mok\x1b[0m\n"));
    }

    #[test]
    fn holds_synchronized_updates() {
        let (mut emu, _) = test_emu();
        emu.vt_write(b"\x1b[?2026hhidden");
        assert_eq!(viewport_text(&emu), "\n");
        assert!(emu.sync_deadline().is_some());
        emu.vt_write(b"\x1b[?2026l");
        assert_eq!(viewport_text(&emu), "hidden\n");
    }

    #[test]
    fn encodes_paste_and_wheel() {
        assert_eq!(encode_paste("a\nb", false), b"a\rb");
        assert_eq!(encode_paste("a\x1b[201~b", true), b"\x1b[200~a[201~b\x1b[201~");
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(encode_wheel(sgr, -2, 3, 4), b"\x1b[<64;4;5M\x1b[<64;4;5M");
    }

    #[test]
    fn detects_background_footer() {
        let busy = "⏺ started\n\n────\n❯\n────\n  ⏵⏵ bypass permissions on · 1 shell · esc to interrupt · ← for agents";
        assert!(detect_background(busy));
        assert!(detect_background("❯\n  ? for shortcuts · 2 shells"));
        let quiet = "⏺ ran 1 shell command\n❯\n  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents";
        assert!(!detect_background(quiet));
    }

    #[test]
    fn detects_trust_prompt() {
        let screen = "Do you trust the files in this folder?\n❯ 1. Yes, proceed\n  2. No, exit\nEnter to confirm · Esc to exit";
        assert_eq!(detect_prompt(screen).as_deref(), Some("Folder trust prompt"));
        assert_eq!(detect_prompt("just some output"), None);
        let perm = "Bash command\n  rm -rf build\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel";
        assert_eq!(detect_prompt(perm).as_deref(), Some("Do you want to proceed?"));
    }
}
