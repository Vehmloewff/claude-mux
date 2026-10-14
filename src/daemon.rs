//! The claudex daemon: owns every claude process, its PTY and its
//! libghostty terminal, and serves the CLI / TUI over a Unix socket.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::Value;

use crate::db::Db;
use crate::hooks;
use crate::paths;
use crate::protocol::*;
use crate::term::{PtyWriter, TRUST_PROMPT, TermEvent, TermHandle};

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;

/// Env vars that make claude think it is nested inside another session.
const STRIP_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDEX_SESSION_ID",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TMUX",
    "TMUX_PANE",
];

struct Proc {
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

struct Session {
    info: SessionInfo,
    proc: Option<Proc>,
    /// Kept after exit so the final screen can still be viewed.
    term: Option<TermHandle>,
    size: (u16, u16),
    queue: VecDeque<String>,
    /// Bumped whenever the screen changes.
    screen_gen: u64,
    /// Set when we typed a message and expect a UserPromptSubmit hook.
    awaiting_submit: Option<(Instant, Duration)>,
    /// A dialog detected on screen (trust prompt, permission prompt, ...).
    screen_prompt: Option<String>,
    /// Status to return to when an on-screen dialog goes away.
    status_before_prompt: Option<Status>,
    /// Whether the initial context is still to be submitted.
    initial_pending: bool,
    /// When claude last reported any hook event.
    last_hook: Option<Instant>,
    /// The process is being stopped on request (so it is resumable).
    killed: bool,
    /// When the footer stopped showing background tasks while we were in
    /// `Background` (a fallback if claude never wakes up).
    background_cleared: Option<Instant>,
}

struct Inner {
    sessions: BTreeMap<String, Session>,
    /// Bumped whenever any `SessionInfo` changes.
    version: u64,
}

pub struct Daemon {
    inner: Mutex<Inner>,
    /// Only used with `inner` locked, which serializes writes.
    db: Db,
    changed: Condvar,
    shutting_down: AtomicBool,
}

type Shared = Arc<Daemon>;

/// Outcome of trying to become the daemon.
pub enum Started {
    /// Served until shutdown.
    Ran,
    /// Another claudexd holds the lock; nothing to do.
    AlreadyRunning,
}

pub fn run() -> Result<Started> {
    std::fs::create_dir_all(paths::home())?;
    // Exactly one daemon per state directory, even when several commands
    // start one at the same moment. The lock is held until the process exits
    // (the file is close-on-exec, so sessions don't inherit it).
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths::home().join("daemon.lock"))
        .context("open daemon lock file")?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(Started::AlreadyRunning);
    }
    std::mem::forget(lock);
    let sock = paths::socket();
    let db = Db::open(&paths::db_file())?;
    let sessions = load_sessions(&db)?;
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    eprintln!("[{}] claudex daemon listening on {}", paths::now(), sock.display());

    let daemon: Shared = Arc::new(Daemon {
        inner: Mutex::new(Inner { sessions, version: 1 }),
        db,
        changed: Condvar::new(),
        shutting_down: AtomicBool::new(false),
    });

    {
        let d = daemon.clone();
        std::thread::spawn(move || ticker(d));
    }
    {
        // SIGTERM and SIGINT stop gracefully (like `claudex shutdown`), leaving
        // sessions resumable. The handler only sets a flag; this thread acts.
        let d = daemon.clone();
        install_signal_handlers();
        std::thread::spawn(move || {
            while !STOP_SIGNAL.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
            }
            eprintln!("[{}] received stop signal", paths::now());
            shutdown(&d);
        });
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let d = daemon.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle_conn(&d, stream) {
                eprintln!("connection error: {e:#}");
            }
        });
    }
    Ok(Started::Ran)
}

