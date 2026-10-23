//! claude-mux: run and manage many Claude Code sessions.
//!
//! Shared by the `claude-mux` CLI and its daemon (`claude-mux daemon`).

pub mod client;
pub mod daemon;
pub mod db;
pub mod hooks;
pub mod manage;
pub mod messages;
pub mod paths;
pub mod protocol;
pub mod term;
pub mod upgrade;

pub fn ago(ts: u64) -> String {
    let secs = paths::now().saturating_sub(ts);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

pub fn short_path(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}
