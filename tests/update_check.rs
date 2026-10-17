//! Hermetic CLI coverage: no real network requests, live daemon, or binary replacement.
//! A local HTTP server stands in for GitHub releases.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("claude-mux-cli-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir(root.join("state")).unwrap();
        Self(root)
    }

    fn command(&self, args: &[&str], releases_base: Option<&str>) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_claude-mux"));
        command
            .args(args)
            .env("CLAUDE_MUX_HOME", self.0.join("state"))
            .env("CLAUDE_MUX_SOCKET", self.0.join("absent.sock"))
            .env_remove("CLAUDE_MUX_PROCESS");
        match releases_base {
            Some(base) => {
                command.env("CLAUDE_MUX_RELEASES_BASE", base);
            }
            // No base configured: the check would hit real GitHub, so point it
            // at a closed port to prove no command depends on a response.
            None => {
                command.env("CLAUDE_MUX_RELEASES_BASE", "http://127.0.0.1:9");
            }
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = self.command(args, None);
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        output
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A GitHub releases double: `/releases/latest` redirects to the (only)
/// release `v999.0.0`, which every command's daily update check asks for.
struct Releases {
    base: String,
    latest_requests: Arc<Mutex<usize>>,
    _thread: std::thread::JoinHandle<()>,
}

impl Releases {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let latest_requests = Arc::new(Mutex::new(0));
        let counter = latest_requests.clone();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let counter = counter.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    // Slowing the redirect down widens the race the
                    // concurrency test exercises.
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    let mut stream = stream;
                    if line.contains("/releases/latest") {
                        *counter.lock().unwrap() += 1;
                        let _ = write!(
                            stream,
                            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/releases/tag/v999.0.0\r\n\
                             Content-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    } else {
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    let _ = stream.flush();
                });
            }
        });
        Self { base: format!("http://127.0.0.1:{port}"), latest_requests, _thread: thread }
    }

    fn latest_requests(&self) -> usize {
        *self.latest_requests.lock().unwrap()
    }
}

#[test]
fn internal_commands_and_explicit_upgrade_skip_automatic_checks() {
    let sandbox = Sandbox::new();
    let releases = Releases::new();
    sandbox.run(&["hook", "Stop"]);
    // Hold the daemon lock so this test cannot start a long-lived process.
    let lock = std::fs::File::create(sandbox.0.join("state/daemon.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    sandbox.run(&["daemon"]);
    sandbox.run(&["upgrade", "--version", env!("CARGO_PKG_VERSION")]);
    sandbox.run(&["--version"]);
    assert_eq!(releases.latest_requests(), 0);
    assert!(!sandbox.0.join("state/update-check").exists());
}

#[test]
fn failed_checks_are_silent_and_do_not_break_the_command() {
    let sandbox = Sandbox::new();
    // Port 9 is closed: the request fails without a response.
    for _ in 0..2 {
        let output = sandbox.command(&["shutdown"], None).output().unwrap();
        assert!(output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("update "));
    }
}

#[test]
fn available_updates_are_reported_on_every_user_command_without_refetching() {
    let sandbox = Sandbox::new();
    let releases = Releases::new();
    for _ in 0..2 {
        let output = sandbox.command(&["shutdown"], Some(&releases.base)).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stderr).contains("update v999.0.0 available"));
        assert!(output.stdout.is_empty());
    }
    assert_eq!(releases.latest_requests(), 1);
}

#[test]
fn concurrent_commands_make_only_one_request() {
    let sandbox = Sandbox::new();
    let releases = Releases::new();
    let mut children = Vec::new();
    for _ in 0..4 {
        children.push(sandbox.command(&["shutdown"], Some(&releases.base)).spawn().unwrap());
    }
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(releases.latest_requests(), 1);
}