fn load_sessions(db: &Db) -> Result<BTreeMap<String, Session>> {
    let mut map = BTreeMap::new();
    for mut info in db.load_sessions().context("load sessions")? {
        if info.status.is_live() {
            info.status = Status::Stopped;
            info.status_detail = Some("daemon restarted; resume to continue".into());
        }
        info.queued = 0;
        map.insert(
            info.id.clone(),
            Session {
                info,
                proc: None,
                term: None,
                size: (DEFAULT_COLS, DEFAULT_ROWS),
                queue: VecDeque::new(),
                screen_gen: 0,
                awaiting_submit: None,
                screen_prompt: None,
                status_before_prompt: None,
                initial_pending: false,
                last_hook: None,
                killed: false,
                background_cleared: None,
            },
        );
    }
    Ok(map)
}

impl Daemon {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record that session info changed: persist and wake watchers.
    fn touch(&self, inner: &mut Inner) {
        inner.version += 1;
        if let Err(e) = self.db.save_sessions(inner.sessions.values().map(|s| &s.info)) {
            eprintln!("[{}] failed to save sessions: {e:#}", paths::now());
        }
        self.changed.notify_all();
    }
}

fn set_status(s: &mut Session, status: Status, detail: Option<String>) {
    s.info.status = status;
    s.info.status_detail = detail;
    s.info.updated_at = paths::now();
    if status != Status::NeedsInput {
        s.status_before_prompt = None;
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn new_id(inner: &Inner) -> String {
    loop {
        let id: String = uuid::Uuid::new_v4().simple().to_string()[..6].to_string();
        if !inner.sessions.contains_key(&id) && id.chars().any(|c| c.is_ascii_alphabetic()) {
            return id;
        }
    }
}

// ---------------------------------------------------------------------------
// Connections

fn write_line<T: serde::Serialize>(w: &mut impl Write, msg: &T) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()?;
    Ok(())
}

fn handle_conn(d: &Shared, stream: UnixStream) -> Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let req: Request = serde_json::from_str(&line).context("invalid request")?;
    match req {
        Request::Subscribe => subscribe(d, reader, writer),
        Request::Shutdown => {
            write_line(&mut writer, &Response::Ok)?;
            shutdown(d);
            Ok(())
        }
        req => {
            let resp = handle_request(d, req).unwrap_or_else(|e| Response::Error { message: format!("{e:#}") });
            write_line(&mut writer, &resp)
        }
    }
}

fn session_mut<'a>(inner: &'a mut Inner, id: &str) -> Result<&'a mut Session> {
    inner.sessions.get_mut(id).ok_or_else(|| anyhow!("no session with id '{id}'"))
}

