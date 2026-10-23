# claude-mux

Run many Claude Code sessions at once, and drive them from anywhere.

claude-mux keeps your `claude` sessions running in a background daemon.
Attach to any of them to type into it. Scripts, and other Claude sessions,
can start them, send them work and wait for the result from the command
line.

- **Real sessions.** Each one is the normal interactive `claude` in its own
  terminal, not a stripped-down API wrapper. Attach at any time and keep
  typing.
- **Scriptable.** `create`, `send` and `wait` turn Claude Code into a
  building block. Send messages any time: they queue until claude is ready.
  `wait` returns the final reply and exits with a code that says what
  happened.
- **Accurate status.** Claude Code hooks report whether a session is
  working, idle, blocked on a permission prompt or waiting on background
  tasks. claude-mux doesn't guess from the screen.
- **Cheap when idle.** Idle sessions are put to sleep to free memory and
  resumed automatically when you send them something.
- **Durable.** Sessions outlive the commands that start them and survive
  daemon restarts, so you can resume them later.

```sh
claude-mux create "Fix the login redirect bug" -d "Login bug"   # prints the session's id
claude-mux send -s 3f9a "Also add a regression test"            # any unique prefix of the id works
claude-mux wait                                                 # no -s: pick sessions from a list
claude-mux attach -s 3f9a                                       # type into it; ctrl-esc detaches
```

## Install

Requires [Claude Code](https://claude.com/claude-code) on your `PATH`. Works
on macOS and Linux.

```sh
curl -fsSL https://raw.githubusercontent.com/Vehmloewff/claude-mux/main/install.sh | sh
```

This installs the binary to `~/.local/bin`. Run `claude-mux upgrade` later to
update it. User-facing commands check GitHub for updates at most once every
24 hours.

## Usage

| Command | |
|---|---|
| `create [PROMPT]` | Start a session and print its id. |
| `send MESSAGE` | Send a message. It's queued until the session is idle. |
| `wait` | Block until the sessions finish, then print the final reply. |
| `attach` | Type into a session's terminal. `ctrl-esc` detaches. |
| `list` · `status` · `peek` · `last` | Inspect sessions. |
| `messages` | List the messages sent to a session, how long claude worked on each, and its reply. |
| `watch` | Print each new message once claude has replied to it. |
| `questions` | List and answer questions a session is waiting on. |
| `keys KEY…` | Press keys in a session, such as `down enter`. |
| `kill` · `resume` · `rm` · `describe` | Manage sessions. |
| `shutdown` · `upgrade` | Stop or update the daemon. |

A session's id is the id of its Claude Code conversation. Commands pick a
session with `-s`/`--session`, which takes the id or any unique prefix of it;
`list` shows the first 8 characters. Without `-s`, a command uses the only
session it applies to. When several match, claude-mux asks which one you
mean, and scripts with no terminal get an error listing them instead. Each session is labelled by its description
(`create -d`, or `describe` later), falling back to its first prompt.

Since a session is one conversation, `/clear` starts a new session: the
running claude moves to a new id, and the old session is marked `cleared`
and points to its successor. It can still be resumed.

Run `claude-mux --help` for every flag. It covers everything an agent needs
to drive claude-mux.

`attach` shows the session's claude full screen. Press `ctrl-esc` to detach
and leave it running. Terminals that can't report `ctrl-esc` (those without
the kitty keyboard protocol, such as Terminal.app) take `ctrl-]` instead.

## How it works

A daemon owns every `claude` process and its pseudo-terminal, and it
emulates each terminal with
[`alacritty_terminal`](https://crates.io/crates/alacritty_terminal). Every
session is started with Claude Code hooks that report back to the daemon,
which is how it knows each session's state and final reply. Sessions are
stored in `~/.claude-mux/`.
