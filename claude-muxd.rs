use std::process::ExitCode;

use clap::Parser;

/// The claude-mux daemon: owns every Claude Code session, its PTY and its
/// terminal emulator, and serves `claude-mux` over a Unix socket.
///
/// Any `claude-mux` command that needs it starts it automatically, detached, so
/// there is no need to run it yourself or install it as a service. Running it
/// by hand keeps it in the foreground with logs on stderr. If a daemon is
/// already running for this state directory, it exits immediately. Stop it
/// with `claude-mux shutdown`.
#[derive(Parser)]
#[command(name = "claude-muxd", version, after_help = "State lives in $CLAUDE_MUX_HOME/state.db (default ~/.claude-mux); \
the socket is $CLAUDE_MUX_SOCKET (default $CLAUDE_MUX_HOME/daemon.sock).")]
struct Cli {}

fn main() -> ExitCode {
    Cli::parse();
    match claude_mux::daemon::run() {
        Ok(claude_mux::daemon::Started::Ran) => ExitCode::SUCCESS,
        Ok(claude_mux::daemon::Started::AlreadyRunning) => {
            eprintln!("claude-muxd: already running for {}", claude_mux::paths::home().display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("claude-muxd: {e:#}");
            ExitCode::FAILURE
        }
    }
}