fn handle_request(d: &Shared, req: Request) -> Result<Response> {
    match req {
        Request::Ping => Ok(Response::Ok),
        Request::Create(c) => {
            let info = create(d, c)?;
            Ok(Response::Session { session: info })
        }
        Request::Send { id, message, now } => {
            let mut inner = d.lock();
            let s = session_mut(&mut inner, &id)?;
            if !s.info.status.is_live() {
                bail!("session '{id}' is {}; resume it first", s.info.status.label());
            }
            if now {
                type_message(s, message);
            } else {
                s.queue.push_back(message);
                s.info.queued = s.queue.len();
                deliver_if_ready(s);
            }
            let info = s.info.clone();
            d.touch(&mut inner);
            Ok(Response::Session { session: info })
        }
        Request::Wait { ids, any, timeout_ms, turn } => wait(d, ids, any, timeout_ms, turn),
        Request::List => {
            let inner = d.lock();
            Ok(Response::Sessions { sessions: inner.sessions.values().map(|s| s.info.clone()).collect() })
        }
        Request::Get { id } => {
            let mut inner = d.lock();
            Ok(Response::Session { session: session_mut(&mut inner, &id)?.info.clone() })
        }
        Request::Peek { id, ansi } => {
            let term = {
                let mut inner = d.lock();
                session_mut(&mut inner, &id)?.term.clone()
            };
            let term = term.ok_or_else(|| anyhow!("session '{id}' has no screen (not running)"))?;
            let text = term.text(false, ansi).ok_or_else(|| anyhow!("terminal did not respond"))?;
            Ok(Response::Text { text })
        }
        Request::Kill { id } => {
            let mut inner = d.lock();
            let s = session_mut(&mut inner, &id)?;
            if let Some(p) = s.proc.as_mut() {
                s.killed = true;
                let _ = p.killer.kill();
            }
            s.queue.clear();
            s.info.queued = 0;
            d.touch(&mut inner);
            Ok(Response::Ok)
        }
        Request::Remove { id } => {
            let mut inner = d.lock();
            let mut s = inner.sessions.remove(&id).ok_or_else(|| anyhow!("no session with id '{id}'"))?;
            if let Some(p) = s.proc.as_mut() {
                let _ = p.killer.kill();
            }
            d.touch(&mut inner);
            Ok(Response::Ok)
        }
        Request::Resume { id } => {
            let info = resume(d, &id)?;
            Ok(Response::Session { session: info })
        }
        Request::Rename { id, name } => {
            let mut inner = d.lock();
            let s = session_mut(&mut inner, &id)?;
            s.info.name = name.filter(|n| !n.trim().is_empty());
            let info = s.info.clone();
            d.touch(&mut inner);
            Ok(Response::Session { session: info })
        }
        Request::Keys { id, keys } => {
            let term = {
                let mut inner = d.lock();
                session_mut(&mut inner, &id)?.term.clone()
            };
            let term = term.ok_or_else(|| anyhow!("session '{id}' is not running"))?;
            for key in keys {
                term.key(key);
                std::thread::sleep(Duration::from_millis(30));
            }
            // Let claude redraw and the dialog scan catch up, so an immediate
            // `wait` sees the post-keypress status.
            std::thread::sleep(Duration::from_millis(600));
            Ok(Response::Ok)
        }
        Request::Hook { id, event, payload } => {
            on_hook(d, &id, &event, payload);
            Ok(Response::Ok)
        }
        Request::Shutdown | Request::Subscribe => unreachable!(),
    }
}

fn wait(d: &Shared, ids: Vec<String>, any: bool, timeout_ms: Option<u64>, turn: bool) -> Result<Response> {
    let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    let mut inner = d.lock();
    if ids.is_empty() {
        bail!("no session ids given");
    }
    loop {
        let mut infos = Vec::new();
        for id in &ids {
            infos.push(session_mut(&mut inner, id)?.info.clone());
        }
        let done = |s: Status| if turn { s.is_turn_over() } else { s.is_settled() };
        let settled = infos.iter().filter(|i| done(i.effective_status())).count();
        if (any && settled > 0) || settled == infos.len() {
            return Ok(Response::Wait { sessions: infos, timed_out: false });
        }
        let wait_for = match deadline {
            Some(dl) => {
                let now = Instant::now();
                if now >= dl {
                    return Ok(Response::Wait { sessions: infos, timed_out: true });
                }
                (dl - now).min(Duration::from_secs(5))
            }
            None => Duration::from_secs(5),
        };
        inner = d.changed.wait_timeout(inner, wait_for).unwrap_or_else(|e| e.into_inner()).0;
    }
}

// ---------------------------------------------------------------------------
// Session lifecycle

fn create(d: &Shared, c: CreateRequest) -> Result<SessionInfo> {
    let cwd = std::path::PathBuf::from(&c.cwd);
    if !cwd.is_dir() {
        bail!("working directory '{}' does not exist", c.cwd);
    }
    let mut inner = d.lock();
    let id = match c.id {
        Some(id) => {
            if !valid_id(&id) {
                bail!("invalid id '{id}' (use letters, digits, '-', '_' or '.')");
            }
            if inner.sessions.contains_key(&id) {
                bail!("a session with id '{id}' already exists");
            }
            id
        }
        None => new_id(&inner),
    };
    let now = paths::now();
    let info = SessionInfo {
        id: id.clone(),
        name: c.name,
        cwd: c.cwd,
        context: c.context.clone(),
        status: Status::Starting,
        status_detail: None,
        title: None,
        claude_session_id: uuid::Uuid::new_v4().to_string(),
        created_at: now,
        updated_at: now,
        exit_code: None,
        queued: 0,
        turns: 0,
        last_message: None,
        claude_args: c.claude_args,
        auto_trust: c.trust,
        background: Vec::new(),
    };
    let mut session = Session {
        info,
        proc: None,
        term: None,
        size: (DEFAULT_COLS, DEFAULT_ROWS),
        queue: VecDeque::new(),
        screen_gen: 0,
        awaiting_submit: None,
        screen_prompt: None,
        status_before_prompt: None,
        initial_pending: !c.context.trim().is_empty(),
        last_hook: None,
        killed: false,
        background_cleared: None,
    };
    if session.initial_pending {
        // Claude can take a while to boot, especially on first run.
        session.awaiting_submit = Some((Instant::now(), Duration::from_secs(90)));
    }
    spawn_claude(d, &mut session, false)?;
    let info = session.info.clone();
    inner.sessions.insert(id, session);
    d.touch(&mut inner);
    Ok(info)
}

