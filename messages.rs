//! The conversation in a session, read from Claude Code's transcript: each
//! message sent to claude, how long it worked on it, and its final reply.
//! Tool calls, intermediate text and local slash commands are left out.

use serde::Serialize;
use serde_json::Value;

use crate::protocol::{SessionInfo, Status};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Message {
    /// What was sent to claude.
    pub request: String,
    /// Unix seconds.
    pub requested_at: u64,
    /// How long claude worked on it, so far if it isn't done.
    pub duration_ms: u64,
    /// Whether claude has finished with it.
    pub done: bool,
    /// Claude's final reply. `None` before claude has said anything.
    pub response: Option<String>,
    #[serde(skip)]
    started_ms: u64,
    /// A slash command, which only counts once claude answers it: `/model`
    /// and the like never reach claude.
    #[serde(skip)]
    command: bool,
    #[serde(skip)]
    answered: bool,
    /// The assistant message `response` came from. Each content block is
    /// its own transcript line, sharing the message's id.
    #[serde(skip)]
    reply_id: Option<String>,
}

/// Builds messages from a transcript, a line at a time, so a watcher can
/// feed it what was appended since it last looked.
#[derive(Default)]
pub struct Transcript {
    messages: Vec<Message>,
    /// Bytes after the last newline, waiting for the rest of their line.
    partial: Vec<u8>,
}

impl Transcript {
    pub fn parse(jsonl: &str) -> Self {
        let mut t = Self::default();
        t.feed(jsonl.as_bytes());
        t
    }

    /// Takes the next bytes of the file; a trailing incomplete line is held
    /// back until the rest of it arrives.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        let Some(end) = self.partial.iter().rposition(|b| *b == b'\n') else {
            return;
        };
        let rest = self.partial.split_off(end + 1);
        let complete = std::mem::replace(&mut self.partial, rest);
        for line in String::from_utf8_lossy(&complete).lines() {
            self.line(line);
        }
    }

    fn line(&mut self, line: &str) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if entry.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let at = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp)
            .unwrap_or(0);
        match entry.get("type").and_then(Value::as_str) {
            Some("user") => {
                if let Some(text) = human_prompt(&entry) {
                    self.start(text, at, false);
                } else if let Some(text) = slash_command(&entry) {
                    self.start(text, at, true);
                }
            }
            Some("attachment") => {
                if let Some(text) = queued_prompt(&entry) {
                    self.start(text, at, false);
                }
            }
            Some("assistant") => self.assistant(&entry, at),
            _ => {}
        }
    }

    fn start(&mut self, request: String, at: u64, command: bool) {
        if let Some(last) = self.messages.last_mut() {
            if last.command && !last.answered {
                self.messages.pop();
            } else {
                last.done = true;
            }
        }
        self.messages.push(Message {
            request,
            requested_at: at / 1000,
            duration_ms: 0,
            done: false,
            response: None,
            started_ms: at,
            command,
            answered: false,
            reply_id: None,
        });
    }

    fn assistant(&mut self, entry: &Value, at: u64) {
        let Some(m) = self.messages.last_mut() else {
            return;
        };
        if m.done {
            return;
        }
        m.answered = true;
        m.duration_ms = m.duration_ms.max(at.saturating_sub(m.started_ms));
        let Some(message) = entry.get("message") else {
            return;
        };
        let text = text_of(message.get("content"));
        if text.trim().is_empty() {
            return;
        }
        let id = message
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string);
        match &mut m.response {
            Some(reply) if id.is_some() && m.reply_id == id => {
                reply.push_str("\n\n");
                reply.push_str(&text);
            }
            reply => *reply = Some(text),
        }
        m.reply_id = id;
    }

    /// The messages so far. The last one is `done` only once `session`
    /// shows claude has finished its turn; pass `None` to leave it open.
    pub fn messages(&self, session: Option<&SessionInfo>) -> Vec<Message> {
        let mut out: Vec<Message> = self
            .messages
            .iter()
            .filter(|m| !m.command || m.answered)
            .cloned()
            .collect();
        if let Some(last) = out.last_mut()
            && !last.done
            && let Some(session) = session
        {
            if turn_finished(session) {
                last.done = true;
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                last.duration_ms = last.duration_ms.max(now.saturating_sub(last.started_ms));
            }
        }
        out
    }
}

/// Whether the session's claude has nothing more to say about the last
/// message: idle, or no longer running. Background tasks and permission
/// prompts leave the turn open.
pub fn turn_finished(session: &SessionInfo) -> bool {
    matches!(
        session.effective_status(),
        Status::Idle | Status::Exited | Status::Stopped
    )
}

