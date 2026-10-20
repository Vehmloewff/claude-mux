//! Choosing sessions on the command line: by id, by a unique id prefix, or
//! from a picker when the caller has a terminal to show one on.

use std::io::IsTerminal;

use anyhow::{Result, bail};

use claude_mux::protocol::SessionInfo;
use claude_mux::{ago, short_path};

/// Which sessions the picker offers when no id is given.
#[derive(Clone, Copy)]
pub enum Offer {
    All,
    Running,
    Resumable,
}

impl Offer {
    fn includes(self, s: &SessionInfo) -> bool {
        match self {
            Offer::All => true,
            Offer::Running => s.status.is_live(),
            Offer::Resumable => !s.status.is_live() || s.asleep,
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Offer::All => "sessions",
            Offer::Running => "running sessions",
            Offer::Resumable => "stopped sessions",
        }
    }
}

/// Sessions `query` names: the one whose id it is, else every session
/// whose id starts with it.
pub fn matches<'a>(sessions: &'a [SessionInfo], query: &str) -> Vec<&'a SessionInfo> {
    if let Some(s) = sessions.iter().find(|s| s.id == query) {
        return vec![s];
    }
    let query = query.to_ascii_lowercase();
    sessions
        .iter()
        .filter(|s| s.id.starts_with(&query))
        .collect()
}

/// The id of the session `query` names. Without a query, the only session
/// `offer` includes. Asks which one when that leaves several.
pub fn one(
    sessions: &[SessionInfo],
    query: Option<&str>,
    offer: Offer,
    prompt: &str,
) -> Result<String> {
    match query {
        Some(query) => {
            let hits = matching(sessions, query)?;
            if let [only] = hits[..] {
                return Ok(only.id.clone());
            }
            let why = format!(
                "`{query}` matches {} sessions; pass more of the id to --session",
                hits.len()
            );
            choose(
                &hits,
                &format!("`{query}` matches {} sessions. {prompt}", hits.len()),
                &why,
            )
        }
        None => {
            let candidates = offered(sessions, offer)?;
            if let [only] = candidates[..] {
                return Ok(chosen_for_you(only));
            }
            let why = format!(
                "{} {}; pick one with --session",
                candidates.len(),
                offer.noun()
            );
            choose(&candidates, prompt, &why)
        }
    }
}

/// The ids of the sessions `queries` name, or of the sessions picked from
/// those `offer` includes when there are no queries.
pub fn many(
    sessions: &[SessionInfo],
    queries: &[String],
    offer: Offer,
    prompt: &str,
) -> Result<Vec<String>> {
    if queries.is_empty() {
        let candidates = offered(sessions, offer)?;
        if let [only] = candidates[..] {
            return Ok(vec![chosen_for_you(only)]);
        }
        if !interactive() {
            bail!(
                "{} {}; pick with --session\n{}",
                candidates.len(),
                offer.noun(),
                rows(&candidates)
            );
        }
        let mut picker = cliclack::multiselect(prompt).required(true);
        for s in &candidates {
            picker = picker.item(s.id.clone(), label(s), hint(s));
        }
        return picker.interact().map_err(cancelled);
    }
    let mut ids: Vec<String> = Vec::new();
    for query in queries {
        let id = one(sessions, Some(query), offer, prompt)?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn matching<'a>(sessions: &'a [SessionInfo], query: &str) -> Result<Vec<&'a SessionInfo>> {
    if query.trim().is_empty() {
        bail!("session id is empty");
    }
    let hits = matches(sessions, query);
    if hits.is_empty() {
        bail!("no session matches `{query}` (see `claude-mux list`)");
    }
    Ok(hits)
}

fn offered(sessions: &[SessionInfo], offer: Offer) -> Result<Vec<&SessionInfo>> {
    let mut candidates: Vec<&SessionInfo> = sessions.iter().filter(|s| offer.includes(s)).collect();
    if candidates.is_empty() {
        if sessions.is_empty() {
            bail!("no sessions (create one with `claude-mux create \"...\"`)");
        }
        bail!("no {}", offer.noun());
    }
    // Most recently active first: usually the one meant.
    candidates.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(candidates)
}

/// Note which session was used when no `--session` named it.
fn chosen_for_you(s: &SessionInfo) -> String {
    eprintln!("claude-mux: session {} ({})", s.short_id(), label(s));
    s.id.clone()
}

/// Ask which of `candidates` is meant, or explain with `why` and list them
/// when there is no terminal to ask on.
fn choose(candidates: &[&SessionInfo], prompt: &str, why: &str) -> Result<String> {
    if !interactive() {
        bail!("{why}\n{}", rows(candidates));
    }
    let mut picker = cliclack::select(prompt);
    for s in candidates {
        picker = picker.item(s.id.clone(), label(s), hint(s));
    }
    picker.interact().map_err(cancelled)
}

/// The picker draws on stderr and reads the keyboard from stdin.
fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// The picker has already said it was cancelled; exit like an interrupted
/// command would.
fn cancelled(e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::Interrupted {
        std::process::exit(130);
    }
    e.into()
}

fn label(s: &SessionInfo) -> String {
    crate::truncate(s.display_name(), 60)
}

fn hint(s: &SessionInfo) -> String {
    let asleep = if s.asleep { " (asleep)" } else { "" };
    format!(
        "{} · {}{asleep} · {} · {}",
        s.short_id(),
        s.status_label(),
        short_path(&s.cwd),
        ago(s.updated_at)
    )
}

fn rows(sessions: &[&SessionInfo]) -> String {
    sessions
        .iter()
        .map(|s| {
            format!(
                "  {}  {:<11}  {:<40}  {}",
                s.short_id(),
                s.status_label(),
                crate::truncate(s.display_name(), 40),
                short_path(&s.cwd)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