fn resume(d: &Shared, id: &str) -> Result<SessionInfo> {
    let mut inner = d.lock();
    let s = session_mut(&mut inner, id)?;
    if s.info.status.is_live() {
        bail!("session '{id}' is still running");
    }
    s.initial_pending = false;
    s.awaiting_submit = None;
    s.screen_prompt = None;
    s.info.exit_code = None;
    set_status(s, Status::Starting, Some("resuming".into()));
    spawn_claude(d, s, true)?;
    let info = s.info.clone();
    d.touch(&mut inner);
    Ok(info)
}

fn claude_bin() -> String {
    std::env::var("CLAUDEX_CLAUDE_BIN").unwrap_or_else(|_| "claude".into())
}

fn spawn_claude(d: &Shared, s: &mut Session, resume: bool) -> Result<()> {
    let (cols, rows) = s.size;
    let pty = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .context("failed to open a pty")?;

    let mut cmd = CommandBuilder::new(claude_bin());
    if resume {
        cmd.args(["--resume", &s.info.claude_session_id]);
    } else {
        cmd.args(["--session-id", &s.info.claude_session_id]);
    }
    cmd.args(["--settings", &hooks::settings_json()]);
    if let Some(name) = &s.info.name {
        cmd.args(["--name", name]);
    }
    cmd.args(&s.info.claude_args);
    if !resume && !s.info.context.trim().is_empty() {
        cmd.arg("--");
        cmd.arg(&s.info.context);
    }
    cmd.cwd(&s.info.cwd);
    for key in STRIP_ENV {
        cmd.env_remove(key);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "claudex");
    cmd.env("CLAUDEX_SESSION_ID", &s.info.id);
    cmd.env("CLAUDEX_HOME", paths::home());
    cmd.env("CLAUDEX_SOCKET", paths::socket());
    // Make `claudex` itself available so sessions can orchestrate others.
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf())) {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut parts = vec![dir];
        parts.extend(std::env::split_paths(&path));
        if let Ok(joined) = std::env::join_paths(parts) {
            cmd.env("PATH", joined);
        }
    }

    let mut child = pty.slave.spawn_command(cmd).context("failed to start claude (is it on PATH?)")?;
    drop(pty.slave);
    let killer = child.clone_killer();
    let mut reader = pty.master.try_clone_reader()?;
    let writer: PtyWriter = Arc::new(Mutex::new(pty.master.take_writer()?));

    let id = s.info.id.clone();
    let term = {
        let d = d.clone();
        let id = id.clone();
        TermHandle::spawn(cols, rows, writer, move |ev| on_term_event(&d, &id, ev))
    };

    // PTY output -> terminal emulator.
    {
        let term = term.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => term.output(buf[..n].to_vec()),
                }
            }
        });
    }

    // Process exit.
    {
        let d = d.clone();
        let id = id.clone();
        std::thread::spawn(move || {
            let code = child.wait().map(|s| s.exit_code() as i32).ok();
            let mut inner = d.lock();
            if let Some(s) = inner.sessions.get_mut(&id) {
                s.proc = None;
                s.queue.clear();
                s.info.queued = 0;
                s.info.exit_code = code;
                s.awaiting_submit = None;
                s.screen_prompt = None;
                if d.shutting_down.load(Ordering::SeqCst) {
                    set_status(s, Status::Stopped, Some("daemon shut down; resume to continue".into()));
                } else if std::mem::take(&mut s.killed) {
                    set_status(s, Status::Stopped, Some("killed; resume to continue".into()));
                } else {
                    set_status(s, Status::Exited, code.map(|c| format!("exit code {c}")));
                }
                d.touch(&mut inner);
            }
        });
    }

    s.proc = Some(Proc { master: pty.master, killer });
    s.term = Some(term);
    Ok(())
}

