//! A libghostty-vt terminal emulator for one session.
//!
//! libghostty handles are `!Send`, so each session's terminal lives on its
//! own thread and is driven through a channel (`TermHandle`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::key::{self, Key, Mods};
use libghostty_vt::mouse;
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::screen::{CellContentTag, CellWide};
use libghostty_vt::style::{StyleColor, Underline};
use libghostty_vt::terminal::{Mode, ScrollViewport};
use libghostty_vt::{RenderState, Terminal};

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
            .name("claudex-term".into())
            .spawn(move || {
                if let Err(e) = run(cols, rows, writer, rx, on_event) {
                    eprintln!("terminal thread failed: {e}");
                }
            })
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

struct Emu {
    term: Terminal<'static, 'static>,
    render: RenderState<'static>,
    rows_it: RowIterator<'static>,
    cells_it: CellIterator<'static>,
    keys: key::Encoder<'static>,
    key_event: key::Event<'static>,
    mouse: mouse::Encoder<'static>,
    mouse_event: mouse::Event<'static>,
    writer: PtyWriter,
}

fn run(
    cols: u16,
    rows: u16,
    writer: PtyWriter,
    rx: Receiver<Msg>,
    on_event: impl Fn(TermEvent),
) -> anyhow::Result<()> {
    let mut term = Terminal::new(cols, rows)?;
    term.set_scrollback_max_lines(Some(10_000))?;
    let title_changed = Rc::new(RefCell::new(false));
    {
        let writer = writer.clone();
        let title_changed = title_changed.clone();
        term.on_pty_write(move |_t, data| {
            // Replies to queries (device attributes, cursor position, ...).
            if let Ok(mut w) = writer.lock() {
                let _ = w.write_all(data);
                let _ = w.flush();
            }
        })?
        .on_title_changed(move |_t| {
            *title_changed.borrow_mut() = true;
        })?
        .on_size(move |t| {
            Some(libghostty_vt::terminal::SizeReportSize {
                rows: t.rows().unwrap_or(rows),
                columns: t.cols().unwrap_or(cols),
                cell_width: 8,
                cell_height: 16,
            })
        })?;
    }

    let mut emu = Emu {
        term,
        render: RenderState::new()?,
        rows_it: RowIterator::new()?,
        cells_it: CellIterator::new()?,
        keys: key::Encoder::new()?,
        key_event: key::Event::new()?,
        mouse: mouse::Encoder::new()?,
        mouse_event: mouse::Event::new()?,
        writer,
    };

    let mut last_scan = Instant::now() - Duration::from_secs(1);
    let mut scan_pending = false;
    let mut prompt: Option<String> = None;
    let mut background = false;

    loop {
        let timeout = if scan_pending { Duration::from_millis(150) } else { Duration::from_secs(3600) };
        match rx.recv_timeout(timeout) {
            Ok(Msg::Output(data)) => {
                emu.term.vt_write(&data);
                // Drain any further queued output before notifying.
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        Msg::Output(more) => emu.term.vt_write(&more),
                        other => {
                            handle(&mut emu, other);
                        }
                    }
                }
                if std::mem::take(&mut *title_changed.borrow_mut())
                    && let Ok(title) = emu.term.title() {
                        on_event(TermEvent::Title(clean_title(title)));
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
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        // Look for blocking dialogs, throttled since it walks the screen.
        if scan_pending && last_scan.elapsed() >= Duration::from_millis(150) {
            scan_pending = false;
            last_scan = Instant::now();
            let text = viewport_text(&mut emu);
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
            emu.term.vt_write(&data);
            true
        }
        Msg::Resize(cols, rows) => {
            let cols = cols.max(10);
            let rows = rows.max(3);
            if emu.term.cols().ok() != Some(cols) || emu.term.rows().ok() != Some(rows) {
                let _ = emu.term.resize(cols, rows, 8, 16);
            }
            true
        }
        Msg::Key(input) => {
            let bytes = encode_key(emu, &input);
            emu.term.scroll_viewport(ScrollViewport::Bottom);
            write_pty(emu, &bytes);
            true
        }
        Msg::Paste(text) => {
            let bracketed = emu.term.mode(Mode::BRACKETED_PASTE).unwrap_or(false);
            let mut data = text.into_bytes();
            let mut buf = vec![0u8; data.len() * 2 + 16];
            let bytes = match libghostty_vt::paste::encode(&mut data, bracketed, &mut buf) {
                Ok(n) => buf[..n].to_vec(),
                Err(_) => data,
            };
            emu.term.scroll_viewport(ScrollViewport::Bottom);
            write_pty(emu, &bytes);
            true
        }
        Msg::Scroll { delta, col, row } => {
            if emu.term.is_mouse_tracking().unwrap_or(false) {
                let bytes = encode_wheel(emu, delta, col, row);
                write_pty(emu, &bytes);
            } else {
                emu.term.scroll_viewport(ScrollViewport::Delta(delta as isize));
            }
            true
        }
        Msg::ScrollReset => {
            emu.term.scroll_viewport(ScrollViewport::Bottom);
            true
        }
        Msg::Snapshot(reply) => {
            let _ = reply.send(snapshot(emu).unwrap_or_default());
            false
        }
        Msg::Text { scrollback, ansi, reply } => {
            let text = if scrollback || ansi {
                format_screen(emu, if ansi { Format::Vt } else { Format::Plain })
            } else {
                viewport_text(emu)
            };
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

fn encode_key(emu: &mut Emu, input: &KeyInput) -> Vec<u8> {
    let mut mods = Mods::empty();
    if input.shift {
        mods |= Mods::SHIFT;
    }
    if input.ctrl {
        mods |= Mods::CTRL;
    }
    if input.alt {
        mods |= Mods::ALT;
    }
    if input.sup {
        mods |= Mods::SUPER;
    }
    let (key, text, unshifted) = match &input.code {
        KeyCode::Char(c) => {
            let (key, unshifted) = physical_key(*c);
            (key, Some(c.to_string()), unshifted)
        }
        KeyCode::Enter => (Key::Enter, None, None),
        KeyCode::Tab => (Key::Tab, None, None),
        KeyCode::BackTab => {
            mods |= Mods::SHIFT;
            (Key::Tab, None, None)
        }
        KeyCode::Backspace => (Key::Backspace, None, None),
        KeyCode::Escape => (Key::Escape, None, None),
        KeyCode::Up => (Key::ArrowUp, None, None),
        KeyCode::Down => (Key::ArrowDown, None, None),
        KeyCode::Left => (Key::ArrowLeft, None, None),
        KeyCode::Right => (Key::ArrowRight, None, None),
        KeyCode::Home => (Key::Home, None, None),
        KeyCode::End => (Key::End, None, None),
        KeyCode::PageUp => (Key::PageUp, None, None),
        KeyCode::PageDown => (Key::PageDown, None, None),
        KeyCode::Insert => (Key::Insert, None, None),
        KeyCode::Delete => (Key::Delete, None, None),
        KeyCode::F(n) => (function_key(*n), None, None),
    };

    emu.keys.set_options_from_terminal(&emu.term);
    emu.keys.set_macos_option_as_alt(key::OptionAsAlt::True);
    let ev = &mut emu.key_event;
    ev.set_action(key::Action::Press)
        .set_key(key)
        .set_mods(mods)
        .set_consumed_mods(if text.is_some() && input.shift { Mods::SHIFT } else { Mods::empty() })
        .set_composing(false)
        .set_utf8(if input.ctrl { None } else { text.clone() });
    if let Some(u) = unshifted {
        ev.set_unshifted_codepoint(u);
    }
    let mut out = Vec::with_capacity(16);
    if emu.keys.encode_to_vec(&emu.key_event, &mut out).is_err() || out.is_empty() {
        // Keys libghostty has no mapping for: send the text as typed.
        if let Some(t) = text {
            out = t.into_bytes();
            if input.alt {
                out.insert(0, 0x1b);
            }
        }
    }
    out
}

fn function_key(n: u8) -> Key {
    const KEYS: [Key; 12] = [
        Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6,
        Key::F7, Key::F8, Key::F9, Key::F10, Key::F11, Key::F12,
    ];
    KEYS.get(n.saturating_sub(1) as usize).copied().unwrap_or(Key::Unidentified)
}

/// Map a character to the US-layout physical key that produces it.
fn physical_key(c: char) -> (Key, Option<char>) {
    const LETTERS: [Key; 26] = [
        Key::A, Key::B, Key::C, Key::D, Key::E, Key::F, Key::G, Key::H, Key::I,
        Key::J, Key::K, Key::L, Key::M, Key::N, Key::O, Key::P, Key::Q, Key::R,
        Key::S, Key::T, Key::U, Key::V, Key::W, Key::X, Key::Y, Key::Z,
    ];
    const DIGITS: [Key; 10] = [
        Key::Digit0, Key::Digit1, Key::Digit2, Key::Digit3, Key::Digit4,
        Key::Digit5, Key::Digit6, Key::Digit7, Key::Digit8, Key::Digit9,
    ];
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        return (LETTERS[(lower as u8 - b'a') as usize], Some(lower));
    }
    if c.is_ascii_digit() {
        return (DIGITS[(c as u8 - b'0') as usize], Some(c));
    }
    let (key, base) = match c {
        ' ' => (Key::Space, ' '),
        '`' | '~' => (Key::Backquote, '`'),
        '\\' | '|' => (Key::Backslash, '\\'),
        '[' | '{' => (Key::BracketLeft, '['),
        ']' | '}' => (Key::BracketRight, ']'),
        ',' | '<' => (Key::Comma, ','),
        '=' | '+' => (Key::Equal, '='),
        '-' | '_' => (Key::Minus, '-'),
        '.' | '>' => (Key::Period, '.'),
        '\'' | '"' => (Key::Quote, '\''),
        ';' | ':' => (Key::Semicolon, ';'),
        '/' | '?' => (Key::Slash, '/'),
        '!' => (Key::Digit1, '1'),
        '@' => (Key::Digit2, '2'),
        '#' => (Key::Digit3, '3'),
        '$' => (Key::Digit4, '4'),
        '%' => (Key::Digit5, '5'),
        '^' => (Key::Digit6, '6'),
        '&' => (Key::Digit7, '7'),
        '*' => (Key::Digit8, '8'),
        '(' => (Key::Digit9, '9'),
        ')' => (Key::Digit0, '0'),
        _ => return (Key::Unidentified, None),
    };
    (key, Some(base))
}

fn encode_wheel(emu: &mut Emu, delta: i32, col: u16, row: u16) -> Vec<u8> {
    let cols = emu.term.cols().unwrap_or(80) as u32;
    let rows = emu.term.rows().unwrap_or(24) as u32;
    emu.mouse.set_options_from_terminal(&emu.term).set_size(mouse::EncoderSize {
        screen_width: cols,
        screen_height: rows,
        cell_width: 1,
        cell_height: 1,
        padding_top: 0,
        padding_bottom: 0,
        padding_right: 0,
        padding_left: 0,
    });
    let button = if delta < 0 { mouse::Button::Four } else { mouse::Button::Five };
    emu.mouse_event
        .set_action(mouse::Action::Press)
        .set_button(Some(button))
        .set_mods(Mods::empty())
        .set_position(libghostty_vt::ffi::MousePosition {
            x: col as f32 + 0.5,
            y: row as f32 + 0.5,
        });
    let mut out = Vec::new();
    for _ in 0..delta.unsigned_abs().min(10) {
        let _ = emu.mouse.encode_to_vec(&emu.mouse_event, &mut out);
    }
    out
}

fn color_of(c: StyleColor) -> Option<Color> {
    match c {
        StyleColor::None => None,
        StyleColor::Palette(i) => Some(Color::Indexed(i.0)),
        StyleColor::Rgb(rgb) => Some(Color::Rgb([rgb.r, rgb.g, rgb.b])),
    }
}

fn snapshot(emu: &mut Emu) -> anyhow::Result<Screen> {
    let scrollbar = emu.term.scrollbar().ok();
    let snap = emu.render.update(&emu.term)?;
    let cols = snap.cols()?;
    let rows = snap.rows()?;
    let cursor = if snap.cursor_visible()? {
        snap.cursor_viewport()?.map(|c| (c.x, c.y))
    } else {
        None
    };

    let mut styles: Vec<CellStyle> = vec![CellStyle::default()];
    let mut style_ids: HashMap<CellStyle, u16> = HashMap::from([(CellStyle::default(), 0)]);
    let mut lines = Vec::with_capacity(rows as usize);
    let mut grapheme = String::new();

    let mut row_iter = emu.rows_it.update(&snap)?;
    while let Some(row) = row_iter.next() {
        let mut spans: Vec<Span> = Vec::new();
        let mut cell_iter = emu.cells_it.update(row)?;
        let mut x: u16 = 0;
        while let Some(cell) = cell_iter.next() {
            let col = x;
            x += 1;
            let raw = cell.raw_cell()?;
            if matches!(raw.wide()?, CellWide::SpacerTail | CellWide::SpacerHead) {
                continue;
            }
            let style = if cell.has_styling()? { cell.style()? } else { Default::default() };
            let mut a = 0u8;
            if style.bold {
                a |= protocol::ATTR_BOLD;
            }
            if style.italic {
                a |= protocol::ATTR_ITALIC;
            }
            if style.underline != Underline::None {
                a |= protocol::ATTR_UNDERLINE;
            }
            if style.inverse {
                a |= protocol::ATTR_INVERSE;
            }
            if style.faint {
                a |= protocol::ATTR_FAINT;
            }
            if style.strikethrough {
                a |= protocol::ATTR_STRIKE;
            }
            if style.invisible {
                a |= protocol::ATTR_INVISIBLE;
            }
            // Background-only cells (e.g. erased with a colored bg) carry
            // their color in the cell content rather than a style.
            let bg = match raw.content_tag()? {
                CellContentTag::BgColorPalette => Some(Color::Indexed(raw.bg_color_palette()?.0)),
                CellContentTag::BgColorRgb => {
                    let rgb = raw.bg_color_rgb()?;
                    Some(Color::Rgb([rgb.r, rgb.g, rgb.b]))
                }
                _ => color_of(style.bg_color),
            };
            let cs = CellStyle { fg: color_of(style.fg_color), bg, a };
            let sid = *style_ids.entry(cs).or_insert_with(|| {
                styles.push(cs);
                (styles.len() - 1) as u16
            });

            grapheme.clear();
            if cell.graphemes_len()? > 0 {
                cell.graphemes_utf8(&mut grapheme)?;
            }
            if grapheme.is_empty() {
                grapheme.push(' ');
            }

            match spans.last_mut() {
                Some(last) if last.s == sid && !is_wide(&grapheme) && last_is_narrow(last) => {
                    last.t.push_str(&grapheme)
                }
                _ => spans.push(Span { x: col, t: grapheme.clone(), s: sid }),
            }
        }
        // Drop trailing unstyled whitespace to keep frames small.
        if let Some(last) = spans.last_mut()
            && last.s == 0 {
                let trimmed = last.t.trim_end().len();
                last.t.truncate(trimmed);
                if last.t.is_empty() {
                    spans.pop();
                }
            }
        lines.push(spans);
    }
    let scrolled = scrollbar.map(|s| s.offset + s.len < s.total).unwrap_or(false);
    Ok(Screen { cols, rows, styles, lines, cursor, scrolled })
}

fn is_wide(g: &str) -> bool {
    unicode_width::UnicodeWidthStr::width(g) != 1
}

/// Spans are kept narrow-only so that column positions stay exact even if
/// the viewer's width tables disagree with libghostty's.
fn last_is_narrow(span: &Span) -> bool {
    span.t.chars().last().map(|c| !is_wide(&c.to_string())).unwrap_or(true)
}

fn viewport_text(emu: &mut Emu) -> String {
    let Ok(snap) = emu.render.update(&emu.term) else { return String::new() };
    let mut out = String::new();
    let mut grapheme = String::new();
    let Ok(mut row_iter) = emu.rows_it.update(&snap) else { return out };
    while let Some(row) = row_iter.next() {
        let mut line = String::new();
        if let Ok(mut cells) = emu.cells_it.update(row) {
            while let Some(cell) = cells.next() {
                let wide = cell.raw_cell().and_then(|c| c.wide()).unwrap_or(CellWide::Narrow);
                if matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead) {
                    continue;
                }
                grapheme.clear();
                if cell.graphemes_len().unwrap_or(0) > 0 {
                    let _ = cell.graphemes_utf8(&mut grapheme);
                }
                line.push_str(if grapheme.is_empty() { " " } else { &grapheme });
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    let trimmed = out.trim_end_matches('\n').len();
    out.truncate(trimmed);
    out.push('\n');
    out
}

fn format_screen(emu: &mut Emu, format: Format) -> String {
    let opts = FormatterOptions::new().with_format(format).with_trim(true).with_unwrap(true);
    match Formatter::new(&emu.term, opts).and_then(|mut f| f.format_len().map(|n| (f, n))) {
        Ok((mut f, n)) => {
            let mut buf = vec![0u8; n];
            match f.format_buf(&mut buf) {
                Ok(n) => String::from_utf8_lossy(&buf[..n]).into_owned(),
                Err(_) => String::new(),
            }
        }
        Err(_) => String::new(),
    }
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
        let emu = Emu {
            term: {
                let mut term = Terminal::new(40, 6).unwrap();
                term.set_scrollback_max_lines(Some(100)).unwrap();
                term
            },
            render: RenderState::new().unwrap(),
            rows_it: RowIterator::new().unwrap(),
            cells_it: CellIterator::new().unwrap(),
            keys: key::Encoder::new().unwrap(),
            key_event: key::Event::new().unwrap(),
            mouse: mouse::Encoder::new().unwrap(),
            mouse_event: mouse::Event::new().unwrap(),
            writer,
        };
        (emu, buf)
    }

    fn key(code: KeyCode, ctrl: bool, alt: bool, shift: bool) -> KeyInput {
        KeyInput { code, ctrl, alt, shift, sup: false }
    }

    #[test]
    fn encodes_keys() {
        let (mut emu, _) = test_emu();
        assert_eq!(encode_key(&mut emu, &key(KeyCode::Char('a'), false, false, false)), b"a");
        assert_eq!(encode_key(&mut emu, &key(KeyCode::Char('A'), false, false, true)), b"A");
        assert_eq!(encode_key(&mut emu, &key(KeyCode::Char('c'), true, false, false)), b"\x03");
        assert_eq!(encode_key(&mut emu, &key(KeyCode::Char('b'), false, true, false)), b"\x1bb");
        assert_eq!(encode_key(&mut emu, &key(KeyCode::Enter, false, false, false)), b"\r");
        assert_eq!(encode_key(&mut emu, &key(KeyCode::Up, false, false, false)), b"\x1b[A");
        assert_eq!(encode_key(&mut emu, &key(KeyCode::Char('é'), false, false, false)), "é".as_bytes());
        assert_eq!(encode_key(&mut emu, &key(KeyCode::Backspace, false, false, false)), b"\x7f");
    }

    #[test]
    fn snapshots_styles() {
        let (mut emu, _) = test_emu();
        emu.term.vt_write(b"plain \x1b[1;31mred\x1b[0m\r\n\xe4\xbd\xa0\xe5\xa5\xbd!");
        let s = snapshot(&mut emu).unwrap();
        assert_eq!(s.cols, 40);
        assert_eq!(s.lines[0][0].t, "plain ");
        assert_eq!(s.lines[0][1].t, "red");
        assert_eq!(s.lines[0][1].x, 6);
        let red = s.styles[s.lines[0][1].s as usize];
        assert_eq!(red.fg, Some(Color::Indexed(1)));
        assert!(red.a & protocol::ATTR_BOLD != 0);
        let texts: Vec<_> = s.lines[1].iter().map(|sp| (sp.x, sp.t.as_str())).collect();
        assert_eq!(texts, vec![(0, "你"), (2, "好"), (4, "!")]);
        assert_eq!(viewport_text(&mut emu), "plain red\n你好!\n");
    }

    #[test]
    fn answers_queries() {
        let (mut emu, out) = test_emu();
        emu.term
            .on_pty_write({
                let w = emu.writer.clone();
                move |_t, d| {
                    w.lock().unwrap().write_all(d).unwrap();
                }
            })
            .unwrap();
        emu.term.vt_write(b"\x1b[6n");
        assert_eq!(out.lock().unwrap().as_slice(), b"\x1b[1;1R");
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
