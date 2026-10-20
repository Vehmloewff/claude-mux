//! `claude-mux upgrade`: replace the binary with a GitHub release build.
//!
//! Uses the same release assets as `install.sh`
//! (`claude-mux-<arch>-<os>.tar.gz`, holding the `claude-mux` binary),
//! fetched with `reqwest` and unpacked with `tar`.

use std::future::Future;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use semver::Version;
use tokio::io::AsyncWriteExt;

const REPO: &str = "Vehmloewff/claude-mux";

/// Base of every release URL. `CLAUDE_MUX_RELEASES_BASE` points it at a
/// test double so the update check stays hermetic in CI.
fn releases_base() -> String {
    std::env::var("CLAUDE_MUX_RELEASES_BASE")
        .unwrap_or_else(|_| format!("https://github.com/{REPO}"))
}

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

fn check_client() -> Result<reqwest::Client> {
    // The daily check must never hold a command up: same bounds as the old
    // `curl --connect-timeout 1 --max-time 2 --retry 0`.
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(2))
        .build()
        .context("building http client")
}

fn download_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("building http client")
}

/// Tag of the latest release, read from where `/releases/latest` redirects
/// (no API token or rate limit involved).
async fn latest_tag() -> Result<String> {
    let response = check_client()?
        .get(format!("{}/releases/latest", releases_base()))
        .send()
        .await
        .with_context(|| format!("no published release found for {REPO}"))?;
    if !response.status().is_success() {
        bail!(
            "no published release found for {REPO} ({})",
            response.status()
        );
    }
    let url = response.url().to_string();
    match url.trim().rsplit_once("/tag/") {
        Some((_, tag)) if !tag.is_empty() => Ok(tag.to_string()),
        _ => bail!("no releases found for {REPO}"),
    }
}

/// The release tag to install: `version` (with or without a leading `v`), or the latest.
pub async fn resolve_tag(version: Option<String>) -> Result<String> {
    match version {
        Some(v) if v.starts_with('v') => Ok(v),
        Some(v) => Ok(format!("v{v}")),
        None => latest_tag().await,
    }
}

/// Best-effort daily update check for user-facing commands only. Internal hooks and the daemon
/// must never wait for this network request. Cached reminders are revalidated against this binary.
pub async fn notify_if_update_available() {
    let current =
        Version::parse(env!("CARGO_PKG_VERSION")).expect("package version is valid semver");
    if let Ok(Some(tag)) = check_for_update(
        &crate::paths::home(),
        crate::paths::now(),
        &current,
        latest_tag,
    )
    .await
    {
        eprintln!("claude-mux: update {tag} available. Run `claude-mux upgrade` to install it.");
    }
}

fn is_newer_tag(tag: &str, current: &Version) -> bool {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .is_ok_and(|version| version.cmp_precedence(current).is_gt())
}

async fn save_check(path: &Path, checked_at: u64, available: Option<&str>) -> Result<()> {
    // Atomic replacement lets concurrent commands read the cache without waiting for the lock,
    // and preserves the timestamp if a process dies mid-write.
    let new = path.with_extension("new");
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&new)
        .await?;
    file.write_all(format!("{checked_at}\n{}\n", available.unwrap_or("")).as_bytes())
        .await?;
    tokio::fs::rename(new, path).await?;
    Ok(())
}

async fn read_check(path: &Path, current: &Version) -> Option<(u64, Option<String>)> {
    let cached = tokio::fs::read_to_string(path).await.ok()?;
    let mut lines = cached.lines();
    let checked_at = lines.next()?.parse::<u64>().ok()?;
    let available = lines
        .next()
        .filter(|s| is_newer_tag(s, current))
        .map(str::to_owned);
    Some((checked_at, available))
}

async fn check_for_update<F, Fut>(
    home: &Path,
    now: u64,
    current: &Version,
    latest: F,
) -> Result<Option<String>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(home.join("update-check.lock"))?;
    let state_file = home.join("update-check");
    // Never wait for another command's network request. Closing the file releases the lock.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(read_check(&state_file, current)
            .await
            .and_then(|(_, available)| available));
    }

    // Re-read after acquiring the lock: another process may have just finished its check.
    let state = read_check(&state_file, current).await;
    let due = state
        .as_ref()
        .is_none_or(|(checked_at, _)| now.saturating_sub(*checked_at) >= 24 * 60 * 60);
    let mut available = state.and_then(|(_, available)| available);
    if due {
        // Persist before attempting the request: failures and interrupted checks count too.
        // If state cannot be saved, do not make an unthrottled network request.
        save_check(&state_file, now, available.as_deref()).await?;
        if let Ok(tag) = latest().await {
            available = is_newer_tag(&tag, current).then_some(tag);
            save_check(&state_file, now, available.as_deref()).await?;
        }
    }
    Ok(available)
}

