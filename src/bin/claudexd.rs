use std::process::ExitCode;

use clap::Parser;

/// The claudex daemon: owns every Claude Code session, its PTY and its
/// libghostty terminal, and serves `claudex` over a Unix socket.
///
/// Any `claudex` command that needs it starts it automatically, detached, so
/// there is no need to run it yourself or install it as a service. Running it
/// by hand keeps it in the foreground with logs on stderr. If a daemon is
/// already running for this state directory, it exits immediately. Stop it
/// with `claudex shutdown`.
#[derive(Parser)]
#[command(name = "claudexd", version, after_help = "State lives in $CLAUDEX_HOME/state.db (default ~/.claudex); \
the socket is $CLAUDEX_SOCKET (default $CLAUDEX_HOME/daemon.sock).")]
struct Cli {}

fn main() -> ExitCode {
    Cli::parse();
    match claudex::daemon::run() {
        Ok(claudex::daemon::Started::Ran) => ExitCode::SUCCESS,
        Ok(claudex::daemon::Started::AlreadyRunning) => {
            eprintln!("claudexd: already running for {}", claudex::paths::home().display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("claudexd: {e:#}");
            ExitCode::FAILURE
        }
    }
}
