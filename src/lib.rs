//! claudex: run and manage many Claude Code sessions.
//!
//! Shared by the `claudex` CLI and the `claudexd` daemon.

pub mod client;
pub mod daemon;
pub mod hooks;
pub mod paths;
pub mod protocol;
pub mod term;
