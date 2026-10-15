use std::io::Read;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use claude_mux::protocol::{self, CreateRequest, Request, Response, SessionInfo, Status};
use claude_mux::{ago, client, hooks, manage, short_path};

#[derive(Parser)]
#[command(
    name = "claude-mux",
    version,
    about = "Run and manage many Claude Code sessions from one place",
    long_about = "Run and manage many Claude Code sessions from one place.\n\n\
Each session is an interactive `claude` running in the claude-muxd daemon, which \
must already be running. People, scripts and other Claude sessions can create sessions, send \
them work and wait for results; `claude-mux manage` shows every session live.",
    after_help = "Run `claude-mux --help` for the workflow, session states, exit codes and environment.",
    after_long_help = AFTER_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

const AFTER_HELP: &str = "\
TYPICAL WORKFLOW
  id=$(claude-mux create \"Fix the flaky test in tests/auth_test.go\" --name flaky-test -C ~/repo)
  claude-mux wait \"$id\"                       # blocks until the turn ends, prints claude's reply
  claude-mux send \"$id\" \"Also add a regression test\" --wait
  claude-mux wait a b c --any                  # first of several sessions to finish

  The context passed to `create` is the session's first prompt. The session
  shares none of the caller's context, so write it like a ticket: the goal,
  where to look, and how to tell when it's done.

SESSION STATES
  starting     claude is booting
  working      processing a prompt (queued messages count as work)
  needs-input  a permission prompt, trust dialog or other choice is on screen
  background   turn ended, but background shells/agents are still running;
               claude resumes by itself when they finish (`wait` keeps waiting)
  idle         turn finished, nothing running; waiting for the next message.
               After a few seconds idle, claude's process is stopped to save
               memory (asleep) and resumed automatically by the next
               `send` or `keys` (CLAUDE_MUX_SLEEP_AFTER=<secs>|never)
  exited       the claude process ended on its own
  stopped      killed or the daemon restarted; `claude-mux resume` continues it

WAIT EXIT CODES (wait, create --wait, send --wait)
  0 idle   2 needs input   3 exited/stopped   124 timeout
  4 background tasks still running (only with `wait --turn`)

WHEN A SESSION NEEDS INPUT
  `claude-mux peek <id>` prints the screen so you can read the dialog. Answer it
  with keys, e.g. `claude-mux keys <id> enter` (accept the highlighted option) or
  `claude-mux keys <id> down enter`, or leave it for a person in `claude-mux manage`.
  `create --trust` accepts the folder-trust dialog automatically.

NOTES
  Sessions keep running when the caller exits; `claude-mux kill` stops one
  (resumable) and `claude-mux rm` deletes it. Inside a managed session `claude-mux`
  is on PATH and CLAUDE_MUX_SESSION_ID holds the session's own id.

ENVIRONMENT
  CLAUDE_MUX_HOME        holds state.db, daemon.log, daemon.sock (default ~/.claude-mux)
  CLAUDE_MUX_SOCKET      daemon socket path (default $CLAUDE_MUX_HOME/daemon.sock)
  CLAUDE_MUX_CLAUDE_BIN  claude executable (default `claude` on PATH)";

#[derive(Subcommand)]
enum Command {
    /// Start a new Claude Code session; prints its id.
    ///
    /// CONTEXT becomes claude's first prompt. The command returns as soon as the
    /// session starts (unless --wait); use `claude-mux wait <id>` for the result.
    Create {
        /// Context / task for the session. Use '-' to read from stdin.
        context: Option<String>,
        /// Session id to use (default: a random short id).
        #[arg(long)]
        id: Option<String>,
        /// Display name shown in `manage`.
        #[arg(long, short)]
        name: Option<String>,
        /// Working directory for claude (default: current directory).
        #[arg(long, short = 'C')]
        cwd: Option<String>,
        /// Model to use (passed to claude --model).
        #[arg(long)]
        model: Option<String>,
        /// Permission mode (passed to claude --permission-mode).
        #[arg(long)]
        permission_mode: Option<String>,
        /// Skip all permission prompts (claude --dangerously-skip-permissions).
        #[arg(long)]
        yolo: bool,
        /// Automatically accept the "do you trust this folder?" dialog.
        #[arg(long)]
        trust: bool,
        /// Wait for the first turn to finish and print claude's reply.
        #[arg(long, short)]
        wait: bool,
        /// Timeout in seconds for --wait.
        #[arg(long)]
        timeout: Option<u64>,
        /// Extra arguments passed to claude verbatim (after `--`).
        #[arg(last = true)]
        claude_args: Vec<String>,
    },
    /// Send a message to a session (queued until claude is idle).
    ///
    /// Each queued message is typed in once the previous turn finishes, so it
    /// runs as its own turn. Slash commands (e.g. "/compact") work too.
    Send {
        /// `<message>` or `<id> <message>`. Use '-' as the message to read stdin.
        #[arg(required = true, num_args = 1..=2)]
        args: Vec<String>,
        /// Session id (default: the most recently created live session).
        #[arg(long)]
        id: Option<String>,
        /// Type the message right away, even if claude is busy.
        #[arg(long)]
        now: bool,
        /// Wait for the turn to finish and print claude's reply.
        #[arg(long, short)]
        wait: bool,
        /// Timeout in seconds for --wait.
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Wait until sessions finish their current work.
    ///
    /// Returns when each session is idle, needs input, or has exited, then
    /// prints claude's final reply to stdout (status notes go to stderr).
    /// A session whose turn ended with background shells/agents still running
    /// is not done: claude resumes when they finish, so `wait` keeps waiting
    /// (unless --turn).
    ///
    /// Exit codes: 0 idle, 2 needs input, 3 exited/stopped, 4 background
    /// tasks still running (--turn only), 124 timeout.
    Wait {
        /// Session ids to wait for.
        #[arg(required = true)]
        ids: Vec<String>,
        /// Return when any session settles instead of all of them.
        #[arg(long)]
        any: bool,
        /// Give up after this many seconds.
        #[arg(long)]
        timeout: Option<u64>,
        /// Return when the turn ends even if background tasks are still
        /// running (exit code 4), instead of waiting for claude to finish them.
        #[arg(long)]
        turn: bool,
        /// Don't print claude's final message.
        #[arg(long, short)]
        quiet: bool,
        /// Print the session records as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Interactive TUI to watch and drive every session.
    ///
    /// Session list: j/k move, 1-9 jump, enter focus terminal, n new, s send,
    /// x kill, r resume, d delete, R rename, pgup/pgdn scroll, ? help, q quit.
    /// Terminal focused: keys go to claude; ctrl-] returns to the list.
    Manage {
        /// Session to select on start.
        id: Option<String>,
    },
    /// Open `manage` focused on one session's terminal.
    Attach { id: String },
    /// List sessions.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one session's details.
    #[command(alias = "show")]
    Status {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Print a session's current screen.
    Peek {
        id: String,
        /// Keep colors and styles (ANSI escapes).
        #[arg(long)]
        ansi: bool,
    },
    /// Press keys in a session, e.g. `claude-mux keys <id> down enter`.
    ///
    /// Names: enter esc tab space backspace up down left right home end
    /// pgup pgdn delete f1-f12, single characters, with ctrl-/alt-/shift- prefixes.
    Keys {
        id: String,
        #[arg(required = true, allow_hyphen_values = true)]
        keys: Vec<String>,
    },
    /// Print the last reply claude gave in a session.
    Last { id: String },
    /// Stop a session's claude process (it can be resumed later).
    Kill { id: String },
    /// Restart a stopped/exited session, continuing its conversation.
    Resume { id: String },
    /// Remove a session (killing it if it is running).
    #[command(alias = "remove")]
    Rm { ids: Vec<String> },
    /// Rename a session (empty name clears it).
    Rename { id: String, name: String },
    /// Stop the daemon and every running session.
    Shutdown,
    /// Internal: Claude Code hook handler.
    #[command(hide = true)]
    Hook { event: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("claude-mux: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn read_arg(value: String) -> Result<String> {
    if value == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).context("reading stdin")?;
        Ok(buf)
    } else {
        Ok(value)
    }
}

fn get_session(id: &str) -> Result<SessionInfo> {
    match client::request(&Request::Get { id: id.into() })? {
        Response::Session { session } => Ok(session),
        other => bail!("unexpected response: {other:?}"),
    }
}

fn list_sessions() -> Result<Vec<SessionInfo>> {
    match client::request(&Request::List)? {
        Response::Sessions { sessions } => Ok(sessions),
        other => bail!("unexpected response: {other:?}"),
    }
}

fn run(command: Command) -> Result<ExitCode> {
    match command {
        Command::Create {
            context,
            id,
            name,
            cwd,
            model,
            permission_mode,
            yolo,
            trust,
            wait,
            timeout,
            claude_args: extra,
        } => {
            let context = context.map(read_arg).transpose()?.unwrap_or_default();
            let cwd = match cwd {
                Some(dir) => std::fs::canonicalize(&dir).with_context(|| format!("no such directory: {dir}"))?,
                None => std::env::current_dir()?,
            };
            let mut claude_args = Vec::new();
            if let Some(m) = model {
                claude_args.extend(["--model".to_string(), m]);
            }
            if let Some(p) = permission_mode {
                claude_args.extend(["--permission-mode".to_string(), p]);
            }
            if yolo {
                claude_args.push("--dangerously-skip-permissions".into());
            }
            claude_args.extend(extra);
            let req = Request::Create(CreateRequest {
                id,
                name,
                cwd: cwd.to_string_lossy().into_owned(),
                context,
                claude_args,
                trust,
            });
            let Response::Session { session } = client::request(&req)? else { bail!("unexpected response") };
            println!("{}", session.id);
            if wait {
                return wait_cmd(vec![session.id], false, timeout, false, false, false);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Send { mut args, id, now, wait, timeout } => {
            let message = read_arg(args.pop().expect("message"))?;
            let id = match (args.pop(), id) {
                (Some(a), Some(b)) if a != b => bail!("session id given twice ('{a}' and '{b}')"),
                (Some(id), _) | (None, Some(id)) => id,
                (None, None) => {
                    let latest = list_sessions()?
                        .into_iter()
                        .filter(|s| s.status.is_live())
                        .max_by_key(|s| s.created_at)
                        .context("no running sessions; pass --id or create one")?;
                    eprintln!("claude-mux: sending to {}", latest.id);
                    latest.id
                }
            };
            if message.trim().is_empty() {
                bail!("message is empty");
            }
            client::request(&Request::Send { id: id.clone(), message, now })?;
            if wait {
                return wait_cmd(vec![id], false, timeout, false, false, false);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Wait { ids, any, timeout, turn, quiet, json } => wait_cmd(ids, any, timeout, turn, quiet, json),
        Command::Manage { id } => manage::run(id, false).map(|_| ExitCode::SUCCESS),
        Command::Attach { id } => {
            get_session(&id)?;
            manage::run(Some(id), true).map(|_| ExitCode::SUCCESS)
        }
        Command::List { json } => {
            let sessions = list_sessions()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else if sessions.is_empty() {
                eprintln!("no sessions (create one with `claude-mux create \"...\"`)");
            } else {
                print_table(&sessions);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Status { id, json } => {
            let s = get_session(&id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                print_details(&s);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Peek { id, ansi } => {
            let Response::Text { text } = client::request(&Request::Peek { id, ansi })? else {
                bail!("unexpected response")
            };
            print!("{text}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Keys { id, keys } => {
            let keys = keys
                .iter()
                .map(|k| protocol::KeyInput::parse(k).with_context(|| format!("unknown key '{k}'")))
                .collect::<Result<Vec<_>>>()?;
            client::request(&Request::Keys { id, keys })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Last { id } => {
            let s = get_session(&id)?;
            match s.last_message {
                Some(m) => println!("{m}"),
                None => bail!("session '{id}' has not finished a turn yet"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Kill { id } => {
            client::request(&Request::Kill { id })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Resume { id } => {
            client::request(&Request::Resume { id })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Rm { ids } => {
            for id in ids {
                client::request(&Request::Remove { id })?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Rename { id, name } => {
            client::request(&Request::Rename { id, name: Some(name) })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Shutdown => {
            match client::connect_existing() {
                Some(mut conn) => {
                    conn.send(&Request::Shutdown)?;
                    let _ = conn.recv::<Response>();
                    // Wait for the daemon to finish stopping sessions.
                    for _ in 0..100 {
                        if client::connect_existing().is_none() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
                None => eprintln!("claude-mux: daemon is not running"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Hook { event } => {
            hooks::run_hook(&event);
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn wait_cmd(ids: Vec<String>, any: bool, timeout: Option<u64>, turn: bool, quiet: bool, json: bool) -> Result<ExitCode> {
    let req = Request::Wait { ids, any, timeout_ms: timeout.map(|s| s * 1000), turn };
    let Response::Wait { sessions, timed_out } = client::request(&req)? else { bail!("unexpected response") };
    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
    } else {
        let multi = sessions.len() > 1;
        for s in &sessions {
            let status = s.effective_status();
            if multi || status != Status::Idle {
                let detail = s.status_detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default();
                eprintln!("{}: {}{}", s.id, status.label(), detail);
            }
            if !quiet && status.is_turn_over()
                && let Some(m) = &s.last_message {
                    if multi {
                        println!("--- {} ---", s.id);
                    }
                    println!("{m}");
                }
        }
    }
    if timed_out {
        eprintln!("claude-mux: timed out");
        return Ok(ExitCode::from(124));
    }
    let statuses: Vec<Status> = sessions.iter().map(|s| s.effective_status()).collect();
    Ok(if statuses.iter().any(|s| matches!(s, Status::NeedsInput)) {
        ExitCode::from(2)
    } else if statuses.iter().any(|s| matches!(s, Status::Exited | Status::Stopped)) {
        ExitCode::from(3)
    } else if statuses.contains(&Status::Background) {
        ExitCode::from(4)
    } else {
        ExitCode::SUCCESS
    })
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.lines().next().unwrap_or("");
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

fn print_table(sessions: &[SessionInfo]) {
    println!("{:<14} {:<12} {:>5} {:<28} CWD", "ID", "STATUS", "AGE", "NAME");
    for s in sessions {
        println!(
            "{:<14} {:<12} {:>5} {:<28} {}",
            s.id,
            s.effective_status().label(),
            ago(s.created_at),
            truncate(s.display_name(), 28),
            short_path(&s.cwd)
        );
    }
}

fn print_details(s: &SessionInfo) {
    println!("id:        {}", s.id);
    if let Some(n) = &s.name {
        println!("name:      {n}");
    }
    if let Some(t) = &s.title {
        println!("title:     {t}");
    }
    let detail = s.status_detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default();
    let asleep = if s.asleep { " (asleep)" } else { "" };
    println!("status:    {}{asleep}{}", s.effective_status().label(), detail);
    println!("cwd:       {}", s.cwd);
    println!("created:   {} ago", ago(s.created_at));
    println!("turns:     {}", s.turns);
    if s.queued > 0 {
        println!("queued:    {}", s.queued);
    }
    println!("claude id: {}", s.claude_session_id);
    if !s.context.is_empty() {
        println!("context:   {}", truncate(&s.context, 100));
    }
    if let Some(m) = &s.last_message {
        println!("last reply:\n{m}");
    }
}