/// A prompt the user typed or sent. Claude Code marks these with
/// `origin.kind == "human"`; older versions have no origin, so anything
/// that isn't a tool result, a command, or Claude Code's own note counts.
fn human_prompt(entry: &Value) -> Option<String> {
    let message = entry.get("message")?;
    match entry.get("origin") {
        Some(origin) => {
            if origin.get("kind").and_then(Value::as_str) != Some("human") {
                return None;
            }
        }
        None => {
            let flag = |key| entry.get(key).and_then(Value::as_bool) == Some(true);
            if flag("isMeta") || flag("isCompactSummary") {
                return None;
            }
            let content = message.get("content")?;
            if let Value::Array(blocks) = content
                && blocks
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            {
                return None;
            }
            let text = text_of(Some(content));
            let text = text.trim_start();
            if text.starts_with(['<', '/']) || text.starts_with("[Request interrupted") {
                return None;
            }
        }
    }
    let text = prompt_text(message.get("content")?);
    (!text.trim().is_empty()).then_some(text)
}

/// A message sent while claude was working, which it reads mid-turn.
fn queued_prompt(entry: &Value) -> Option<String> {
    let a = entry.get("attachment")?;
    if a.get("type").and_then(Value::as_str) != Some("queued_command")
        || a.get("commandMode").and_then(Value::as_str) != Some("prompt")
        || a.get("origin")
            .and_then(|o| o.get("kind"))
            .and_then(Value::as_str)
            != Some("human")
    {
        return None;
    }
    let text = prompt_text(a.get("prompt")?);
    (!text.trim().is_empty()).then_some(text)
}

/// `/name args` from the tags Claude Code records a slash command with.
fn slash_command(entry: &Value) -> Option<String> {
    let content = entry.get("message")?.get("content")?.as_str()?;
    let name = tag(content, "command-name")?.trim();
    if !name.starts_with('/') {
        return None;
    }
    Some(match tag(content, "command-args").map(str::trim) {
        Some(args) if !args.is_empty() => format!("{name} {args}"),
        _ => name.to_string(),
    })
}

fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{name}>"))?;
    Some(&text[start..start + end])
}

/// A prompt's text, with `[image]` where an image was attached.
fn prompt_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(str::to_string),
                Some("image") => Some("[image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Unix milliseconds from an RFC 3339 UTC time, as in
/// `2026-10-13T16:38:12.649Z`.
fn parse_timestamp(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, mo, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (h, mi, sec) = (t.next()??, t.next()??, t.next()??);
    let ms = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().chain("000".chars()).take(3).collect();
        digits.parse::<i64>().ok()?
    };
    // Days from 1970-01-01 to the date (Howard Hinnant's days_from_civil).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let total = ((days * 24 + h) * 60 + mi) * 60 + sec;
    u64::try_from(total * 1000 + ms).ok()
}

