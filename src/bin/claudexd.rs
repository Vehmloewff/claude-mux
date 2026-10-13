use std::process::ExitCode;

use clap::Parser;

/// The claudex daemon: owns every Claude Code session, its PTY and its
/// libghostty terminal, and serves `claudex` over a Unix socket.
///
/// It must be running before `claudex` is used; run it under a service
/// manager or in the background. Stop it with `claudex shutdown`.
#[derive(Parser)]
#[command(name = "claudexd", version, after_help = "State lives in $CLAUDEX_HOME/state.db (default ~/.claudex); \
the socket is $CLAUDEX_SOCKET (default $CLAUDEX_HOME/daemon.sock).")]
struct Cli {}

fn main() -> ExitCode {
    Cli::parse();
    match claudex::daemon::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("claudexd: {e:#}");
            ExitCode::FAILURE
        }
    }
}
