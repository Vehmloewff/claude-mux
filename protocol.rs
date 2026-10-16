//! Wire protocol between `claude-mux` clients and the daemon.
//!
//! Every message is a single line of JSON. Most connections are a single
//! request followed by a single response; `Subscribe` turns the connection
//! into a bidirectional stream used by `claude-mux manage`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Claude is booting and has not reported anything yet.
    Starting,
    /// Claude is processing a prompt.
    Working,
    /// Claude is blocked on the user (permission prompt, trust dialog, ...).
    NeedsInput,
    /// The turn ended but background tasks (shells, agents) are still
    /// running; claude resumes on its own when they finish.
    Background,
    /// Claude finished its turn and is waiting for the next message.
    Idle,
    /// The claude process exited.
    Exited,
    /// The session was running when the daemon stopped; it can be resumed.
    Stopped,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Working => "working",
            Status::NeedsInput => "needs-input",
            Status::Background => "background",
            Status::Idle => "idle",
            Status::Exited => "exited",
            Status::Stopped => "stopped",
        }
    }

    /// Whether the process is alive in the daemon.
    pub fn is_live(self) -> bool {
        !matches!(self, Status::Exited | Status::Stopped)
    }

    /// Whether `claude-mux wait` should stop waiting on this status.
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            Status::Idle | Status::NeedsInput | Status::Exited | Status::Stopped
        )
    }

    /// Settled, or merely waiting on background work (`wait --turn`).
    pub fn is_turn_over(self) -> bool {
        self.is_settled() || self == Status::Background
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Claude Code's id for the conversation (`--session-id`/`--resume`).
    /// A `/clear` starts a new conversation, and with it a new session.
    pub id: String,
    /// What the session is for, as given by the user.
    pub description: Option<String>,
    pub cwd: String,
    /// The first prompt the session was created with.
    pub prompt: String,
    pub status: Status,
    /// Extra detail on the status, e.g. the permission prompt text.
    pub status_detail: Option<String>,
    /// Terminal title set by claude (usually a short task summary).
    pub title: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub exit_code: Option<i32>,
    /// Messages waiting to be typed into claude once it is idle.
    pub queued: usize,
    /// Number of completed turns.
    pub turns: u64,
    pub last_message: Option<String>,
    pub claude_args: Vec<String>,
    #[serde(default)]
    pub auto_trust: bool,
    /// Background tasks still running when the last turn ended.
    #[serde(default)]
    pub background: Vec<BackgroundTask>,
    /// Idle with its claude process stopped to save memory; the daemon
    /// resumes it when it is sent a message or keys.
    #[serde(default)]
    pub asleep: bool,
    /// The session that took over this one's claude process when it started
    /// a new conversation (`/clear`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continued_as: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    /// "shell", "agent", ...
    pub kind: String,
    pub description: String,
}

impl SessionInfo {
    /// The description, else the first line of the prompt, else claude's
    /// terminal title, else the short id.
    pub fn display_name(&self) -> &str {
        self.description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .or_else(|| self.prompt.lines().map(str::trim).find(|l| !l.is_empty()))
            .or(self.title.as_deref())
            .unwrap_or(self.short_id())
    }

    /// The first characters of the id: enough to tell sessions apart, and
    /// accepted anywhere an id is.
    pub fn short_id(&self) -> &str {
        short_id(&self.id)
    }

    /// The status to show people: `cleared` for a session whose process
    /// went on to a new conversation.
    pub fn status_label(&self) -> &'static str {
        match self.effective_status() {
            Status::Stopped if self.continued_as.is_some() => "cleared",
            status => status.label(),
        }
    }

    /// The status `wait` reports: queued messages count as pending work.
    pub fn effective_status(&self) -> Status {
        if self.queued > 0 && matches!(self.status, Status::Idle | Status::Background) {
            Status::Working
        } else {
            self.status
        }
    }
}

/// The first characters of a session id, as `list` shows it.
pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateRequest {
    pub description: Option<String>,
    pub cwd: String,
    pub prompt: String,
    pub claude_args: Vec<String>,
    /// Automatically accept the folder trust dialog.
    #[serde(default)]
    pub trust: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Create(CreateRequest),
    Send {
        id: String,
        message: String,
        /// Type the message immediately instead of waiting for claude to be idle.
        now: bool,
    },
    Wait {
        ids: Vec<String>,
        /// Return as soon as any (instead of all) of the sessions settle.
        any: bool,
        timeout_ms: Option<u64>,
        /// Also return when only background tasks are left running.
        #[serde(default)]
        turn: bool,
    },
    List,
    Get {
        id: String,
    },
    Peek {
        id: String,
        /// Include ANSI styling.
        ansi: bool,
    },
    Kill {
        id: String,
    },
    Remove {
        id: String,
    },
    Resume {
        id: String,
    },
    Describe {
        id: String,
        description: Option<String>,
    },
    /// Press keys in a session (e.g. to answer a dialog).
    Keys {
        id: String,
        keys: Vec<KeyInput>,
    },
    /// A Claude Code hook fired in the claude process `process`.
    Hook {
        process: String,
        event: String,
        payload: serde_json::Value,
    },
    Shutdown,
    /// Turn this connection into an event stream (see `StreamEvent`).
    Subscribe,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant, reason = "short-lived messages")]