static STOP_SIGNAL: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_: libc::c_int) {
    STOP_SIGNAL.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    // Handled (not ignored or blocked) signals reset to their defaults on
    // exec, so the claude children are unaffected.
    for sig in [libc::SIGTERM, libc::SIGINT] {
        unsafe {
            libc::signal(sig, on_stop_signal as *const () as libc::sighandler_t);
        }
    }
}

fn shutdown(d: &Shared) {
    if d.shutting_down.swap(true, Ordering::SeqCst) {
        return;
    }
    {
        let mut inner = d.lock();
        for s in inner.sessions.values_mut() {
            if let Some(p) = s.proc.as_mut() {
                let _ = p.killer.kill();
            }
        }
    }
    // Give exit watchers a moment to record the final state.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if d.lock().sessions.values().all(|s| s.proc.is_none()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut inner = d.lock();
    for s in inner.sessions.values_mut() {
        if s.info.status.is_live() {
            set_status(s, Status::Stopped, Some("daemon shut down; resume to continue".into()));
        }
    }
    d.touch(&mut inner);
    let _ = std::fs::remove_file(paths::socket());
    eprintln!("[{}] claudex daemon shut down", paths::now());
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// Typing messages into claude

/// Paste the message, then press Enter once claude has taken the paste.
fn type_message(s: &mut Session, message: String) {
    let Some(term) = s.term.clone() else { return };
    let grace = if message.trim_start().starts_with('/') { Duration::from_secs(4) } else { Duration::from_secs(15) };
    s.awaiting_submit = Some((Instant::now(), grace));
    if matches!(s.info.status, Status::Idle | Status::Background) {
        set_status(s, Status::Working, Some("message sent".into()));
    }
    std::thread::spawn(move || {
        term.scroll_reset();
        term.paste(message.trim_end().to_string());
        std::thread::sleep(Duration::from_millis(250));
        term.key(KeyInput::plain(KeyCode::Enter));
    });
}

fn deliver_if_ready(s: &mut Session) {
    // Claude is at its prompt in both states; a message just starts a turn.
    if !matches!(s.info.status, Status::Idle | Status::Background) || s.awaiting_submit.is_some() || s.screen_prompt.is_some() {
        return;
    }
    if let Some(msg) = s.queue.pop_front() {
        s.info.queued = s.queue.len();
        type_message(s, msg);
    }
}

/// Periodic housekeeping: recover from messages that never produced a
/// UserPromptSubmit hook (slash commands, swallowed input, ...).
fn ticker(d: Shared) {
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let mut inner = d.lock();
        let mut changed = false;
        for s in inner.sessions.values_mut() {
            // Background tasks vanished from the footer but claude never woke
            // up for them (e.g. they were killed): the session is idle.
            if let Some(at) = s.background_cleared
                && at.elapsed() > Duration::from_secs(10)
            {
                s.background_cleared = None;
                if s.info.status == Status::Background && !s.last_hook.is_some_and(|h| h > at) {
                    s.info.background.clear();
                    set_status(s, Status::Idle, None);
                    deliver_if_ready(s);
                    changed = true;
                }
            }
            if let Some((at, grace)) = s.awaiting_submit
                && at.elapsed() > grace
                && s.screen_prompt.is_none()
            {
                s.awaiting_submit = None;
                s.initial_pending = false;
                // Hooks since typing mean claude is busy (the message was
                // queued behind the current turn); otherwise it was eaten.
                let busy = s.last_hook.is_some_and(|h| h > at);
                if !busy && matches!(s.info.status, Status::Working | Status::Starting) {
                    // Back at the prompt; keep reporting background work if
                    // the last turn left some running.
                    set_status(s, resting_status(s), background_detail(&s.info.background));
                }
                deliver_if_ready(s);
                changed = true;
            }
        }
        if changed {
            d.touch(&mut inner);
        }
    }
}