/// `45s`, `2m 14s`, `1h 3m`.
pub fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs / 60 % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "user", "timestamp": ts, "origin": {"kind": "human"},
            "message": {"role": "user", "content": text},
        })
        .to_string()
    }

    fn assistant(id: &str, blocks: Value, ts: &str) -> String {
        serde_json::json!({
            "type": "assistant", "timestamp": ts,
            "message": {"id": id, "role": "assistant", "content": blocks},
        })
        .to_string()
    }

    fn text(t: &str) -> Value {
        serde_json::json!([{"type": "text", "text": t}])
    }

    fn tool() -> Value {
        serde_json::json!([{"type": "tool_use", "id": "t", "name": "Bash", "input": {}}])
    }

    fn tool_result(ts: &str) -> String {
        serde_json::json!({
            "type": "user", "timestamp": ts,
            "message": {"content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]},
        })
        .to_string()
    }

    fn lines(lines: &[String]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    #[test]
    fn timestamps_parse_to_unix_millis() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_timestamp("2026-10-13T16:38:12.649Z"),
            Some(1_791_909_492_649)
        );
        assert_eq!(
            parse_timestamp("2000-03-01T00:00:01.5Z"),
            Some(951_868_801_500)
        );
        assert_eq!(parse_timestamp("nonsense"), None);
    }

    #[test]
    fn each_prompt_gets_its_final_reply_and_duration() {
        let t = Transcript::parse(&lines(&[
            user("Fix the bug", "2026-01-01T00:00:00Z"),
            assistant("a1", text("Looking at it."), "2026-01-01T00:00:02Z"),
            assistant("a1", tool(), "2026-01-01T00:00:03Z"),
            tool_result("2026-01-01T00:00:04Z"),
            assistant("a2", text("Fixed it."), "2026-01-01T00:01:05Z"),
            user("Thanks", "2026-01-01T00:02:00Z"),
        ]));
        let m = t.messages(None);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].request, "Fix the bug");
        assert_eq!(m[0].response.as_deref(), Some("Fixed it."));
        assert_eq!(m[0].duration_ms, 65_000);
        assert!(m[0].done);
        assert_eq!(m[1].request, "Thanks");
        assert_eq!(m[1].response, None);
        assert!(!m[1].done);
    }

    #[test]
    fn text_blocks_of_one_reply_are_joined() {
        let t = Transcript::parse(&lines(&[
            user("Hi", "2026-01-01T00:00:00Z"),
            assistant("a1", text("One."), "2026-01-01T00:00:01Z"),
            assistant("a1", text("Two."), "2026-01-01T00:00:01Z"),
        ]));
        assert_eq!(
            t.messages(None)[0].response.as_deref(),
            Some("One.\n\nTwo.")
        );
    }

    #[test]
    fn noise_is_left_out() {
        let meta = serde_json::json!({
            "type": "user", "isMeta": true, "timestamp": "2026-01-01T00:00:01Z",
            "message": {"content": "<local-command-caveat>Caveat</local-command-caveat>"},
        });
        let model = serde_json::json!({
            "type": "user", "timestamp": "2026-01-01T00:00:01Z",
            "message": {"content": "<command-name>/model</command-name>\n<command-args></command-args>"},
        });
        let notification = serde_json::json!({
            "type": "user", "timestamp": "2026-01-01T00:00:01Z",
            "origin": {"kind": "task-notification"},
            "message": {"content": "<task-notification>done</task-notification>"},
        });
        let sidechain = serde_json::json!({
            "type": "user", "isSidechain": true, "timestamp": "2026-01-01T00:00:01Z",
            "origin": {"kind": "human"}, "message": {"content": "agent prompt"},
        });
        let t = Transcript::parse(&lines(&[
            meta.to_string(),
            model.to_string(),
            user("Real", "2026-01-01T00:00:02Z"),
            notification.to_string(),
            sidechain.to_string(),
            assistant("a1", text("Reply"), "2026-01-01T00:00:03Z"),
        ]));
        let m = t.messages(None);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].request, "Real");
        assert_eq!(m[0].response.as_deref(), Some("Reply"));
    }

    #[test]
    fn answered_slash_commands_count() {
        let review = serde_json::json!({
            "type": "user", "timestamp": "2026-01-01T00:00:00Z",
            "message": {"content": "<command-message>review</command-message>\n<command-name>/review</command-name>\n<command-args>main</command-args>"},
        });
        let t = Transcript::parse(&lines(&[
            review.to_string(),
            assistant("a1", text("Looks good"), "2026-01-01T00:00:09Z"),
        ]));
        let m = t.messages(None);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].request, "/review main");
    }

    #[test]
    fn mid_turn_messages_start_a_new_message() {
        let queued = serde_json::json!({
            "type": "attachment", "timestamp": "2026-01-01T00:00:10Z",
            "attachment": {"type": "queued_command", "commandMode": "prompt",
                "origin": {"kind": "human"}, "prompt": "Also add a test"},
        });
        let t = Transcript::parse(&lines(&[
            user("Fix it", "2026-01-01T00:00:00Z"),
            assistant("a1", text("On it"), "2026-01-01T00:00:05Z"),
            queued.to_string(),
            assistant("a2", text("Added"), "2026-01-01T00:00:20Z"),
        ]));
        let m = t.messages(None);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].response.as_deref(), Some("On it"));
        assert_eq!(m[1].request, "Also add a test");
        assert_eq!(m[1].duration_ms, 10_000);
    }

    #[test]
    fn prompts_without_origin_fall_back_to_their_shape() {
        let old = serde_json::json!({
            "type": "user", "timestamp": "2026-01-01T00:00:00Z",
            "message": {"content": "Old prompt"},
        });
        let interrupted = serde_json::json!({
            "type": "user", "timestamp": "2026-01-01T00:00:01Z",
            "message": {"content": [{"type": "text", "text": "[Request interrupted by user]"}]},
        });
        let t = Transcript::parse(&lines(&[
            old.to_string(),
            tool_result("2026-01-01T00:00:01Z"),
            interrupted.to_string(),
        ]));
        let m = t.messages(None);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].request, "Old prompt");
    }

    #[test]
    fn partial_lines_wait_for_the_rest() {
        let line = user("Hello", "2026-01-01T00:00:00Z") + "\n";
        let (a, b) = line.split_at(10);
        let mut t = Transcript::default();
        t.feed(a.as_bytes());
        assert!(t.messages(None).is_empty());
        t.feed(b.as_bytes());
        assert_eq!(t.messages(None)[0].request, "Hello");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(900), "0s");
        assert_eq!(format_duration(45_000), "45s");
        assert_eq!(format_duration(134_000), "2m 14s");
        assert_eq!(format_duration(3_780_000), "1h 3m");
    }
}
