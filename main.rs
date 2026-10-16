use std::io::Read;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use claude_mux::paths;
use claude_mux::protocol::{self, CreateRequest, KeyInput, Request, Response, SessionInfo, Status, short_id};
use claude_mux::term::{Dialog, parse_dialog};
use claude_mux::{ago, client, hooks, manage, short_path, upgrade};

mod select;
use select::Offer;

#[derive(Parser)]
#[command(
    name = "claude-mux",
    version,
    about = "Run many Claude Code sessions in the background and drive them from scripts, agents or a TUI",
    long_about = "Run many Claude Code sessions in the background and drive them from scripts, agents or a TUI.\n\n\
Each session is a real interactive `claude` in a terminal owned by the claude-mux daemon, which starts \
by itself when a command needs it. Sessions keep running after the command that started them exits.",
    after_help = "Run `claude-mux --help` for how sessions work, exit codes and the environment.\n\
Run `claude-mux <command> --help` for one command.",
    after_long_help = AFTER_LONG_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

const AFTER_LONG_HELP: &str = "\
QUICK START
  id=$(claude-mux create \"Fix the flaky test in tests/auth_test.go\" -C ~/repo)
  claude-mux wait -s \"$id\"                # blocks until claude is done; prints its reply
  claude-mux send -s \"$id\" \"Add a regression test\" --wait
  claude-mux manage                       # watch and type into every session

  A new session knows nothing about the caller. Write the prompt like a
  ticket: the goal, where to look, and how to tell when it's done.

CHOOSING A SESSION
  `create` prints the new session's id. Other commands pick a session with
  -s/--session, which takes that id or any unique prefix of it; `list`
  shows the first 8 characters. Without -s, a command uses the only session
  it applies to, such as the only running one for `send`. When several
  match, claude-mux asks which one in a terminal, and without a terminal
  fails and lists them, so scripts should always pass -s. Sessions are
  labelled by their description (`create -d`, `describe`), else by their
  prompt's first line.

SESSION STATES
  starting     claude is booting
  working      claude is busy, or messages are queued for it
  needs-input  a permission prompt or question is on screen (see ANSWERING)
  background   the turn ended but background shells/agents still run; claude
               carries on when they finish, so `wait` keeps waiting
  idle         done, ready for the next message. After 5 seconds idle the
               claude process sleeps to free memory; `send` and `keys` wake it
  exited       claude quit on its own; `resume` restarts it
  stopped      killed, or the daemon restarted; `resume` restarts it
  cleared      `/clear` moved claude on to a new session (see below)

/CLEAR STARTS A NEW SESSION
  A session is one Claude Code conversation and its id is claude's id for
  that conversation. `/clear` starts a new conversation, so the running
  claude moves to a new session with a new id. The old session becomes
  `cleared`: it keeps its last reply and can be resumed, but `send`, `keys`
  and `peek` on it fail and name the new id, and `wait` exits 3. Scripts
  wanting a fresh start should `create` a new session. Picking another
  conversation with `/resume` moves claude the same way.

ANSWERING
  When `wait` exits 2, a session is waiting on a permission prompt or a
  question. `claude-mux questions` prints them with numbered options.
  Record a choice for each question, submit, then `wait` again:

    claude-mux questions answer \"Auth method\" 2
    claude-mux questions answer \"Features\" 1,3      # a checkbox takes several
    claude-mux questions submit

  `questions chat MESSAGE` answers in words instead. For anything else on
  screen, use `peek` to see it and `keys` to press keys. `create --trust`
  accepts the folder-trust dialog; `create --yolo` skips permission prompts.

OUTPUT AND EXIT CODES
  Results go to stdout; notes and errors go to stderr. `list`, `status`,
  `wait` and `questions` take --json.

    0    success (wait: every session is idle)
    1    error
    2    wait: a session needs input
    3    wait: a session exited, was stopped or was cleared
    4    wait --turn: background tasks are still running
    64   invalid arguments
    124  wait: timed out
    130  cancelled at a prompt

  The wait codes also apply to `create --wait` and `send --wait`.

ENVIRONMENT
  CLAUDE_MUX_HOME         state directory: state.db, daemon.log, daemon.sock
                          (default ~/.claude-mux)
  CLAUDE_MUX_SOCKET       daemon socket (default $CLAUDE_MUX_HOME/daemon.sock)
  CLAUDE_MUX_CLAUDE_BIN   claude executable (default `claude` on PATH)
  CLAUDE_MUX_SLEEP_AFTER  seconds idle before a session sleeps, or `never`
                          (default 5)
  CLAUDE_MUX_SESSION_ID   set inside each session to the id it started with

  Inside a session `claude-mux` is on PATH, so claude can run sessions too.";

/// `-s/--session` for commands that act on one session.
#[derive(clap::Args)]
struct One {
    /// Session id or unique prefix. Without it, uses the only session this
    /// applies to, or asks which one
    #[arg(short, long = "session", value_name = "ID")]
    session: Option<String>,
}

/// `-s/--session` for commands that act on several sessions.
#[derive(clap::Args)]
struct Many {
    /// Session id or unique prefix; repeat it or separate with commas for
    /// several. Without it, uses the only session this applies to, or asks
    /// which ones
    #[arg(short, long = "session", value_name = "ID", value_delimiter = ',')]
    sessions: Vec<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Start a session; prints its id
    ///
    /// Starts an interactive `claude` in the background with PROMPT as its
    /// first message, prints the session's id on stdout, and returns at once.
    /// Use `wait` for the reply, or pass --wait.
    Create {
        /// First message for claude; '-' reads it from stdin. Without one,
        /// claude starts idle, waiting for `send`
        prompt: Option<String>,
        /// What the session is for; labels it in `list`, `manage` and the
        /// session picker (default: the prompt's first line)
        #[arg(long, short)]
        description: Option<String>,
        /// Directory to run claude in (default: the current directory)
        #[arg(long, short = 'C', value_name = "DIR")]
        cwd: Option<String>,
        /// Model, passed to `claude --model`
        #[arg(long)]
        model: Option<String>,
        /// Permission mode, passed to `claude --permission-mode`
        #[arg(long, value_name = "MODE")]
        permission_mode: Option<String>,
        /// Skip every permission prompt (`claude --dangerously-skip-permissions`)
        #[arg(long)]
        yolo: bool,
        /// Accept the "do you trust this folder?" dialog automatically
        #[arg(long)]
        trust: bool,
        /// Wait for the first turn like `wait` (same exit codes). stdout gets
        /// only the reply; the id goes to stderr
        #[arg(long, short)]
        wait: bool,
        /// With --wait, give up after this many seconds (exit code 124)
        #[arg(long, value_name = "SECS", requires = "wait")]
        timeout: Option<u64>,
        /// Further arguments for `claude`, after `--`
        #[arg(last = true, value_name = "CLAUDE_ARGS")]
        claude_args: Vec<String>,
    },
    /// Send a message; it waits in a queue until claude is ready
    ///
    /// Queued messages are typed into claude one at a time, each as its own
    /// turn once the previous one ends. A sleeping session wakes up for it.
    /// Slash commands work, but `/clear` moves claude to a new session with
    /// a new id (see `claude-mux --help`).
    Send {
        /// Text to send; '-' reads it from stdin
        message: String,
        #[command(flatten)]
        session: One,
        /// Type the message right away, even mid-turn, instead of queuing it
        #[arg(long)]
        now: bool,
        /// Wait for claude to finish, like `wait` (same output and exit codes)
        #[arg(long, short)]
        wait: bool,
        /// With --wait, give up after this many seconds (exit code 124)
        #[arg(long, value_name = "SECS", requires = "wait")]
        timeout: Option<u64>,
    },
    /// Block until sessions finish; prints claude's final reply
    ///
    /// Returns once every session is idle, needs input, exited, stopped or was
    /// cleared, then prints each one's last reply on stdout. When waiting on
    /// several sessions each reply is headed `--- <id> ---`, and a status line
    /// per session goes to stderr. A turn that leaves background tasks running
    /// isn't finished: claude resumes when they end, so this keeps waiting
    /// unless --turn.
    ///
    /// Exit codes: 0 all idle, 2 one needs input, 3 one exited, stopped or was
    /// cleared, 4 background tasks running (--turn), 124 timed out.
    Wait {
        #[command(flatten)]
        sessions: Many,
        /// Return as soon as any one of the sessions finishes
        #[arg(long)]
        any: bool,
        /// Give up after this many seconds (exit code 124)
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
        /// Return when the turn ends even if background tasks still run (exit code 4)
        #[arg(long)]
        turn: bool,
        /// Print no replies; only the exit code and status lines
        #[arg(long, short)]
        quiet: bool,
        /// Print the session records as JSON instead (fields as in `list --json`)
        #[arg(long)]
        json: bool,
    },
    /// Show and answer the questions sessions are waiting on
    ///
    /// Without a subcommand, prints every session that needs input with its
    /// questions and numbered options. Answering takes two steps: `answer`
    /// records a choice per question, and `submit` enters the recorded choices
    /// into claude. Choices are kept until submitted, and dropped if the
    /// question changes. -s limits all of this to one session.
    ///
    ///   claude-mux questions answer "Auth method" 2
    ///   claude-mux questions answer "Features" 1,3
    ///   claude-mux questions submit
    Questions {
        #[command(subcommand)]
        cmd: Option<QuestionsCmd>,
        /// Only this session's questions: id or unique prefix (default: every
        /// session's)
        #[arg(short, long, value_name = "ID", global = true)]
        session: Option<String>,
        /// Print the waiting sessions, their questions and recorded choices as JSON
        #[arg(long)]
        json: bool,
    },
    /// List sessions
    ///
    /// Prints one row per session: short id, state, age, description and
    /// directory.
    ///
    /// With --json, prints an array of session records. The main fields: id,
    /// description, prompt, cwd, status (`stopped` covers `cleared`),
    /// status_detail (e.g. the prompt being asked), continued_as (the session a
    /// `/clear` moved claude to), asleep, queued (messages waiting), turns,
    /// last_message (claude's last reply), created_at and updated_at (Unix
    /// seconds).
    #[command(alias = "ls")]
    List {
        /// Print the sessions as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show a session's details and last reply
    #[command(alias = "show")]
    Status {
        #[command(flatten)]
        session: One,
        /// Print the session record as JSON (fields as in `list --json`)
        #[arg(long)]
        json: bool,
    },
    /// Print claude's last reply in a session
    ///
    /// Fails if the session hasn't finished a turn yet.
    Last {
        #[command(flatten)]
        session: One,
    },
    /// Print what's on a session's screen
    ///
    /// Prints the terminal's visible text: useful to see a dialog before
    /// answering it with `keys`.
    Peek {
        #[command(flatten)]
        session: One,
        /// Keep colors and styles as ANSI escapes
        #[arg(long)]
        ansi: bool,
    },
    /// Press keys in a session, e.g. `keys -s 3f9a down enter`
    ///
    /// Presses each key in order, then pauses briefly so the screen and state
    /// have caught up before the command returns.
    ///
    /// Key names: enter, esc, tab, space, backspace, delete, up, down, left,
    /// right, home, end, pgup, pgdn, f1-f12, or any single character. Prefix
    /// ctrl-, alt-, shift- or cmd- to combine, as in ctrl-c.
    Keys {
        /// Keys by name, e.g. enter, esc, down, 1, ctrl-c (all names: --help)
        #[arg(required = true, allow_hyphen_values = true, value_name = "KEY")]
        keys: Vec<String>,
        #[command(flatten)]
        session: One,
    },
    /// Set or clear a session's description
    ///
    /// The description labels the session in `list`, `manage` and the session
    /// picker. Without one, the prompt's first line is used.
    Describe {
        /// New description
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        description: Option<String>,
        /// Remove the description
        #[arg(long)]
        clear: bool,
        #[command(flatten)]
        session: One,
    },
    /// Stop a session's claude process; `resume` restarts it
    ///
    /// Queued messages are dropped. The conversation is kept.
    Kill {
        #[command(flatten)]
        session: One,
    },
    /// Restart a stopped, exited or cleared session where it left off
    Resume {
        #[command(flatten)]
        session: One,
    },
    /// Delete sessions, stopping any that are running
    ///
    /// Removes them from claude-mux. Claude Code's own transcripts are kept.
    #[command(alias = "remove")]
    Rm {
        #[command(flatten)]
        sessions: Many,
    },
    /// Open the TUI to watch and drive every session
    ///
    /// Lists sessions beside the selected one's live terminal. In the list:
    /// j/k move, 1-9 jump, enter types into the terminal, n new, s send, x
    /// kill, r resume, d delete, R describe, pgup/pgdn scroll, ? all keys, q
    /// quit. While typing into a terminal, ctrl-] returns to the list.
    /// Quitting leaves every session running.
    Manage {
        /// Session to select first: id or unique prefix
        #[arg(short, long, value_name = "ID")]
        session: Option<String>,
    },
    /// Open the TUI typing into one session's terminal
    ///
    /// Same as `manage`, starting with the session's terminal focused. ctrl-]
    /// returns to the list.
    Attach {
        #[command(flatten)]
        session: One,
    },
    /// Stop the daemon and every running session
    ///
    /// Running sessions become `stopped`; `resume` continues them. The next
    /// command starts the daemon again.
    Shutdown,
    /// Install the latest release and restart the daemon
    ///
    /// Downloads the release from GitHub and replaces this binary. The daemon
    /// must match the CLI's version, so it is restarted, stopping sessions
    /// whose claude is running (resume them afterwards); sleeping sessions are
    /// unaffected. Asks first if any are running, unless --yes.
    Upgrade {
        /// Release to install, e.g. v0.2.0 (default: the latest)
        #[arg(long)]
        version: Option<String>,
        /// Reinstall even if this version is already installed
        #[arg(long)]
        force: bool,
        /// Don't ask before stopping running sessions
        #[arg(long, short)]
        yes: bool,
    },
    /// Run the daemon in the foreground, logging to stderr
    ///
    /// Not needed normally: any command starts the daemon in the background
    /// when it isn't running. Exits at once if a daemon already runs for this
    /// state directory.
    #[command(hide = true)]
    Daemon,
    /// Internal: Claude Code hook handler.
    #[command(hide = true)]
    Hook { event: String },
}

#[derive(Subcommand)]
enum QuestionsCmd {
    /// Record a choice for one question; `submit` enters it
    ///
    /// QUESTION is the question's header as `questions` prints it, or its
    /// full text when it has none (case doesn't matter). It is looked up
    /// across every waiting session and must match exactly one question.
    Answer {
        /// The question's header, or its text when it has no header
        question: String,
        /// Option number, from 1; a checkbox question takes several, as 1,3
        #[arg(value_name = "N[,N...]")]
        selection: String,
        /// Text to add to the choice, for questions that offer notes
        #[arg(long)]
        notes: Option<String>,
    },
    /// Enter every recorded choice and submit the dialogs
    Submit,
    /// Answer in words, via the dialog's "Chat about this" option
    ///
    /// Works when exactly one session is waiting on a question. Claude reads
    /// MESSAGE as your reply and carries on from there.
    Chat {
        /// What to tell claude; '-' reads it from stdin
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

/// The session `-s` names, or the one picked from those `offer` includes.
fn pick(session: One, offer: Offer, prompt: &str) -> Result<String> {
    select::one(&list_sessions()?, session.session.as_deref(), offer, prompt)
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
                Some(dir) => std::fs::canonicalize(&dir).map_err(|_| anyhow::anyhow!("no such directory: {dir}"))?,
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
            if wait {
                eprintln!("{}", session.id);
                return wait_cmd(vec![session.id], false, timeout, false, false, false);
            }
            println!("{}", session.id);
            Ok(ExitCode::SUCCESS)
        }
        Command::Send { message, session, now, wait, timeout } => {
            let message = read_arg(message)?;
            if message.trim().is_empty() {
                bail!("message is empty");
            }
            let id = pick(session, Offer::Running, "Send to which session?")?;
            client::request(&Request::Send { id: id.clone(), message, now })?;
            if wait {
                return wait_cmd(vec![id], false, timeout, false, false, false);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Wait { sessions, any, timeout, turn, quiet, json } => {
            let ids = select::many(&list_sessions()?, &sessions.sessions, Offer::Running, "Wait for which sessions?")?;
            wait_cmd(ids, any, timeout, turn, quiet, json)
        }
        Command::Manage { session } => {
            let id = match session {
                Some(q) => Some(select::one(&list_sessions()?, Some(&q), Offer::All, "Select which session?")?),
                None => None,
            };
            manage::run(id, false).map(|_| ExitCode::SUCCESS)
        }
        Command::Attach { session } => {
            let id = pick(session, Offer::Running, "Attach to which session?")?;
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
        Command::Questions { cmd, session, json } => questions_cmd(cmd, session, json),
        Command::Status { session, json } => {
            let id = pick(session, Offer::All, "Show which session?")?;
            let s = get_session(&id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                print_details(&s);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Peek { session, ansi } => {
            let id = pick(session, Offer::Running, "Peek at which session?")?;
            let Response::Text { text } = client::request(&Request::Peek { id, ansi })? else {
                bail!("unexpected response")
            };
            print!("{text}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Keys { keys, session } => {
            let keys = keys
                .iter()
                .map(|k| protocol::KeyInput::parse(k).with_context(|| format!("unknown key '{k}' (see `claude-mux keys --help`)")))
                .collect::<Result<Vec<_>>>()?;
            let id = pick(session, Offer::Running, "Press keys in which session?")?;
            client::request(&Request::Keys { id, keys })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Last { session } => {
            let id = pick(session, Offer::All, "Show the last reply of which session?")?;
            let s = get_session(&id)?;
            match s.last_message {
                Some(m) => println!("{m}"),
                None => bail!("session {} has not finished a turn yet", s.short_id()),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Kill { session } => {
            let id = pick(session, Offer::Running, "Kill which session?")?;
            client::request(&Request::Kill { id })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Resume { session } => {
            let id = pick(session, Offer::Resumable, "Resume which session?")?;
            client::request(&Request::Resume { id })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Rm { sessions } => {
            for id in select::many(&list_sessions()?, &sessions.sessions, Offer::All, "Remove which sessions?")? {
                client::request(&Request::Remove { id })?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Describe { description, clear, session } => {
            let id = pick(session, Offer::All, "Describe which session?")?;
            let description = if clear { None } else { description };
            client::request(&Request::Describe { id, description })?;
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
                eprintln!("{}: {}{}", s.short_id(), status.label(), detail);
            }
            if !quiet && status.is_turn_over()
                && let Some(m) = &s.last_message {
                    if multi {
                        println!("--- {} ---", s.short_id());
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
    println!("{:<8}  {:<12} {:>5}  {:<40} CWD", "ID", "STATUS", "AGE", "DESCRIPTION");
    for s in sessions {
        println!(
            "{:<8}  {:<12} {:>5}  {:<40} {}",
            s.short_id(),
            s.status_label(),
            ago(s.created_at),
            truncate(s.display_name(), 40),
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

fn questions_cmd(cmd: Option<QuestionsCmd>, session: Option<String>, json: bool) -> Result<ExitCode> {
    let mut sessions = list_sessions()?;
    if let Some(query) = session {
        let id = select::one(&sessions, Some(&query), Offer::All, "Questions from which session?")?;
        sessions.retain(|s| s.id == id);
    }
    let mut rows = waiting_rows(sessions);
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
                    names.push(format!("{}  {}", row.session.short_id(), question_name(question)));
                }
            }
            if names.is_empty() {
                bail!("no question named `{query}` is waiting");
            }
            bail!("no question named `{query}` is waiting\n{}", names.join("\n"));
        }
        _ => {
            let lines: Vec<String> = hits.iter().map(|h| format!("{}  {}", short_id(h.session_id), question_name(&h.ask.questions[h.index]))).collect();
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
    println!("{}  {}  {nums}", short_id(found.session_id), question_name(question));
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
            let lines: Vec<String> = dialogs.iter().map(|r| format!("{}  {}", r.session.short_id(), r.session.display_name())).collect();
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
    println!("{}  chat", row.session.short_id());
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
        println!("submitted {}", row.session.short_id());
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
    println!("{:<8}  {:>7}  {:<40} CWD", "ID", "WAITING", "DESCRIPTION");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let s = &row.session;
        println!(
            "{:<8}  {:>7}  {:<40} {}",
            s.short_id(),
            ago(s.updated_at),
            truncate(s.display_name(), 40),
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
    println!("id:          {}", s.id);
    if let Some(d) = &s.description {
        println!("description: {d}");
    }
    if let Some(t) = &s.title {
        println!("title:       {t}");
    }
    let detail = s.status_detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default();
    let asleep = if s.asleep { " (asleep)" } else { "" };
    println!("status:      {}{asleep}{}", s.effective_status().label(), detail);
    println!("cwd:         {}", s.cwd);
    println!("created:     {} ago", ago(s.created_at));
    println!("turns:       {}", s.turns);
    if s.queued > 0 {
        println!("queued:      {}", s.queued);
    }
    if let Some(next) = &s.continued_as {
        println!("continued:   as {next}");
    }
    if !s.prompt.is_empty() {
        println!("prompt:      {}", truncate(&s.prompt, 100));
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