/// Idle, or Background if the last turn left tasks running.
fn resting_status(s: &Session) -> Status {
    if s.info.background.is_empty() { Status::Idle } else { Status::Background }
}

// ---------------------------------------------------------------------------
// Status tracking

fn on_term_event(d: &Shared, id: &str, ev: TermEvent) {
    let mut inner = d.lock();
    let Some(s) = inner.sessions.get_mut(id) else { return };
    match ev {
        TermEvent::Dirty => {
            s.screen_gen += 1;
            d.changed.notify_all();
        }
        TermEvent::Title(title) => {
            // Keep the last meaningful title: claude resets it on exit and
            // uses its own name before it has a task summary.
            if !title.is_empty() && title != "Claude Code" && s.info.title.as_deref() != Some(&title) {
                s.info.title = Some(title);
                d.touch(&mut inner);
            }
        }
        TermEvent::Background(running) => {
            s.background_cleared = (!running && s.info.status == Status::Background).then(Instant::now);
        }
        TermEvent::Prompt(prompt) => {
            if s.info.auto_trust && prompt.as_deref() == Some(TRUST_PROMPT) {
                if let Some(term) = s.term.clone() {
                    std::thread::spawn(move || accept_trust(&term));
                }
                // Being answered for us; don't report it as needing input.
                s.screen_prompt = prompt;
                return;
            }
            s.screen_prompt = prompt.clone();
            match prompt {
                Some(text) if s.info.status.is_live() => {
                    if s.info.status != Status::NeedsInput {
                        s.status_before_prompt = Some(s.info.status);
                    }
                    set_status(s, Status::NeedsInput, Some(text));
                }
                None if s.info.status == Status::NeedsInput => {
                    let back = s.status_before_prompt.take().unwrap_or(Status::Working);
                    let back = if matches!(back, Status::Idle | Status::Background) { back } else { Status::Working };
                    set_status(s, back, None);
                    // Timers were paused while the dialog was up.
                    if let Some((_, grace)) = s.awaiting_submit {
                        s.awaiting_submit = Some((Instant::now(), grace));
                    }
                    deliver_if_ready(s);
                }
                _ => {}
            }
            d.touch(&mut inner);
        }
    }
}

/// Move the dialog selection onto "Yes, I trust this folder" and confirm.
fn accept_trust(term: &TermHandle) {
    let Some(text) = term.text(false, false) else { return };
    let lines: Vec<&str> = text.lines().collect();
    let selected = lines.iter().position(|l| l.trim_start().starts_with('❯'));
    let yes = lines.iter().position(|l| l.contains("Yes, I trust") || l.contains("Yes, proceed"));
    let (Some(selected), Some(yes)) = (selected, yes) else { return };
    let code = if yes > selected { KeyCode::Down } else { KeyCode::Up };
    for _ in 0..yes.abs_diff(selected) {
        term.key(KeyInput::plain(code.clone()));
    }
    std::thread::sleep(Duration::from_millis(100));
    term.key(KeyInput::plain(KeyCode::Enter));
}

