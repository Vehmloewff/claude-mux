//! `claude-mux upgrade`: replace the binary with a GitHub release build.
//!
//! Uses the same release assets as `install.sh`
//! (`claude-mux-<arch>-<os>.tar.gz`, holding the `claude-mux` binary),
//! fetched with `curl` and unpacked with `tar`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const REPO: &str = "Vehmloewff/claude-mux";

/// Release asset for the platform this binary was built for.
fn asset_name() -> Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-musl",
        other => bail!("no release builds for {other}"),
    };
    let arch = match std::env::consts::ARCH {
        arch @ ("x86_64" | "aarch64") => arch,
        other => bail!("no release builds for {other}"),
    };
    Ok(format!("claude-mux-{arch}-{os}.tar.gz"))
}

fn curl(args: &[&str]) -> Result<String> {
    let out = Command::new("curl").args(["-fsSL"]).args(args).output().context("failed to run curl")?;
    if !out.status.success() {
        bail!("curl failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Tag of the latest release, read from where `/releases/latest` redirects
/// (no API token or rate limit involved).
fn latest_tag() -> Result<String> {
    let url = curl(&["-o", "/dev/null", "-w", "%{url_effective}", &format!("https://github.com/{REPO}/releases/latest")])
        .with_context(|| format!("no published release found for {REPO}"))?;
    match url.trim().rsplit_once("/tag/") {
        Some((_, tag)) if !tag.is_empty() => Ok(tag.to_string()),
        _ => bail!("no releases found for {REPO}"),
    }
}

/// The release tag to install: `version` (with or without a leading `v`), or the latest.
pub fn resolve_tag(version: Option<String>) -> Result<String> {
    match version {
        Some(v) if v.starts_with('v') => Ok(v),
        Some(v) => Ok(format!("v{v}")),
        None => latest_tag(),
    }
}

/// Download release `tag` and replace the running binary with it. Returns the binary's path.
pub fn install(tag: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().and_then(|p| p.canonicalize()).context("locating the running binary")?;
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{}", asset_name()?);
    let tmp = std::env::temp_dir().join(format!("claude-mux-upgrade-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let result = download_and_install(&url, &tmp, &exe);
    let _ = std::fs::remove_dir_all(&tmp);
    result.map(|()| exe)
}

fn download_and_install(url: &str, tmp: &Path, exe: &Path) -> Result<()> {
    let archive = tmp.join("release.tar.gz");
    eprintln!("Downloading {url}");
    curl(&["-o", &archive.to_string_lossy(), url]).with_context(|| format!("downloading {url}"))?;
    let status = Command::new("tar").arg("-xzf").arg(&archive).arg("-C").arg(tmp).status().context("failed to run tar")?;
    if !status.success() {
        bail!("failed to unpack {}", archive.display());
    }

    let src = tmp.join("claude-mux");
    if !src.is_file() {
        bail!("release archive does not contain claude-mux");
    }
    // Copy next to the target, then rename over it, so running copies
    // (this one and the daemon) keep their old inode.
    let new = exe.with_file_name(".claude-mux.new");
    std::fs::copy(&src, &new).with_context(|| format!("writing {}", new.display()))?;
    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&new, exe).with_context(|| format!("replacing {}", exe.display()))?;
    Ok(())
}
