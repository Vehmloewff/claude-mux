//! Claude Code hook integration.
//!
//! Each managed claude is started with `--settings` containing hooks that
//! call `claude-mux hook <Event>`. The hook forwards its JSON payload to the
//! daemon, which is how claude-mux knows when a session is working, blocked on
//! a permission prompt, or done with its turn.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{Value, json};

use crate::paths;
use crate::protocol::Request;

pub const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PostToolUse",
    "Notification",
    "Stop",
    "SessionEnd",
];

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The `--settings` JSON passed to every managed claude.
pub fn settings_json() -> String {
    let exe = paths::sibling_exe("claude-mux").to_string_lossy().into_owned();
    let mut hooks = serde_json::Map::new();
    for event in EVENTS {
        hooks.insert(
            (*event).into(),
            json!([{ "hooks": [{
                "type": "command",
                "command": format!("{} hook {}", shell_quote(&exe), event),
                "timeout": 10,
            }]}]),
        );
    }
    json!({ "hooks": hooks }).to_string()
}

/// Entry point for `claude-mux hook <event>`. Never fails loudly: a broken
/// daemon must not break the claude session.
pub fn run_hook(event: &str) {
    let Ok(id) = std::env::var("CLAUDE_MUX_SESSION_ID") else { return };
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let req = Request::Hook { id, event: event.to_string(), payload };
    let Ok(stream) = UnixStream::connect(paths::socket()) else { return };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    let Ok(mut line) = serde_json::to_vec(&req) else { return };
    line.push(b'\n');
    let mut writer = &stream;
    if writer.write_all(&line).is_err() {
        return;
    }
    let mut ack = String::new();
    let _ = BufReader::new(&stream).read_line(&mut ack);
}

/// Pull the final assistant text out of a Stop hook payload, falling back
/// to the transcript file for older Claude Code versions.
pub fn last_assistant_message(payload: &Value) -> Option<String> {
    if let Some(msg) = payload.get("last_assistant_message").and_then(Value::as_str)
        && !msg.trim().is_empty() {
            return Some(msg.to_string());
        }
    let path = payload.get("transcript_path")?.as_str()?;
    let data = std::fs::read_to_string(path).ok()?;
    data.lines().rev().find_map(|line| {
        let entry: Value = serde_json::from_str(line).ok()?;
        if entry.get("type")?.as_str()? != "assistant" {
            return None;
        }
        let content = entry.get("message")?.get("content")?;
        let text = match content {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => return None,
        };
        (!text.trim().is_empty()).then_some(text)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_register_every_event() {
        let v: Value = serde_json::from_str(&settings_json()).unwrap();
        for e in EVENTS {
            let cmd = v["hooks"][e][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(cmd.ends_with(&format!(" hook {e}")));
        }
    }

    #[test]
    fn reads_transcript_fallback() {
        let dir = std::env::temp_dir().join(format!("claude-mux-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(
            &path,
            [
                r#"{"type":"user","message":{"content":"hi"}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"first"}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done!"},{"type":"tool_use"}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use"}]}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let payload = json!({ "transcript_path": path });
        assert_eq!(last_assistant_message(&payload).as_deref(), Some("done!"));
        let direct = json!({ "last_assistant_message": "direct", "transcript_path": path });
        assert_eq!(last_assistant_message(&direct).as_deref(), Some("direct"));
    }
}