fn on_hook(d: &Shared, id: &str, event: &str, payload: Value) {
    if std::env::var_os("CLAUDEX_DEBUG").is_some() {
        eprintln!("[{}] hook {id} {event} {payload}", paths::now());
    }
    let last_message = (event == "Stop").then(|| hooks::last_assistant_message(&payload)).flatten();
    let mut inner = d.lock();
    let Some(s) = inner.sessions.get_mut(id) else { return };
    if !s.info.status.is_live() {
        return;
    }
    s.last_hook = Some(Instant::now());
    match event {
        "SessionStart" => {
            let source = payload.get("source").and_then(Value::as_str).unwrap_or("");
            if (s.info.status == Status::Starting && !s.initial_pending) || source == "clear" {
                s.awaiting_submit = None;
                set_status(s, Status::Idle, None);
            }
        }
        "UserPromptSubmit" => {
            s.awaiting_submit = None;
            s.initial_pending = false;
            set_status(s, Status::Working, None);
        }
        "PostToolUse" => {
            if s.screen_prompt.is_none() && s.info.status != Status::Working {
                set_status(s, Status::Working, None);
            }
        }
        "Notification" => {
            let kind = payload.get("notification_type").and_then(Value::as_str).unwrap_or("");
            let message = payload.get("message").and_then(Value::as_str).map(str::to_string);
            let is_permission = kind == "permission_prompt"
                || message.as_deref().is_some_and(|m| m.contains("permission"));
            if is_permission {
                if s.info.status != Status::NeedsInput {
                    s.status_before_prompt = Some(s.info.status);
                }
                set_status(s, Status::NeedsInput, message);
            } else if kind == "idle_prompt" && s.info.status == Status::Working && s.awaiting_submit.is_none() {
                set_status(s, resting_status(s), background_detail(&s.info.background));
            }
        }
        "Stop" => {
            s.awaiting_submit = None;
            s.initial_pending = false;
            s.info.turns += 1;
            if last_message.is_some() {
                s.info.last_message = last_message;
            }
            // Running background tasks will wake claude with a task
            // notification when they finish, so the session isn't idle yet.
            s.info.background = running_background_tasks(&payload);
            s.background_cleared = None;
            set_status(s, resting_status(s), background_detail(&s.info.background));
        }
        "SessionEnd" => {}
        _ => {}
    }
    if matches!(s.info.status, Status::Idle | Status::Background) {
        deliver_if_ready(s);
    }
    d.touch(&mut inner);
}

/// Background tasks the Stop hook reports as still running.
fn running_background_tasks(payload: &Value) -> Vec<BackgroundTask> {
    let Some(tasks) = payload.get("background_tasks").and_then(Value::as_array) else { return Vec::new() };
    let field = |t: &Value, k: &str| t.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    tasks
        .iter()
        .filter(|t| matches!(t.get("status").and_then(Value::as_str), Some("running" | "pending") | None))
        .map(|t| {
            let description = field(t, "description");
            BackgroundTask {
                id: field(t, "id"),
                kind: t.get("type").and_then(Value::as_str).unwrap_or("task").to_string(),
                description: if description.is_empty() { field(t, "command") } else { description },
            }
        })
        .collect()
}

