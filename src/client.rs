//! Client side of the daemon socket, including daemon auto-start.

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

/// Connect to the daemon, starting it in the background if needed.
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
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "could not reach the claudex daemon at {} (see {})",
        paths::socket().display(),
        paths::log_file().display()
    )
}

/// Connect only if a daemon is already running.
pub fn connect_existing() -> Option<Conn> {
    try_connect()
}

fn spawn_daemon() -> Result<()> {
    std::fs::create_dir_all(paths::home())?;
    let exe = paths::sibling_exe("claudexd");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log_file())?;
    let mut cmd = Command::new(&exe);
    cmd.stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Detach from the caller's session so the daemon outlives the terminal.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
        .with_context(|| format!("failed to start {} (is claudexd installed?)", exe.display()))?;
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
