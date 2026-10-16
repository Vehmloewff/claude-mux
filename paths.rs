use std::path::PathBuf;

/// Root directory for daemon state: `$CLAUDE_MUX_HOME` or `~/.claude-mux`.
pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_MUX_HOME") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/tmp".into());
    home.join(".claude-mux")
}

/// `$CLAUDE_MUX_SOCKET`, else `<home>/daemon.sock` — unless that exceeds the
/// Unix socket path limit, in which case a short per-home path in /tmp.
pub fn socket() -> PathBuf {
    if let Some(path) = std::env::var_os("CLAUDE_MUX_SOCKET") {
        return PathBuf::from(path);
    }
    let path = home().join("daemon.sock");
    if path.as_os_str().len() < 100 {
        return path;
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    home().hash(&mut h);
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/claude-mux-{uid}-{:012x}.sock", h.finish() & 0xffff_ffff_ffff))
}

pub fn db_file() -> PathBuf {
    home().join("state.db")
}

pub fn log_file() -> PathBuf {
    home().join("daemon.log")
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Locate the `claude-mux` executable: next to the running binary if present,
/// otherwise rely on PATH. Joining the name (rather than using `current_exe`
/// as is) keeps this working after `claude-mux upgrade` replaced the file.
pub fn exe() -> PathBuf {
    let name = "claude-mux";
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}
