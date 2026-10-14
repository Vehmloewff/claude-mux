# claudex

One place to run and manage all your Claude Code sessions. Each session is a
real interactive `claude` running in a pseudo-terminal and emulated with
[libghostty](https://ghostty.org) (`libghostty-vt`). That means people, scripts
and other Claude sessions can all create sessions, send them work and wait
for results, while you watch and steer them from one TUI.

```sh
claudex create "Fix the login redirect bug in web/auth" --name login-bug   # → prints an id, e.g. 3f9a1c
claudex send 3f9a1c "Also add a regression test"                           # queued until claude is idle
claudex wait 3f9a1c                                                        # blocks until done, prints the reply
claudex manage                                                             # the TUI
```

## Install

Requires Rust, Zig 0.16 (`brew install zig@0.16`) and Claude Code on `PATH`.

```sh
cargo install --path . --locked
```

There's nothing else to set up: the first `claudex` command that needs the
daemon starts `claudexd` in the background.

### Nix

The flake provides `packages.<system>.default` (both binaries), an overlay
(`overlays.default`, adding `pkgs.claudex`) and a `devShells.default`. The
Ghostty source and its Zig dependencies are pinned as flake inputs, so the
build needs no network access.

```sh
nix run github:Vehmloewff/claudex -- list
nix profile install github:Vehmloewff/claudex
```

No service module is needed, since `claudexd` starts on demand.

Everything an agent needs to drive claudex is in `claudex --help`.

## Commands

| Command | |
|---|---|
| `claudex create [CONTEXT] [--id ID] [--name N] [-C DIR] [--model M] [--permission-mode P] [--yolo] [--trust] [--wait] [-- ARGS…]` | Start a session. CONTEXT becomes the first prompt (`-` reads stdin). Prints the id. |
| `claudex send [ID] MESSAGE [--now] [--wait]` | Send a message. It's queued until claude is idle, unless you pass `--now`. With no ID it goes to the newest running session. |
| `claudex wait ID… [--any] [--timeout S] [-q] [--json]` | Block until the sessions settle and print the final reply. |
| `claudex manage [ID]` / `claudex attach ID` | Open the TUI, optionally focused on one session. |
| `claudex list` · `status ID` · `peek ID [--ansi]` · `last ID` | Inspect sessions. |
| `claudex keys ID KEY…` | Press keys, e.g. `claudex keys 3f9a1c down enter` to answer a dialog. |
| `claudex kill ID` · `resume ID` · `rm ID…` · `rename ID NAME` | Manage the lifecycle. |
| `claudex shutdown` | Stop the daemon and all sessions. |

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
| ✕ | exited | the claude process ended on its own |
| ◼ | stopped | killed or the daemon restarted; `resume` continues the conversation |

## `claudex manage`

The left sidebar lists every session with its live status, task title,
directory and age. The right pane shows the selected session's terminal.

- **Session list:** `j`/`k` or arrows to move, `1`–`9` to jump, `enter` to
  focus the terminal, `n` new session, `s` send a message, `x` kill, `r`
  resume, `d` delete, `R` rename, `PgUp`/`PgDn` or the mouse wheel to scroll,
  `?` help, `q` quit. Quitting doesn't stop any sessions.
- **Terminal focused:** every key goes to claude, encoded by libghostty
  (kitty keyboard protocol and paste included). `ctrl-]` returns to the list.

## How it works

- **Daemon.** `claudexd` is a separate binary that is never run as a
  service. Any `claudex` command that needs it (everything except
  `shutdown` and the internal hook) starts it if the socket doesn't answer.
  It's double-forked into its own session, so it isn't a child of the
  command that started it, and its output goes to `~/.claudex/daemon.log`.
  A lock file (`~/.claudex/daemon.lock`) guarantees one daemon per state
  directory even when several commands start it at once. It stays up until
  `claudex shutdown` (or `SIGTERM`), which stops the sessions and records
  them as `stopped` so they can be resumed. Run `claudexd` by hand to keep
  it in the foreground with logs on stderr. It listens
  on `~/.claudex/daemon.sock`. It owns every claude process and its PTY.
  Session records are kept in a Turso database at `~/.claudex/state.db`, so
  sessions survive a daemon restart as `stopped` and can be resumed. Its
  schema is a set of migrations embedded in the binary (`migrations/`), which
  the daemon applies on startup and records in `_applied_migrations`. Claude Code's own session
  id is fixed with `--session-id`, which lets `resume` use `claude --resume`.
- **Terminal emulation.** All PTY output is fed into a libghostty-vt
  `Terminal`, one per session, each on its own thread because the handles are
  `!Send`. That terminal answers claude's queries, encodes keys, mouse and
  paste, and produces render snapshots. The TUI receives those snapshots as
  styled spans.
- **Status.** Every claude is started with `--settings` hooks
  (`SessionStart`, `UserPromptSubmit`, `PostToolUse`, `Notification`,
  `Stop`) that call `claudex hook`, which reports back to the daemon. That
  makes "working / done" exact, and `Stop` also captures the final reply.
  The `Stop` payload lists background tasks that are still running. If any
  are, the session is `background` rather than `idle` until claude is woken
  by their task notification (a fresh `UserPromptSubmit`) and finishes. A
  footer scan (`· 1 shell ·`) covers tasks that end without waking claude.
  Blocking dialogs such as the trust prompt don't fire hooks, so the daemon
  also scans the emulated screen for them.
- **Sending.** Messages are pasted into claude as a bracketed paste followed
  by Enter, once the session is idle, so each one becomes its own turn.

Environment variables: `CLAUDEX_HOME` sets the directory holding `state.db`, the log and the socket,
`CLAUDEX_SOCKET` sets the socket path, `CLAUDEX_CLAUDE_BIN` sets the
claude binary, and `CLAUDEX_DEBUG=1` makes `claudexd` log every hook payload. Inside a session, `CLAUDEX_SESSION_ID` is set and `claudex` is
on `PATH`.
