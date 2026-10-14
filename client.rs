//! Client side of the daemon socket, including starting the daemon on demand.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::paths;
use crate::protocol::{Request, Response};

pub struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Conn {
    pub fn send<T: serde::Serialize>(&mut self, msg: &T) -> Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        Ok(())
    }

    pub fn recv<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&line).context("bad message from daemon")?))
    }

    pub fn writer(&self) -> Result<UnixStream> {
        Ok(self.writer.try_clone()?)
    }

    pub fn into_reader(self) -> BufReader<UnixStream> {
        self.reader
    }
}

fn try_connect() -> Option<Conn> {
    let stream = UnixStream::connect(paths::socket()).ok()?;
    let writer = stream.try_clone().ok()?;
    Some(Conn { reader: BufReader::new(stream), writer })
}

/// Connect to the daemon, starting `claude-muxd` first if it isn't running.
pub fn connect() -> Result<Conn> {
    if let Some(conn) = try_connect() {
        return Ok(conn);
    }
    spawn_daemon()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(conn) = try_connect() {
            return Ok(conn);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    bail!(
        "started claude-muxd but it isn't answering at {} (see {})",
        paths::socket().display(),
        paths::log_file().display()
    )
}

/// Connect only if a daemon is already running (never starts one).
pub fn connect_existing() -> Option<Conn> {
    try_connect()
}

/// Start claude-muxd fully detached: double-forked so it is re-parented to
/// init/launchd rather than staying a child of this command, in its own
/// session, with output appended to the daemon log. If several commands race
/// to start it, claude-muxd's lock file lets only one of them run.
fn spawn_daemon() -> Result<()> {
    std::fs::create_dir_all(paths::home())?;
    let exe = paths::sibling_exe("claude-muxd");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log_file())
        .with_context(|| format!("open {}", paths::log_file().display()))?;
    let mut cmd = Command::new(&exe);
    cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            // Only async-signal-safe calls here (we are between fork and exec).
            match libc::fork() {
                -1 => return Err(std::io::Error::last_os_error()),
                0 => {}                  // grandchild: becomes claude-muxd
                _ => libc::_exit(0),     // intermediate child: exit right away
            }
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to start {} (is claude-muxd installed?)", exe.display()))?;
    // Reap the intermediate process; claude-muxd itself is no longer our child.
    let _ = child.wait();
    Ok(())
}

/// Send one request and read one response.
pub fn request(req: &Request) -> Result<Response> {
    let mut conn = connect()?;
    conn.send(req)?;
    match conn.recv::<Response>()? {
        Some(Response::Error { message }) => bail!(message),
        Some(resp) => Ok(resp),
        None => bail!("daemon closed the connection"),
    }
}
