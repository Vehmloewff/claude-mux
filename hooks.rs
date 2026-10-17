//! Claude Code hook integration.
//!
//! Each managed claude is started with `--settings` containing hooks that
//! call `claude-mux hook <Event>`. The hook forwards its JSON payload to the
//! daemon, which is how claude-mux knows when a session is working, blocked on
//! a permission prompt, or done with its turn.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

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
    let exe = paths::exe().to_string_lossy().into_owned();
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
pub async fn run_hook(event: &str) {
    let Ok(process) = std::env::var("CLAUDE_MUX_PROCESS") else { return };
    let mut input = String::new();
    let _ = tokio::io::stdin().read_to_string(&mut input).await;
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let req = Request::Hook { process, event: event.to_string(), payload };
    let Ok(stream) = UnixStream::connect(paths::socket()).await else { return };
    let Ok(mut line) = serde_json::to_vec(&req) else { return };
    line.push(b'\n');
    let (mut reader, mut writer) = stream.into_split();
    let write = async { writer.write_all(&line).await };
    let mut ack = String::new();
    let read = async { BufReader::new(&mut reader).read_line(&mut ack).await };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), write).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), read).await;
}

/// One plan-mode question Claude is waiting on. Read from the session
/// transcript (`AskUserQuestion`), not from the screen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Ask {
    pub questions: Vec<AskQuestion>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AskQuestion {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub question: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub multi_select: bool,
    pub options: Vec<AskOption>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AskOption {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Set for a screen dialog, where one choice is highlighted.
    #[serde(default, skip_serializing_if = "is_false")]
    pub selected: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// `~/.claude/projects/<cwd-with-non-alphanumerics-as-dashes>/<session-id>.jsonl`.
/// `CLAUDE_CONFIG_DIR` overrides `~/.claude`, matching Claude Code.
pub fn transcript_path(cwd: &str, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) || session_id.contains("..") {
        return None;
    }
    let dir: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let home = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/tmp".into()).join(".claude")
        });
    Some(home.join("projects").join(dir).join(format!("{session_id}.jsonl")))
}

/// The `AskUserQuestion` Claude is blocked on, if the transcript has one
/// with no later tool result.
pub async fn pending_ask_for(cwd: &str, session_id: &str) -> Option<Ask> {
    let path = transcript_path(cwd, session_id)?;
    pending_ask(&read_transcript_tail(&path).await?)
}

/// A pending question is the last thing written, so the tail of the file is
/// enough. Whole transcripts run to tens of megabytes.
async fn read_transcript_tail(path: &Path) -> Option<String> {
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let len = file.metadata().await.ok()?.len();
    const MAX: u64 = 1024 * 1024;
    let truncated = len > MAX;
    if truncated {
        file.seek(SeekFrom::Start(len - MAX)).await.ok()?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.ok()?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated && let Some(i) = text.find('\n') {
        text.drain(..=i);
    }
    Some(text)
}

pub fn pending_ask(jsonl: &str) -> Option<Ask> {
    let mut pending: Option<(String, Ask)> = None;
    for line in jsonl.lines() {
        if !line.contains("AskUserQuestion") && !line.contains("tool_result") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let Some(blocks) = entry.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") if block.get("name").and_then(|n| n.as_str()) == Some("AskUserQuestion") => {
                    let Some(id) = block.get("id").and_then(|i| i.as_str()) else { continue };
                    if let Some(ask) = block.get("input").and_then(parse_ask) {
                        pending = Some((id.to_string(), ask));
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str())
                        && pending.as_ref().is_some_and(|(pid, _)| pid == id)
                    {
                        pending = None;
                    }
                }
                _ => {}
            }
        }
    }
    pending.map(|(_, ask)| ask)
}

fn parse_ask(input: &Value) -> Option<Ask> {
    let questions: Vec<_> = input.get("questions")?.as_array()?.iter().filter_map(parse_question).collect();
    (!questions.is_empty()).then_some(Ask { questions })
}