pub enum Response {
    Ok,
    Error { message: String },
    /// Answer to `Ping`: the daemon's claude-mux version.
    Pong { version: String },
    Session { session: SessionInfo },
    Sessions { sessions: Vec<SessionInfo> },
    Text { text: String },
    Wait { sessions: Vec<SessionInfo>, timed_out: bool },
}

/// Messages a subscribed client (`manage`) sends to the daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    /// Watch a session's screen at the given size (None stops watching).
    Watch { id: Option<String>, cols: u16, rows: u16 },
    Key { id: String, key: KeyInput },
    Paste { id: String, text: String },
    /// Scroll the viewport (negative = up), or forward wheel to the app.
    Scroll { id: String, delta: i32, col: u16, row: u16 },
    ScrollReset { id: String },
}

/// Messages the daemon pushes to a subscribed client.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Sessions { sessions: Vec<SessionInfo> },
    Screen { id: String, screen: Screen },
}

/// A terminal-independent key press, encoded to bytes by the terminal in the
/// daemon (so the session's keyboard modes, e.g. kitty protocol, apply).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyInput {
    pub code: KeyCode,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub sup: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", content = "v", rename_all = "snake_case")]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

/// A rendered snapshot of a session's viewport.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    /// Distinct cell styles referenced by `Span::s`.
    pub styles: Vec<CellStyle>,
    pub lines: Vec<Vec<Span>>,
    pub cursor: Option<(u16, u16)>,
    /// Lines scrolled up from the bottom (0 = following output).
    pub scrolled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Span {
    /// Starting column.
    pub x: u16,
    pub t: String,
    /// Index into `Screen::styles`.
    pub s: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellStyle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<Color>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<Color>,
    /// Bitset of `ATTR_*`.
    #[serde(default)]
    pub a: u8,
}

pub const ATTR_BOLD: u8 = 1;
pub const ATTR_ITALIC: u8 = 2;
pub const ATTR_UNDERLINE: u8 = 4;
pub const ATTR_INVERSE: u8 = 8;
pub const ATTR_FAINT: u8 = 16;
pub const ATTR_STRIKE: u8 = 32;
pub const ATTR_INVISIBLE: u8 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Color {
    /// Palette index, rendered with the viewer's own palette.
    Indexed(u8),
    Rgb([u8; 3]),
}

impl KeyInput {
    pub fn plain(code: KeyCode) -> Self {
        KeyInput { code, shift: false, ctrl: false, alt: false, sup: false }
    }

    /// Parse names like `enter`, `esc`, `down`, `ctrl-c`, `alt-b`, `y`.
    pub fn parse(spec: &str) -> Option<Self> {
        let mut key = KeyInput::plain(KeyCode::Enter);
        let mut rest = spec;
        loop {
            let lower = rest.to_ascii_lowercase();
            let Some((prefix, tail)) = ["ctrl-", "c-", "alt-", "m-", "shift-", "s-", "super-", "cmd-"]
                .iter()
                .find(|p| lower.starts_with(**p) && rest.len() > p.len())
                .map(|p| (*p, &rest[p.len()..]))
            else {
                break;
            };
            match prefix {
                "ctrl-" | "c-" => key.ctrl = true,
                "alt-" | "m-" => key.alt = true,
                "shift-" | "s-" => key.shift = true,
                _ => key.sup = true,
            }
            rest = tail;
        }
        let mut chars = rest.chars();
        key.code = match rest.to_ascii_lowercase().as_str() {
            "enter" | "return" | "cr" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "space" => KeyCode::Char(' '),
            "bs" | "backspace" => KeyCode::Backspace,
            "esc" | "escape" => KeyCode::Escape,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pgup" | "pageup" => KeyCode::PageUp,
            "pgdn" | "pagedown" => KeyCode::PageDown,
            "ins" | "insert" => KeyCode::Insert,
            "del" | "delete" => KeyCode::Delete,
            f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
                KeyCode::F(f[1..].parse().ok()?)
            }
            _ => match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                _ => return None,
            },
        };
        Some(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keys() {
        assert_eq!(KeyInput::parse("enter"), Some(KeyInput::plain(KeyCode::Enter)));
        assert_eq!(KeyInput::parse("y"), Some(KeyInput::plain(KeyCode::Char('y'))));
        let c = KeyInput::parse("ctrl-c").unwrap();
        assert!(c.ctrl && c.code == KeyCode::Char('c'));
        let sa = KeyInput::parse("shift-alt-tab").unwrap();
        assert!(sa.shift && sa.alt && sa.code == KeyCode::Tab);
        assert_eq!(KeyInput::parse("f5").unwrap().code, KeyCode::F(5));
        assert_eq!(KeyInput::parse("-").unwrap().code, KeyCode::Char('-'));
        assert_eq!(KeyInput::parse("bogus"), None);
    }
}
