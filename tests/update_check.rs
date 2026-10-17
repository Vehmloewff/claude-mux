//! Hermetic CLI coverage: no network requests, live daemon, or binary replacement.

use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("claude-mux-cli-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir(root.join("state")).unwrap();
        let curl = root.join("bin/curl");
        std::fs::write(
            &curl,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$REVIEW_CURL_LOG"
if [ "${REVIEW_SLOW:-}" = 1 ]; then sleep 0.2; fi
if [ "${REVIEW_FAIL:-}" = 1 ]; then exit 22; fi
printf 'https://github.com/Vehmloewff/claude-mux/releases/tag/%s' "$REVIEW_TAG"
"#,
        )
        .unwrap();
        std::fs::set_permissions(curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self(root)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_claude-mux"));
        command
            .args(args)
            .env("CLAUDE_MUX_HOME", self.0.join("state"))
            .env("CLAUDE_MUX_SOCKET", self.0.join("absent.sock"))
            .env_remove("CLAUDE_MUX_PROCESS")
            .env("REVIEW_CURL_LOG", self.0.join("curl.log"))
            .env("REVIEW_TAG", "v999.0.0")
            .env("REVIEW_FAIL", "0")
            .env("REVIEW_SLOW", "0")
            .env("PATH", format!("{}:{}", self.0.join("bin").display(), std::env::var("PATH").unwrap_or_default()));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        output
    }

    fn requests(&self) -> Vec<String> {
        std::fs::read_to_string(self.0.join("curl.log")).unwrap_or_default().lines().map(str::to_owned).collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn internal_commands_and_explicit_upgrade_skip_automatic_checks() {
    let sandbox = Sandbox::new();
    sandbox.run(&["hook", "Stop"]);
    // Hold the daemon lock so this test cannot start a long-lived process.
    let lock = std::fs::File::create(sandbox.0.join("state/daemon.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    sandbox.run(&["daemon"]);
    sandbox.run(&["upgrade", "--version", env!("CARGO_PKG_VERSION")]);
    sandbox.run(&["--version"]);
    assert!(sandbox.requests().is_empty());
    assert!(!sandbox.0.join("state/update-check").exists());
}

#[test]
fn requests_are_bounded_and_failed_attempts_are_cached() {
    let sandbox = Sandbox::new();
    for _ in 0..2 {
        let output = sandbox.command(&["shutdown"]).env("REVIEW_FAIL", "1").output().unwrap();
        assert!(output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("update "));
    }
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("--connect-timeout 1 --max-time 2 --retry 0"));
}

#[test]
fn available_updates_are_reported_on_every_user_command_without_refetching() {
    let sandbox = Sandbox::new();
    for _ in 0..2 {
        let output = sandbox.run(&["shutdown"]);
        assert!(String::from_utf8_lossy(&output.stderr).contains("update v999.0.0 available"));
        assert!(output.stdout.is_empty());
    }
    assert_eq!(sandbox.requests().len(), 1);
}

#[test]
fn concurrent_commands_make_only_one_request() {
    let sandbox = Sandbox::new();
    let mut children = Vec::new();
    for _ in 0..4 {
        children.push(
            sandbox
                .command(&["shutdown"])
                .env("REVIEW_SLOW", "1")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(sandbox.requests().len(), 1);
}
