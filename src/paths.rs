use std::path::PathBuf;

/// Root directory for daemon state: `$CLAUDEX_HOME` or `~/.claudex`.
pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDEX_HOME") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/tmp".into());
    home.join(".claudex")
}

/// `$CLAUDEX_SOCKET`, else `<home>/daemon.sock` — unless that exceeds the
/// Unix socket path limit, in which case a short per-home path in /tmp.
pub fn socket() -> PathBuf {
    if let Some(path) = std::env::var_os("CLAUDEX_SOCKET") {
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
    PathBuf::from(format!("/tmp/claudex-{uid}-{:012x}.sock", h.finish() & 0xffff_ffff_ffff))
}

pub fn sessions_file() -> PathBuf {
    home().join("sessions.json")
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

/// Locate a sibling claudex executable (`claudex` / `claudexd`): next to the
/// running binary if present, otherwise rely on PATH.
pub fn sibling_exe(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}