fn parse_question(v: &Value) -> Option<AskQuestion> {
    let question = nonempty(v, "question")?;
    let options = v
        .get("options")
        .and_then(|o| o.as_array())
        .map(|opts| opts.iter().filter_map(parse_option).collect())
        .unwrap_or_default();
    Some(AskQuestion {
        header: nonempty(v, "header"),
        question,
        multi_select: v.get("multiSelect").and_then(|b| b.as_bool()).unwrap_or(false),
        options,
    })
}

fn parse_option(v: &Value) -> Option<AskOption> {
    Some(AskOption {
        label: nonempty(v, "label")?,
        description: nonempty(v, "description"),
        preview: nonempty(v, "preview"),
        selected: false,
    })
}

fn nonempty(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Pull the final assistant text out of a Stop hook payload, falling back
/// to the transcript file for older Claude Code versions.
pub async fn last_assistant_message(payload: &Value) -> Option<String> {
    if let Some(msg) = payload.get("last_assistant_message").and_then(Value::as_str)
        && !msg.trim().is_empty() {
            return Some(msg.to_string());
        }
    let path = payload.get("transcript_path")?.as_str()?;
    let data = tokio::fs::read_to_string(path).await.ok()?;
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
        assert_eq!(
            tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(last_assistant_message(&payload)).as_deref(),
            Some("done!")
        );
        let direct = json!({ "last_assistant_message": "direct", "transcript_path": path });
        assert_eq!(
            tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(last_assistant_message(&direct)).as_deref(),
            Some("direct")
        );
    }

    fn ask_line(id: &str, body: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"AskUserQuestion","input":{body}}}]}}}}"#
        )
    }

    fn result_line(id: &str) -> String {
        format!(r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","content":"answered"}}]}}}}"#)
    }

    #[test]
    fn pending_ask_is_the_unanswered_plan_question() {
        let body = r#"{"questions":[{"header":"License","question":"Which license?","multiSelect":false,"options":[{"label":"MIT","description":"Short","preview":"MIT text"}]},{"header":"Layout","question":"How should files be laid out?","multiSelect":true,"options":[{"label":"Paired files","description":"up.sql and down.sql"}]}]}"#;
        let open = format!("{}\n{}\n", ask_line("t1", body), r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b","name":"Bash","input":{}}]}}"#);
        let ask = pending_ask(&open).unwrap();
        assert_eq!(ask.questions.len(), 2);
        assert_eq!(ask.questions[0].header.as_deref(), Some("License"));
        assert_eq!(ask.questions[0].question, "Which license?");
        assert!(!ask.questions[0].multi_select);
        assert_eq!(ask.questions[0].options[0].label, "MIT");
        assert_eq!(ask.questions[0].options[0].description.as_deref(), Some("Short"));
        assert_eq!(ask.questions[0].options[0].preview.as_deref(), Some("MIT text"));
        assert!(ask.questions[1].multi_select);

        let answered = format!("{}\n{}\n", ask_line("t1", body), result_line("t1"));
        assert!(pending_ask(&answered).is_none());

        let replaced = format!("{}\n{}\n", ask_line("old", body), ask_line("new", body));
        assert!(pending_ask(&replaced).is_some());
        let cleared_old = format!("{}\n{}\n", ask_line("new", body), result_line("old"));
        assert!(pending_ask(&cleared_old).is_some());
    }

    #[test]
    fn transcript_path_encodes_cwd() {
        let path = transcript_path("/Users/elijah.mooring/Projects/claude-mux", "abc-123").unwrap();
        assert!(path.ends_with("projects/-Users-elijah-mooring-Projects-claude-mux/abc-123.jsonl"));
        assert!(transcript_path("/tmp", "../x").is_none());
        assert!(transcript_path("/tmp", "").is_none());
    }
}
