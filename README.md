# claude-mux

One place to run and manage all your Claude Code sessions. Each session is a
real interactive `claude` running in a pseudo-terminal and emulated with
[`alacritty_terminal`](https://crates.io/crates/alacritty_terminal). That means people, scripts
and other Claude sessions can all create sessions, send them work and wait
for results, while you watch and steer them from one TUI.

```sh
claude-mux create "Fix the login redirect bug in web/auth" --name login-bug   # → prints an id, e.g. 3f9a1c
claude-mux send 3f9a1c "Also add a regression test"                           # queued until claude is idle
claude-mux wait 3f9a1c                                                        # blocks until done, prints the reply
claude-mux manage                                                             # the TUI
```

## Install

Requires Rust and Claude Code on `PATH`.

```sh
cargo install --path . --locked
```

There's nothing else to set up: the first `claude-mux` command that needs the
daemon starts `claude-muxd` in the background.

### Nix

The flake provides `packages.<system>.default` (both binaries), an overlay
(`overlays.default`, adding `pkgs.claude-mux`) and a `devShells.default`.

```sh
nix run github:Vehmloewff/claude-mux -- list
nix profile install github:Vehmloewff/claude-mux
```

No service module is needed, since `claude-muxd` starts on demand.

Everything an agent needs to drive claude-mux is in `claude-mux --help`.

## Commands

| Command | |
|---|---|
| `claude-mux create [CONTEXT] [--id ID] [--name N] [-C DIR] [--model M] [--permission-mode P] [--yolo] [--trust] [--wait] [-- ARGS…]` | Start a session. CONTEXT becomes the first prompt (`-` reads stdin). Prints the id. |
| `claude-mux send [ID] MESSAGE [--now] [--wait]` | Send a message. It's queued until claude is idle, unless you pass `--now`. With no ID it goes to the newest running session. |
| `claude-mux wait ID… [--any] [--timeout S] [-q] [--json]` | Block until the sessions settle and print the final reply. |
| `claude-mux manage [ID]` / `claude-mux attach ID` | Open the TUI, optionally focused on one session. |
| `claude-mux list` · `status ID` · `peek ID [--ansi]` · `last ID` | Inspect sessions. |
| `claude-mux keys ID KEY…` | Press keys, e.g. `claude-mux keys 3f9a1c down enter` to answer a dialog. |
| `claude-mux kill ID` · `resume ID` · `rm ID…` · `rename ID NAME` | Manage the lifecycle. |
| `claude-mux shutdown` | Stop the daemon and all sessions. |

`wait` exits with `0` when the sessions are idle, `2` when one needs input
(a permission or trust dialog), `3` when one exited or stopped, and `124` on
timeout. `wait --turn` also returns when a turn ends with background tasks
still running, with exit code `4`.

### Session states

| | State | Meaning |
|---|---|---|
| ◌ | starting | claude is booting |
| ⠋ | working | processing a prompt (queued messages count as work) |
| ◆ | needs-input | a permission prompt, trust dialog or similar is waiting for a person |
| ◷ | background | the turn ended, but background shells/agents are still running; claude resumes when they finish, so `wait` keeps waiting |
| ● | idle | the turn is finished, nothing is running, and claude is waiting for the next message |
| ○ | idle (asleep) | idle, with claude's process stopped to save memory. The next `send`, `keys` or keystroke in the TUI resumes it (`claude --resume`) and delivers the input |
| ✕ | exited | the claude process ended on its own |
| ◼ | stopped | killed or the daemon restarted; `resume` continues the conversation |

## `claude-mux manage`

The left sidebar lists every session with its live status, task title,
directory and age. The right pane shows the selected session's terminal.

- **Session list:** `j`/`k` or arrows to move, `1`–`9` to jump, `enter` to
  focus the terminal, `n` new session, `s` send a message, `x` kill, `r`
  resume, `d` delete, `R` rename, `PgUp`/`PgDn` or the mouse wheel to scroll,
  `?` help, `q` quit. Quitting doesn't stop any sessions.
- **Terminal focused:** every key goes to claude, encoded in the daemon
  (kitty keyboard protocol and bracketed paste included). `ctrl-]` returns to the list.

## How it works

- **Daemon.** `claude-muxd` is a separate binary that is never run as a
  service. Any `claude-mux` command that needs it (everything except
  `shutdown` and the internal hook) starts it if the socket doesn't answer.
  It's double-forked into its own session, so it isn't a child of the
  command that started it, and its output goes to `~/.claude-mux/daemon.log`.
  A lock file (`~/.claude-mux/daemon.lock`) guarantees one daemon per state
  directory even when several commands start it at once. It stays up until
  `claude-mux shutdown` (or `SIGTERM`), which stops the sessions and records
  them as `stopped` so they can be resumed. Run `claude-muxd` by hand to keep
  it in the foreground with logs on stderr. It listens
  on `~/.claude-mux/daemon.sock`. It owns every claude process and its PTY.
  Session records are kept in a Turso database at `~/.claude-mux/state.db`, so
  sessions survive a daemon restart as `stopped` and can be resumed. Its
  schema is a set of migrations embedded in the binary (`migrations/`), which
  the daemon applies on startup and records in `_applied_migrations`. Claude Code's own session
  id is fixed with `--session-id`, which lets `resume` use `claude --resume`.
- **Terminal emulation.** All PTY output is fed into an
  `alacritty_terminal` `Term`, one per session, each on its own thread. That
  terminal answers claude's queries, encodes keys, mouse and
  paste, and produces render snapshots. The TUI receives those snapshots as
  styled spans.
- **Status.** Every claude is started with `--settings` hooks
  (`SessionStart`, `UserPromptSubmit`, `PostToolUse`, `Notification`,
  `Stop`) that call `claude-mux hook`, which reports back to the daemon. That
  makes "working / done" exact, and `Stop` also captures the final reply.
  The `Stop` payload lists background tasks that are still running. If any
  are, the session is `background` rather than `idle` until claude is woken
  by their task notification (a fresh `UserPromptSubmit`) and finishes. A
  footer scan (`· 1 shell ·`) covers tasks that end without waking claude.
  Blocking dialogs such as the trust prompt don't fire hooks, so the daemon
  also scans the emulated screen for them.
- **Sending.** Messages are pasted into claude as a bracketed paste followed
  by Enter, once the session is idle, so each one becomes its own turn.
- **Sleeping.** A claude process uses a lot of memory, so once a session has
  been idle for 5 seconds (no background tasks, dialogs or queued messages),
  the daemon stops its process and marks it `asleep`. The
  next message or keystroke restarts it with `claude --resume` and delivers the
  input once claude is back at its prompt. Sessions with no completed turn
  yet stay awake, since there's nothing to resume. Keys typed without
  submitting a prompt keep a session awake for 10 minutes, so drafts in
  claude's input box aren't lost.

Environment variables: `CLAUDE_MUX_HOME` sets the directory holding `state.db`, the log and the socket,
`CLAUDE_MUX_SOCKET` sets the socket path, `CLAUDE_MUX_CLAUDE_BIN` sets the
claude binary, `CLAUDE_MUX_SLEEP_AFTER` sets the idle seconds before a session sleeps (`never` turns sleeping off), and `CLAUDE_MUX_DEBUG=1` makes `claude-muxd` log every hook payload. Inside a session, `CLAUDE_MUX_SESSION_ID` is set and `claude-mux` is
on `PATH`.
