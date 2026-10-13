//! Client side of the daemon socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result, anyhow, bail};

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

/// Connect to the daemon, failing if it isn't running.
pub fn connect() -> Result<Conn> {
    try_connect().ok_or_else(|| {
        anyhow!(
            "the claudex daemon is not running at {} (start it with `claudexd`)",
            paths::socket().display()
        )
    })
}

/// Connect only if a daemon is already running.
pub fn connect_existing() -> Option<Conn> {
    try_connect()
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