/// e.g. "1 shell running: Run the test suite".
fn background_detail(tasks: &[BackgroundTask]) -> Option<String> {
    let first = tasks.first()?;
    let kind = if tasks.iter().all(|t| t.kind == first.kind) { first.kind.as_str() } else { "task" };
    let plural = if tasks.len() == 1 { "" } else { "s" };
    let mut detail = format!("{} {kind}{plural} running", tasks.len());
    if !first.description.is_empty() {
        detail.push_str(": ");
        detail.push_str(&first.description);
        if tasks.len() > 1 {
            detail.push_str(&format!(" (+{})", tasks.len() - 1));
        }
    }
    Some(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_background_tasks_from_stop_payload() {
        let payload = serde_json::json!({ "background_tasks": [
            { "id": "a", "type": "shell", "status": "running", "description": "Run tests", "command": "make test" },
            { "id": "b", "type": "shell", "status": "completed", "description": "Build" },
            { "id": "c", "type": "shell", "status": "running", "command": "sleep 5" },
        ]});
        let tasks = running_background_tasks(&payload);
        assert_eq!(tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(tasks[1].description, "sleep 5");
        assert_eq!(background_detail(&tasks).as_deref(), Some("2 shells running: Run tests (+1)"));
        assert!(running_background_tasks(&serde_json::json!({ "background_tasks": [] })).is_empty());
        assert!(running_background_tasks(&serde_json::json!({})).is_empty());
    }
}

// ---------------------------------------------------------------------------
// Live streaming for `claudex manage`

#[derive(Clone, Default)]
struct Watch {
    id: Option<String>,
    dirty: bool,
}

fn subscribe(d: &Shared, reader: BufReader<UnixStream>, mut writer: UnixStream) -> Result<()> {
    let watch = Arc::new(Mutex::new(Watch::default()));
    let alive = Arc::new(AtomicBool::new(true));

    {
        let d = d.clone();
        let watch = watch.clone();
        let alive = alive.clone();
        std::thread::spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(ev) = serde_json::from_str::<ClientEvent>(&line) else { continue };
                client_event(&d, &watch, ev);
            }
            alive.store(false, Ordering::SeqCst);
            d.changed.notify_all();
        });
    }

    let mut sent_version = 0u64;
    let mut sent_screen: Option<(String, u64)> = None;
    let frame = Duration::from_millis(33);
    while alive.load(Ordering::SeqCst) {
        let started = Instant::now();
        let (sessions, screen_job) = {
            let mut inner = d.lock();
            let w = watch.lock().unwrap().clone();
            let stale_screen = |inner: &Inner| match &w.id {
                Some(id) => inner
                    .sessions
                    .get(id)
                    .is_some_and(|s| w.dirty || sent_screen.as_ref() != Some(&(id.clone(), s.screen_gen))),
                None => false,
            };
            if inner.version == sent_version && !stale_screen(&inner) {
                inner = d.changed.wait_timeout(inner, Duration::from_secs(2)).unwrap_or_else(|e| e.into_inner()).0;
            }
            let sessions = (inner.version != sent_version).then(|| {
                sent_version = inner.version;
                inner.sessions.values().map(|s| s.info.clone()).collect::<Vec<_>>()
            });
            let w = watch.lock().unwrap().clone();
            let job = w.id.as_ref().and_then(|id| {
                let s = inner.sessions.get(id)?;
                let key = (id.clone(), s.screen_gen);
                (w.dirty || sent_screen.as_ref() != Some(&key)).then(|| (key, s.term.clone()))
            });
            (sessions, job)
        };
        if let Some(sessions) = sessions {
            write_line(&mut writer, &StreamEvent::Sessions { sessions })?;
        }
        if let Some(((id, generation), term)) = screen_job {
            watch.lock().unwrap().dirty = false;
            let screen = term.and_then(|t| t.snapshot()).unwrap_or_default();
            write_line(&mut writer, &StreamEvent::Screen { id: id.clone(), screen })?;
            sent_screen = Some((id, generation));
            // Cap the frame rate; output arriving meanwhile is coalesced.
            if let Some(rest) = frame.checked_sub(started.elapsed()) {
                std::thread::sleep(rest);
            }
        }
    }
    Ok(())
}

fn client_event(d: &Shared, watch: &Mutex<Watch>, ev: ClientEvent) {
    let term_of = |id: &str| d.lock().sessions.get(id).and_then(|s| s.term.clone());
    match ev {
        ClientEvent::Watch { id, cols, rows } => {
            if let Some(id) = &id {
                let mut inner = d.lock();
                if let Some(s) = inner.sessions.get_mut(id)
                    && cols > 0 && rows > 0 && s.size != (cols, rows) {
                        s.size = (cols, rows);
                        if let Some(p) = &s.proc {
                            let _ = p.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                        }
                        if let Some(t) = &s.term {
                            t.resize(cols, rows);
                        }
                    }
            }
            let mut w = watch.lock().unwrap();
            w.id = id;
            w.dirty = true;
            d.changed.notify_all();
        }
        ClientEvent::Key { id, key } => {
            if let Some(t) = term_of(&id) {
                t.key(key);
            }
        }
        ClientEvent::Paste { id, text } => {
            if let Some(t) = term_of(&id) {
                t.paste(text);
            }
        }
        ClientEvent::Scroll { id, delta, col, row } => {
            if let Some(t) = term_of(&id) {
                t.scroll(delta, col, row);
            }
        }
        ClientEvent::ScrollReset { id } => {
            if let Some(t) = term_of(&id) {
                t.scroll_reset();
            }
        }
    }
}