/// Download release `tag` and replace the running binary with it. Returns the binary's path.
pub async fn install(tag: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("locating the running binary")?;
    let url = format!(
        "{}/releases/download/{tag}/{}",
        releases_base(),
        asset_name()?
    );
    let tmp = std::env::temp_dir().join(format!("claude-mux-upgrade-{}", std::process::id()));
    tokio::fs::create_dir_all(&tmp).await?;
    let result = download_and_install(&url, &tmp, &exe).await;
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    result.map(|()| exe)
}

async fn download_and_install(url: &str, tmp: &Path, exe: &Path) -> Result<()> {
    let archive = tmp.join("release.tar.gz");
    eprintln!("Downloading {url}");
    let response = download_client()?
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    if !response.status().is_success() {
        bail!("downloading {url} failed ({})", response.status());
    }
    let mut file = tokio::fs::File::create(&archive)
        .await
        .context("create archive file")?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        file.write_all(&chunk?).await.context("writing archive")?;
    }
    file.flush().await?;
    drop(file);
    let status = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(tmp)
        .status()
        .await
        .context("failed to run tar")?;
    if !status.success() {
        bail!("failed to unpack {}", archive.display());
    }

    let src = tmp.join("claude-mux");
    if !tokio::fs::metadata(&src)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        bail!("release archive does not contain claude-mux");
    }
    // Copy next to the target, then rename over it, so running copies
    // (this one and the daemon) keep their old inode.
    let new = exe.with_file_name(".claude-mux.new");
    tokio::fs::copy(&src, &new)
        .await
        .with_context(|| format!("writing {}", new.display()))?;
    tokio::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).await?;
    tokio::fs::rename(&new, exe)
        .await
        .with_context(|| format!("replacing {}", exe.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;

    struct Home(PathBuf);

    impl Home {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("claude-mux-update-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        async fn seed(&self, checked_at: u64, available: Option<&str>) {
            save_check(&self.0.join("update-check"), checked_at, available)
                .await
                .unwrap();
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn compares_semver_precedence_not_strings_or_build_metadata() {
        for (current, tag, expected) in [
            ("1.9.0", "v1.10.0", true),
            ("1.10.0", "v1.9.0", false),
            ("1.0.0", "1.0.1", true),
            ("1.0.0", "v1.0.0", false),
            ("1.0.0", "v1.0.0-beta.1", false),
            ("1.0.0-beta.1", "v1.0.0-beta.2", true),
            ("1.0.0-beta.2", "v1.0.0", true),
            ("1.0.0", "v1.0.0+build.2", false),
            ("1.0.0", "not-a-version", false),
        ] {
            assert_eq!(
                is_newer_tag(tag, &Version::parse(current).unwrap()),
                expected,
                "{current} vs {tag}"
            );
        }
    }

    #[tokio::test]
    async fn successful_checks_are_cached_until_the_daily_boundary() {
        let home = Home::new();
        let current = Version::new(1, 0, 0);
        let first = check_for_update(&home.0, 1, &current, || async { Ok("v1.1.0".into()) })
            .await
            .unwrap();
        assert_eq!(first.as_deref(), Some("v1.1.0"));
        let cached = check_for_update(&home.0, DAY, &current, || async {
            panic!("checked too soon")
        })
        .await
        .unwrap();
        assert_eq!(cached, first);
        let refreshed =
            check_for_update(&home.0, DAY + 1, &current, || async { Ok("v1.2.0".into()) })
                .await
                .unwrap();
        assert_eq!(refreshed.as_deref(), Some("v1.2.0"));
    }

    #[tokio::test]
    async fn cached_reminders_stop_after_installing_the_release_or_a_newer_version() {
        let home = Home::new();
        home.seed(100, Some("v1.1.0")).await;
        for current in [Version::new(1, 1, 0), Version::new(1, 2, 0)] {
            assert_eq!(
                check_for_update(&home.0, 101, &current, || async {
                    panic!("cache is fresh")
                })
                .await
                .unwrap(),
                None
            );
        }
    }

    #[tokio::test]
    async fn older_equal_and_invalid_releases_are_not_cached_as_updates() {
        for tag in ["v0.9.0", "v1.0.0", "bad-tag"] {
            let home = Home::new();
            let current = Version::new(1, 0, 0);
            assert_eq!(
                check_for_update(&home.0, 1, &current, || async { Ok(tag.into()) })
                    .await
                    .unwrap(),
                None
            );
            assert_eq!(
                read_check(&home.0.join("update-check"), &current).await,
                Some((1, None))
            );
        }
    }

    #[tokio::test]
    async fn failed_checks_are_throttled_and_preserve_available_updates() {
        for available in [None, Some("v1.1.0")] {
            let home = Home::new();
            home.seed(1, available).await;
            let current = Version::new(1, 0, 0);
            let result =
                check_for_update(&home.0, DAY + 1, &current, || async { bail!("offline") })
                    .await
                    .unwrap();
            assert_eq!(result.as_deref(), available);
            let cached = check_for_update(&home.0, DAY + 2, &current, || async {
                panic!("retried failed check")
            })
            .await
            .unwrap();
            assert_eq!(cached, result);
            assert_eq!(
                read_check(&home.0.join("update-check"), &current).await,
                Some((DAY + 1, result))
            );
        }
    }

    // A plain test with its own runtime: `catch_unwind` must drive the
    // panicking future without nesting block_on inside another runtime.
    #[test]
    fn interrupted_checks_also_keep_the_timestamp_and_release_the_lock() {
        let home = Home::new();
        let current = Version::new(1, 0, 0);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = runtime.block_on(check_for_update(&home.0, 1, &current, || async {
                panic!("interrupted request")
            }));
        }));
        assert!(interrupted.is_err());
        assert_eq!(
            runtime
                .block_on(check_for_update(&home.0, 2, &current, || async {
                    panic!("retried interrupted check")
                }))
                .unwrap(),
            None
        );
        // A later refresh proves unwinding released the lock.
        assert_eq!(
            runtime
                .block_on(check_for_update(&home.0, DAY + 1, &current, || async {
                    Ok("v1.1.0".into())
                }))
                .unwrap()
                .as_deref(),
            Some("v1.1.0")
        );
    }

    #[tokio::test]
    async fn lock_contention_uses_cached_reminders_without_fetching() {
        let home = Home::new();
        home.seed(1, Some("v1.1.0")).await;
        let lock = std::fs::File::create(home.0.join("update-check.lock")).unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let result = check_for_update(&home.0, DAY + 1, &Version::new(1, 0, 0), || async {
            panic!("lock is held")
        })
        .await
        .unwrap();
        assert_eq!(result.as_deref(), Some("v1.1.0"));
        assert_eq!(
            read_check(&home.0.join("update-check"), &Version::new(1, 0, 0))
                .await
                .unwrap()
                .0,
            1
        );
    }

    #[tokio::test]
    async fn malformed_cache_is_replaced_and_missing_state_directories_are_created() {
        let home = Home::new();
        std::fs::write(home.0.join("update-check"), "bad timestamp\nv1.1.0\n").unwrap();
        let current = Version::new(1, 0, 0);
        assert_eq!(
            check_for_update(&home.0, 1, &current, || async { Ok("v1.1.0".into()) })
                .await
                .unwrap()
                .as_deref(),
            Some("v1.1.0")
        );
        let nested = home.0.join("new/state");
        check_for_update(&nested, 1, &current, || async { Ok("v1.0.0".into()) })
            .await
            .unwrap();
        assert_eq!(
            std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(nested.join("update-check"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn unusable_state_directory_never_triggers_an_unthrottled_request() {
        let home = Home::new();
        let not_a_dir = home.0.join("file");
        std::fs::write(&not_a_dir, "").unwrap();
        assert!(
            check_for_update(&not_a_dir, 1, &Version::new(1, 0, 0), || async {
                panic!("cannot save state")
            })
            .await
            .is_err()
        );
    }
}
