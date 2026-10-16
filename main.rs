use std::io::Read;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use claude_mux::paths;
use claude_mux::protocol::{self, CreateRequest, KeyInput, Request, Response, SessionInfo, Status};
use claude_mux::term::{Dialog, parse_dialog};
use claude_mux::{ago, client, hooks, manage, short_path, upgrade};

#[derive(Parser)]
#[command(
    name = "claude-mux",
    version,
    about = "Run and manage many Claude Code sessions from one place",
    long_about = "Run and manage many Claude Code sessions from one place.\n\n\
Each session is an interactive `claude` running in the claude-mux daemon, which \
starts on demand. People, scripts and other Claude sessions can create sessions, send \
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
  id=$(claude-mux create \"Fix the flaky test in tests/auth_test.go\" -d \"Flaky auth test\" -C ~/repo)
  claude-mux wait \"$id\"                       # blocks until the turn ends, prints claude's reply
  claude-mux send \"$id\" \"Also add a regression test\" --wait
  claude-mux wait \"$a\" \"$b\" --any            # first of several sessions to finish

  The prompt passed to `create` is the session's first message. The session
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
  cleared      `/clear` moved claude on to a new session (see below); JSON
               shows it as stopped with `continued_as` set

WAIT EXIT CODES (wait, create --wait, send --wait)
  0 idle   2 needs input   3 exited/stopped   124 timeout
  4 background tasks still running (only with `wait --turn`)
  Invalid arguments exit with 64 rather than 2, so they can't pass for
  needs input.

WHEN A SESSION NEEDS INPUT
  `claude-mux questions` lists every question a session is blocked on.
  Record each choice, then submit the dialog:

    claude-mux questions answer \"State\" 3 --notes \"Hi there\"
    claude-mux questions answer \"Title of some checkbox question\" 1,2,3
    claude-mux questions chat \"ask about the second option\"
    claude-mux questions submit

  The question is its header, or its text when it has no header. Option
  numbers start at 1. A checkbox takes several, separated by commas.
  `create --trust` accepts the folder-trust dialog automatically.

NOTES
  Sessions keep running when the caller exits; `claude-mux kill` stops one
  (resumable) and `claude-mux rm` deletes it. Inside a managed session `claude-mux`
  is on PATH and CLAUDE_MUX_SESSION_ID holds the id the session started with.

/CLEAR STARTS A NEW SESSION
  A session is one Claude Code conversation, and its id is claude's own id
  for it. `/clear` starts a new conversation, so the running claude moves
  to a new session with a new id. The old session is left `cleared`: it
  keeps its last reply and can still be resumed, but commands that need it
  running fail and name its successor:

    claude-mux: session 3f9a1c2e was cleared; it continues as 9b21e0c4

  `wait` on the old id returns exit code 3. To give a script a fresh start,
  `create` a new session instead of sending `/clear`. Picking another
  conversation with `/resume` moves claude to that conversation's session
  the same way. CLAUDE_MUX_SESSION_ID keeps the id the process started with.

ENVIRONMENT
  CLAUDE_MUX_HOME        holds state.db, daemon.log, daemon.sock (default ~/.claude-mux)
  CLAUDE_MUX_SOCKET      daemon socket path (default $CLAUDE_MUX_HOME/daemon.sock)
  CLAUDE_MUX_CLAUDE_BIN  claude executable (default `claude` on PATH)";

#[derive(Subcommand)]
enum Command {
    /// Start a new Claude Code session; prints its id.
    ///
    /// PROMPT is claude's first message. The command returns as soon as the
    /// session starts (unless --wait); use `claude-mux wait <id>` for the result.
    Create {
        /// The first message for claude. Use '-' to read from stdin.
        prompt: Option<String>,
        /// What the session is for, shown by `list` and `manage` (default: the
        /// prompt's first line).
        #[arg(long, short)]
        description: Option<String>,
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
    /// runs as its own turn. Slash commands (e.g. "/compact") work too, but
    /// "/clear" moves claude on to a new session with a new id: see
    /// `claude-mux --help`.
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
    /// x kill, r resume, d delete, R describe, pgup/pgdn scroll, ? help, q quit.
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
    /// List questions waiting for an answer, and answer them.
    ///
    /// With no subcommand, prints every waiting question. `answer` records a
    /// choice; `submit` sends the recorded choices to the session.
    Questions {
        #[command(subcommand)]
        cmd: Option<QuestionsCmd>,
        /// Print the questions as JSON.
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
    /// Set what a session is for, as shown by `list` and `manage` (empty clears it).
    Describe { id: String, description: String },
    /// Stop the daemon and every running session.
    Shutdown,
    /// Run the daemon in the foreground (logs on stderr).
    ///
    /// The daemon owns every Claude Code session, its PTY and its terminal
    /// emulator, and serves `claude-mux` over a Unix socket. Any command that
    /// needs it starts it automatically, detached, so there is no need to run
    /// it yourself or install it as a service. If a daemon is already running
    /// for this state directory, it exits immediately. Stop it with
    /// `claude-mux shutdown`.
    Daemon,
    /// Replace claude-mux with the latest GitHub release and restart the daemon.
    ///
    /// The daemon always runs the same version as the CLI, so upgrading
    /// restarts it. Sessions with a running claude process are stopped (resume
    /// them with `claude-mux resume`); asleep sessions wake as usual. If any
    /// are running, upgrade lists them and asks first (or pass --yes).
    Upgrade {
        /// Release to install, e.g. `v0.2.0` (default: the latest).
        #[arg(long)]
        version: Option<String>,
        /// Reinstall even if this version is already installed.
        #[arg(long)]
        force: bool,
        /// Don't ask before restarting the daemon while sessions are running.
        #[arg(long, short)]
        yes: bool,
    },
    /// Internal: Claude Code hook handler.
    #[command(hide = true)]
    Hook { event: String },
}

#[derive(Subcommand)]
enum QuestionsCmd {
    /// Record a choice for one waiting question. `submit` sends it.
    Answer {
        /// Question header, or the question text when it has no header.
        question: String,
        /// Option numbers starting at 1. Comma-separated for a checkbox (`1,2,3`).
        selection: String,
        /// Free-text note attached to this choice.
        #[arg(long)]
        notes: Option<String>,
    },
    /// Send the recorded choices and submit the dialog.
    Submit,
    /// Choose "Chat about this" and send a message.
    Chat {
        /// What to tell Claude. Use '-' to read from stdin.
        message: String,
    },
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            // Usage errors get their own code, apart from `wait`'s 2 (needs input).
            return if e.use_stderr() { ExitCode::from(64) } else { ExitCode::SUCCESS };
        }
    };
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
            prompt,
            description,
            cwd,
            model,
            permission_mode,
            yolo,
            trust,
            wait,
            timeout,
            claude_args: extra,
        } => {
            let prompt = prompt.map(read_arg).transpose()?.unwrap_or_default();
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
                description,
                cwd: cwd.to_string_lossy().into_owned(),
                prompt,
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
        Command::Questions { cmd, json } => questions_cmd(cmd, json),
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
        Command::Describe { id, description } => {
            client::request(&Request::Describe { id, description: Some(description) })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Shutdown => {
            if !client::shutdown()? {
                eprintln!("claude-mux: daemon is not running");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Daemon => {
            if let claude_mux::daemon::Started::AlreadyRunning = claude_mux::daemon::run()? {
                eprintln!("claude-mux: daemon already running for {}", paths::home().display());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Upgrade { version, force, yes } => upgrade_cmd(version, force, yes),
        Command::Hook { event } => {
            hooks::run_hook(&event);
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn upgrade_cmd(version: Option<String>, force: bool, yes: bool) -> Result<ExitCode> {
    let tag = upgrade::resolve_tag(version)?;
    let current = format!("v{}", client::VERSION);
    if tag == current && !force {
        println!("claude-mux {current} is already up to date");
        return Ok(ExitCode::SUCCESS);
    }

    let daemon = client::daemon_version();
    if let Some(daemon_version) = &daemon {
        // Ask the daemon directly: it may be a different version, which `client::request` refuses.
        let sessions = client::connect_existing().and_then(|mut conn| {
            conn.send(&Request::List).ok()?;
            match conn.recv::<Response>() {
                Ok(Some(Response::Sessions { sessions })) => Some(sessions),
                _ => None,
            }
        });
        let running: Option<Vec<SessionInfo>> =
            sessions.map(|s| s.into_iter().filter(|s| s.status.is_live() && !s.asleep).collect());
        let ask = match &running {
            Some(running) if running.is_empty() => false,
            Some(running) => {
                eprintln!("These sessions are running and will be stopped when the daemon restarts:\n");
                print_table(running);
                eprintln!("\nResume them afterwards with `claude-mux resume <id>`.");
                true
            }
            None => {
                eprintln!(
                    "The daemon (claude-mux {daemon_version}) is running and will be restarted, \
                     stopping any running sessions."
                );
                true
            }
        };
        if ask && !yes && !confirm("Upgrade and restart the daemon?")? {
            eprintln!("Not upgrading.");
            return Ok(ExitCode::FAILURE);
        }
    }

    let exe = upgrade::install(&tag)?;
    println!("Upgraded claude-mux {current} → {tag} at {}", exe.display());
    if daemon.is_some() {
        client::shutdown()?;
        drop(client::connect().context("starting the new daemon")?);
        println!("Restarted the daemon");
    }
    Ok(ExitCode::SUCCESS)
}

/// Ask a yes/no question on the terminal; refuses when there is none to ask on.
fn confirm(question: &str) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        bail!("not a terminal, so can't ask; pass --yes to proceed");
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
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

fn counts_as_waiting(session: &SessionInfo, ask: bool) -> bool {
    ask || session.effective_status() == Status::NeedsInput
}

/// Longest-waiting first.
fn order_waiting(sessions: &mut [SessionInfo]) {
    sessions.sort_by(|a, b| a.updated_at.cmp(&b.updated_at).then_with(|| a.id.cmp(&b.id)));
}

fn print_table(sessions: &[SessionInfo]) {
    println!("{:<36}  {:<12} {:>5}  {:<28} CWD", "ID", "STATUS", "AGE", "DESCRIPTION");
    for s in sessions {
        println!(
            "{:<36}  {:<12} {:>5}  {:<28} {}",
            s.id,
            s.status_label(),
            ago(s.created_at),
            truncate(s.display_name(), 28),
            short_path(&s.cwd)
        );
    }
}

#[derive(Serialize)]
struct WaitingRow {
    #[serde(flatten)]
    session: SessionInfo,
    /// Plan-mode questions from the transcript, or a screen dialog when that
    /// has none. Absent when neither could be read; `session.status_detail`
    /// is the fallback.
    dialog: Option<hooks::Ask>,
    /// A permission prompt read off the screen, rather than a transcript question.
    #[serde(skip_serializing_if = "is_false")]
    screen: bool,
    /// Choices recorded with `questions answer` and not yet submitted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recorded: Vec<Recorded>,
}

#[derive(Clone, Serialize)]
struct Recorded {
    question: String,
    options: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn peek_plain(id: &str) -> Option<String> {
    match client::request(&Request::Peek { id: id.to_string(), ansi: false }) {
        Ok(Response::Text { text }) => Some(text),
        _ => None,
    }
}

fn ask_from_screen(dialog: Dialog) -> hooks::Ask {
    hooks::Ask {
        questions: vec![hooks::AskQuestion {
            header: None,
            question: dialog.question,
            multi_select: false,
            options: dialog
                .options
                .into_iter()
                .map(|o| hooks::AskOption {
                    label: o.label,
                    description: None,
                    preview: None,
                    selected: o.selected,
                })
                .collect(),
        }],
    }
}

fn questions_cmd(cmd: Option<QuestionsCmd>, json: bool) -> Result<ExitCode> {
    let mut rows = waiting_rows(list_sessions()?);
    let mut store = load_answers()?;
    attach_recorded(&mut rows, &store);
    match cmd {
        None => {
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                eprintln!("no sessions waiting for input");
            } else {
                print_waiting(&rows);
            }
        }
        Some(QuestionsCmd::Answer { question, selection, notes }) => {
            record_answer(&mut store, &rows, &question, &selection, notes)?;
        }
        Some(QuestionsCmd::Chat { message }) => {
            let message = read_arg(message)?;
            chat_about(&mut store, &rows, &message)?;
        }
        Some(QuestionsCmd::Submit) => submit_answers(&mut store, &rows)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn question_name(q: &hooks::AskQuestion) -> &str {
    q.header.as_deref().filter(|h| !h.is_empty()).unwrap_or(&q.question)
}

fn question_matches(q: &hooks::AskQuestion, query: &str) -> bool {
    let query = query.trim();
    q.header.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(query)) || q.question.eq_ignore_ascii_case(query)
}

fn dialog_key(ask: &hooks::Ask) -> String {
    ask.questions
        .iter()
        .map(|q| {
            let opts: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
            format!("{}|{}|{}", q.multi_select, q.question, opts.join("|"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_selection(spec: &str) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            bail!("`{spec}` has an empty choice");
        }
        let n: usize = part.parse().with_context(|| format!("`{part}` is not an option number"))?;
        if n == 0 {
            bail!("option numbers start at 1");
        }
        if out.contains(&n) {
            bail!("option {n} is repeated");
        }
        out.push(n);
    }
    if out.is_empty() {
        bail!("choose at least one option");
    }
    Ok(out)
}

struct Found<'a> {
    session_id: &'a str,
    ask: &'a hooks::Ask,
    index: usize,
    screen: bool,
}

fn find_question<'a>(rows: &'a [WaitingRow], query: &str) -> Result<Found<'a>> {
    let mut hits: Vec<Found<'a>> = Vec::new();
    for row in rows {
        let Some(ask) = &row.dialog else { continue };
        for (index, question) in ask.questions.iter().enumerate() {
            if question_matches(question, query) {
                hits.push(Found { session_id: &row.session.id, ask, index, screen: row.screen });
            }
        }
    }
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => {
            let mut names = Vec::new();
            for row in rows {
                let Some(ask) = &row.dialog else { continue };
                for question in &ask.questions {
                    names.push(format!("{}  {}", row.session.id, question_name(question)));
                }
            }
            if names.is_empty() {
                bail!("no question named `{query}` is waiting");
            }
            bail!("no question named `{query}` is waiting\n{}", names.join("\n"));
        }
        _ => {
            let lines: Vec<String> = hits.iter().map(|h| format!("{}  {}", h.session_id, question_name(&h.ask.questions[h.index]))).collect();
            bail!("`{query}` matches more than one question\n{}", lines.join("\n"));
        }
    }
}

fn record_answer(store: &mut AnswerStore, rows: &[WaitingRow], query: &str, selection: &str, notes: Option<String>) -> Result<()> {
    let found = find_question(rows, query)?;
    let question = &found.ask.questions[found.index];
    let options = parse_selection(selection)?;
    if let Some(n) = options.iter().find(|n| **n > question.options.len() || !question.multi_select && **n > 9) {
        bail!("option {n} is not on `{}`", question_name(question));
    }
    if question.multi_select {
        if notes.as_ref().is_some_and(|n| !n.is_empty()) {
            bail!("`{}` is a checkbox and takes no notes", question_name(question));
        }
    } else if options.len() != 1 {
        bail!("`{}` takes one option", question_name(question));
    }
    if found.screen && notes.as_ref().is_some_and(|n| !n.is_empty()) {
        bail!("a permission prompt takes no notes");
    }
    let notes = notes.filter(|n| !n.is_empty());
    let key = dialog_key(found.ask);
    let staged = store.sessions.entry(found.session_id.to_string()).or_default();
    if staged.dialog != key {
        staged.dialog = key;
        staged.answers.clear();
    }
    staged.answers.retain(|a| a.index != found.index);
    staged.answers.push(StagedPick { index: found.index, options: options.clone(), notes: notes.clone() });
    staged.answers.sort_by_key(|a| a.index);
    save_answers(store)?;
    let nums = options.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
    println!("{}  {}  {nums}", found.session_id, question_name(question));
    if let Some(notes) = &notes {
        println!("  notes: {notes}");
    }
    Ok(())
}

/// "Chat about this" sits after the options and the Other row, so its number
/// is two past the last option. Choosing it leaves the dialog; the message
/// is queued and typed once Claude is back at its prompt.
fn chat_option(question: &hooks::AskQuestion) -> Result<usize> {
    let n = question.options.len() + 2;
    if n > 9 {
        bail!("Chat about this is option {n}, past the number keys");
    }
    Ok(n)
}

fn sole_dialog(rows: &[WaitingRow]) -> Result<&WaitingRow> {
    let dialogs: Vec<&WaitingRow> = rows.iter().filter(|r| r.dialog.is_some()).collect();
    match dialogs.len() {
        1 => Ok(dialogs[0]),
        0 => bail!("nothing is waiting"),
        _ => {
            let lines: Vec<String> = dialogs.iter().map(|r| format!("{}  {}", r.session.id, r.session.display_name())).collect();
            bail!("more than one session is waiting\n{}", lines.join("\n"));
        }
    }
}

fn chat_about(store: &mut AnswerStore, rows: &[WaitingRow], message: &str) -> Result<()> {
    let message = message.trim();
    if message.is_empty() {
        bail!("chat message is empty");
    }
    let row = sole_dialog(rows)?;
    if row.screen {
        bail!("a permission prompt has no Chat about this");
    }
    let question = row.dialog.as_ref().and_then(|ask| ask.questions.first()).context("no question is waiting")?;
    let n = chat_option(question)?;
    press(&row.session.id, &[n.to_string()])?;
    client::request(&Request::Send { id: row.session.id.clone(), message: message.to_string(), now: false })?;
    store.sessions.remove(&row.session.id);
    save_answers(store)?;
    println!("{}  chat", row.session.id);
    Ok(())
}

fn submit_answers(store: &mut AnswerStore, rows: &[WaitingRow]) -> Result<()> {
    let mut submitted = 0;
    for row in rows {
        let Some(ask) = &row.dialog else { continue };
        let picks = {
            let Some(staged) = store.sessions.get(&row.session.id) else { continue };
            if staged.dialog != dialog_key(ask) || staged.answers.is_empty() {
                continue;
            }
            staged.answers.clone()
        };
        let notes_ui = !row.screen && peek_plain(&row.session.id).is_some_and(|s| s.to_ascii_lowercase().contains("add notes"));
        let steps = submit_steps(ask, &picks, row.screen, notes_ui)?;
        for step in &steps {
            press(&row.session.id, step)?;
        }
        store.sessions.remove(&row.session.id);
        save_answers(store)?;
        println!("submitted {}", row.session.id);
        submitted += 1;
    }
    if submitted == 0 {
        bail!("nothing to submit; record choices with `claude-mux questions answer <question> <n>`");
    }
    Ok(())
}

fn press(id: &str, keys: &[String]) -> Result<()> {
    let mut parsed = Vec::with_capacity(keys.len());
    for key in keys {
        parsed.push(KeyInput::parse(key).with_context(|| format!("key `{key}`"))?);
    }
    client::request(&Request::Keys { id: id.to_string(), keys: parsed })?;
    Ok(())
}

/// Keystrokes that enter `picks` and submit the dialog, starting from the
/// first question. Each inner vec is one `keys` call: the call waits for
/// claude to redraw, which is what lets the next question (and the review
/// screen) exist before the following keys.
///
/// A plain question takes its option's number, which selects it. One such
/// question submits on that selection. A checkbox toggles each chosen number
/// (enter would toggle the highlighted row again), then moves down past the
/// options and the Other row onto Submit and confirms. A preview question
/// whose screen says "add notes" focuses the option, types `n` and the note,
/// leaves the field, then confirms. Anything that does not submit on its own
/// stops on "Submit answers", which is option 1.
fn submit_steps(ask: &hooks::Ask, picks: &[StagedPick], screen: bool, notes_ui: bool) -> Result<Vec<Vec<String>>> {
    if picks.is_empty() {
        bail!("nothing to submit");
    }
    if screen {
        let question = ask.questions.first().context("permission prompt has no question")?;
        let pick = picks.iter().find(|p| p.index == 0).context("permission prompt has no recorded choice")?;
        let target = pick.options[0] - 1;
        let selected = question.options.iter().position(|o| o.selected).unwrap_or(0);
        let mut keys = Vec::new();
        let delta = target as isize - selected as isize;
        let motion = if delta > 0 { "down" } else { "up" };
        for _ in 0..delta.unsigned_abs() {
            keys.push(motion.to_string());
        }
        keys.push("enter".to_string());
        return Ok(vec![keys]);
    }
    let mut steps = Vec::new();
    for (index, question) in ask.questions.iter().enumerate() {
        let Some(pick) = picks.iter().find(|p| p.index == index) else {
            steps.push(vec![if question.multi_select { "enter" } else { "tab" }.to_string()]);
            continue;
        };
        if let Some(n) = pick.options.iter().copied().find(|n| *n == 0 || *n > question.options.len() || !question.multi_select && *n > 9) {
            bail!("option {n} is not on `{}`", question_name(question));
        }
        if question.multi_select {
            if pick.notes.is_some() {
                bail!("`{}` is a checkbox and takes no notes", question_name(question));
            }
            let mut keys = Vec::new();
            for n in &pick.options {
                if *n > 9 {
                    bail!("option {n} is not on `{}`", question_name(question));
                }
                keys.push(n.to_string());
            }
            // Focus starts on row 1. Down once per real option and once more
            // for the Other row lands on Submit; enter confirms it.
            for _ in 0..question.options.len() + 1 {
                keys.push("down".to_string());
            }
            keys.push("enter".to_string());
            steps.push(keys);
            continue;
        }
        if pick.options.len() != 1 {
            bail!("`{}` takes one option", question_name(question));
        }
        let n = pick.options[0];
        let notes = pick.notes.as_deref().filter(|s| !s.is_empty());
        let field = notes_ui && question.options.iter().any(|o| o.preview.is_some());
        if notes.is_some() && !field {
            bail!("`{}` has no notes field", question_name(question));
        }
        if field {
            let mut keys = Vec::new();
            if n != 1 {
                keys.push(n.to_string());
            }
            if let Some(notes) = notes {
                keys.push("n".to_string());
                keys.extend(note_keys(notes)?);
                steps.push(keys);
                steps.push(vec!["esc".to_string()]);
                steps.push(vec!["enter".to_string()]);
            } else {
                keys.push("enter".to_string());
                steps.push(keys);
            }
        } else {
            steps.push(vec![n.to_string()]);
        }
    }
    let submits_itself = ask.questions.len() == 1 && !ask.questions[0].multi_select;
    if !submits_itself {
        steps.push(vec!["1".to_string()]);
    }
    Ok(steps)
}

fn note_keys(notes: &str) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for c in notes.chars() {
        if c == ' ' {
            keys.push("space".to_string());
        } else if c.is_control() {
            bail!("notes contain a control character");
        } else {
            keys.push(c.to_string());
        }
    }
    Ok(keys)
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct AnswerStore {
    sessions: std::collections::BTreeMap<String, StagedDialog>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct StagedDialog {
    dialog: String,
    answers: Vec<StagedPick>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StagedPick {
    index: usize,
    options: Vec<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
}

fn answers_path() -> std::path::PathBuf {
    paths::home().join("waiting.json")
}

fn load_answers() -> Result<AnswerStore> {
    let path = answers_path();
    if !path.exists() {
        return Ok(AnswerStore::default());
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn save_answers(store: &AnswerStore) -> Result<()> {
    let path = answers_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(store)?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn attach_recorded(rows: &mut [WaitingRow], store: &AnswerStore) {
    for row in rows {
        let Some(ask) = &row.dialog else { continue };
        let Some(staged) = store.sessions.get(&row.session.id) else { continue };
        if staged.dialog != dialog_key(ask) {
            continue;
        }
        row.recorded = staged
            .answers
            .iter()
            .filter_map(|pick| {
                let question = ask.questions.get(pick.index)?;
                Some(Recorded { question: question_name(question).to_string(), options: pick.options.clone(), notes: pick.notes.clone() })
            })
            .collect();
    }
}

/// Transcript first: a plan-mode question is an unanswered `AskUserQuestion`,
/// which does not go through the permission-dialog screen parser. A session
/// counts even when its status is still `working`. Other dialogs fall back
/// to the live screen.
fn waiting_rows(mut sessions: Vec<SessionInfo>) -> Vec<WaitingRow> {
    order_waiting(&mut sessions);
    let mut rows = Vec::new();
    for session in sessions {
        if session.status == Status::Exited {
            continue;
        }
        let mut ask = hooks::pending_ask_for(&session.cwd, &session.id);
        let mut from_screen = false;
        if !counts_as_waiting(&session, ask.is_some()) {
            continue;
        }
        if ask.is_none() && session.effective_status() == Status::NeedsInput {
            ask = peek_plain(&session.id).as_deref().and_then(parse_dialog).map(ask_from_screen);
            from_screen = ask.is_some();
        }
        rows.push(WaitingRow { session, dialog: ask, screen: from_screen, recorded: Vec::new() });
    }
    rows
}

fn print_waiting(rows: &[WaitingRow]) {
    println!("{:<36}  {:>7}  {:<28} CWD", "ID", "WAITING", "DESCRIPTION");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let s = &row.session;
        println!(
            "{:<36}  {:>7}  {:<28} {}",
            s.id,
            ago(s.updated_at),
            truncate(s.display_name(), 28),
            short_path(&s.cwd)
        );
        if let Some(ask) = &row.dialog {
            for (q, question) in ask.questions.iter().enumerate() {
                if q > 0 {
                    println!();
                }
                if let Some(header) = &question.header {
                    println!("  {header}");
                }
                if question.multi_select {
                    println!("  {} (choose any)", question.question);
                } else {
                    println!("  {}", question.question);
                }
                let marked = question.options.iter().any(|o| o.selected);
                for (n, opt) in question.options.iter().enumerate() {
                    if marked && opt.selected {
                        println!("    ❯ {}", opt.label);
                    } else if marked {
                        println!("      {}", opt.label);
                    } else {
                        println!("    {}  {}", n + 1, opt.label);
                    }
                    if let Some(description) = &opt.description {
                        for line in description.lines() {
                            println!("        {line}");
                        }
                    }
                }
                if let Some(pick) = row.recorded.iter().find(|r| r.question == question_name(question)) {
                    let nums = pick.options.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
                    println!("    recorded: {nums}");
                    if let Some(notes) = &pick.notes {
                        println!("    notes: {notes}");
                    }
                }
            }
        } else if let Some(detail) = s.status_detail.as_deref().and_then(|d| d.lines().next())
            && !detail.is_empty()
        {
            println!("  {detail}");
        }
    }
    println!();
    println!("claude-mux questions answer <question> <n> [--notes TEXT]");
    println!("claude-mux questions chat <message>");
    println!("claude-mux questions submit");
}

fn print_details(s: &SessionInfo) {
    println!("id:        {}", s.id);
    if let Some(d) = &s.description {
        println!("description: {d}");
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
    if let Some(next) = &s.continued_as {
        println!("continued: as {next}");
    }
    if !s.prompt.is_empty() {
        println!("prompt:    {}", truncate(&s.prompt, 100));
    }
    if let Some(m) = &s.last_message {
        println!("last reply:\n{m}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, status: Status, updated_at: u64) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            description: None,
            cwd: "/tmp".into(),
            prompt: String::new(),
            status,
            status_detail: None,
            title: None,
            created_at: 0,
            updated_at,
            exit_code: None,
            queued: 0,
            turns: 0,
            last_message: None,
            claude_args: vec![],
            auto_trust: false,
            background: vec![],
            asleep: false,
            continued_as: None,
        }
    }

    #[test]
    fn waiting_includes_plan_questions_and_prompts_longest_first() {
        let mut idle_queued = session("idle", Status::Idle, 1);
        idle_queued.queued = 2;
        let sessions = vec![
            session("later", Status::NeedsInput, 50),
            session("work", Status::Working, 10),
            session("early", Status::NeedsInput, 5),
            idle_queued,
            session("done", Status::Idle, 3),
            session("plan", Status::Working, 4),
        ];
        let has_ask = |s: &SessionInfo| s.id == "plan";
        let mut waiting: Vec<_> = sessions.into_iter().filter(|s| counts_as_waiting(s, has_ask(s))).collect();
        order_waiting(&mut waiting);
        let ids: Vec<_> = waiting.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["plan", "early", "later"]);
    }

    fn opt(label: &str, preview: bool, selected: bool) -> hooks::AskOption {
        hooks::AskOption {
            label: label.into(),
            description: None,
            preview: preview.then(|| "body".into()),
            selected,
        }
    }

    fn question(header: &str, multi: bool, options: Vec<hooks::AskOption>) -> hooks::AskQuestion {
        hooks::AskQuestion { header: Some(header.into()), question: format!("q {header}"), multi_select: multi, options }
    }

    fn pick(index: usize, options: Vec<usize>, notes: Option<&str>) -> StagedPick {
        StagedPick { index, options, notes: notes.map(str::to_string) }
    }

    #[test]
    fn selection_parses_one_or_many() {
        assert_eq!(parse_selection("3").unwrap(), vec![3]);
        assert_eq!(parse_selection("1,2,3").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_selection("1, 2").unwrap(), vec![1, 2]);
        assert!(parse_selection("0").is_err());
        assert!(parse_selection("1,1").is_err());
    }

    #[test]
    fn one_question_submits_on_its_number() {
        let ask = hooks::Ask { questions: vec![question("State", false, vec![opt("a", false, false), opt("b", false, false), opt("c", false, false)])] };
        let steps = submit_steps(&ask, &[pick(0, vec![3], None)], false, false).unwrap();
        assert_eq!(steps, vec![vec!["3"]]);
    }

    #[test]
    fn several_questions_confirm_after_each_choice() {
        let ask = hooks::Ask {
            questions: vec![
                question("State", false, vec![opt("a", false, false), opt("b", false, false)]),
                question("Layout", false, vec![opt("a", false, false), opt("b", false, false)]),
            ],
        };
        let steps = submit_steps(&ask, &[pick(0, vec![2], None), pick(1, vec![1], None)], false, false).unwrap();
        assert_eq!(steps, vec![vec!["2"], vec!["1"], vec!["1"]]);
    }

    #[test]
    fn checkbox_toggles_then_advances_and_confirms() {
        let ask = hooks::Ask { questions: vec![question("Boxes", true, vec![opt("a", false, false), opt("b", false, false), opt("c", false, false)])] };
        let steps = submit_steps(&ask, &[pick(0, vec![1, 2, 3], None)], false, false).unwrap();
        assert_eq!(
            steps,
            vec![vec!["1", "2", "3", "down", "down", "down", "down", "enter"], vec!["1"]]
        );
    }

    #[test]
    fn notes_are_typed_before_the_choice_is_confirmed() {
        let ask = hooks::Ask { questions: vec![question("State", false, vec![opt("a", true, false), opt("b", true, false), opt("c", true, false)])] };
        let steps = submit_steps(&ask, &[pick(0, vec![3], Some("Hi there"))], false, true).unwrap();
        assert_eq!(
            steps,
            vec![vec!["3", "n", "H", "i", "space", "t", "h", "e", "r", "e"], vec!["esc"], vec!["enter"]]
        );
    }

    #[test]
    fn notes_require_the_notes_field() {
        let ask = hooks::Ask { questions: vec![question("State", false, vec![opt("a", false, false)])] };
        assert!(submit_steps(&ask, &[pick(0, vec![1], Some("Hi"))], false, false).is_err());
    }

    #[test]
    fn chat_about_this_is_the_option_after_other() {
        let state = question("State", false, vec![opt("a", false, false), opt("b", false, false), opt("c", false, false)]);
        assert_eq!(chat_option(&state).unwrap(), 5);
        let boxes = question("Boxes", true, vec![opt("a", false, false), opt("b", false, false)]);
        assert_eq!(chat_option(&boxes).unwrap(), 4);
    }

    #[test]
    fn screen_prompt_moves_from_the_highlight() {
        let ask = hooks::Ask { questions: vec![question("Trust", false, vec![opt("1. Yes", false, false), opt("2. No", false, true)])] };
        let steps = submit_steps(&ask, &[pick(0, vec![1], None)], true, false).unwrap();
        assert_eq!(steps, vec![vec!["up", "enter"]]);
    }
}
