//! Updates from GitHub releases. A release is a `vX.Y.Z` tag with two assets: `ember.exe` and
//! `ember.exe.sha256` (its SHA-256 in hex). When a newer one is out, the new exe is downloaded
//! beside the running one, checked against its hash, and swapped in: Windows lets a running exe
//! be renamed, so the old one becomes `ember.exe.old` (deleted on the next start) and the new one
//! takes its name. It runs from the next launch; the app offers to restart into it.
//!
//! The release repo must be public: a private one can't be read without a token, and no token
//! ships in the app. Until then the check simply finds nothing.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Where releases are published ("owner/name").
const REPO: &str = "Taxents/Ember";
const UA: &str = concat!("Ember/", env!("CARGO_PKG_VERSION"));
const ASSET: &str = "ember.exe";

/// Deletes what the last update left behind. Call once at start.
pub fn clean_up() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(sibling(&exe, ".old"));
        let _ = std::fs::remove_file(sibling(&exe, ".new"));
    }
}

/// Where an update check is.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Checking,
    UpToDate,
    /// A newer version is out and downloading.
    Downloading(String),
    /// A newer version is installed and runs from the next start.
    Ready(String),
    /// The check or the download failed (offline, GitHub unreachable, a bad download).
    Failed,
    /// Development builds aren't installed, so they don't update.
    Off,
}

/// Checks for a newer release and installs it, telling `report` each step. Blocks (network);
/// call on a worker.
pub fn check(report: &dyn Fn(Status)) -> Status {
    let done = |s: Status| {
        report(s.clone());
        s
    };
    if cfg!(debug_assertions) {
        return done(Status::Off);
    }
    report(Status::Checking);
    // The list rather than releases/latest: that 404s until the first release is out.
    let Some(releases) = get_json(&format!(
        "https://api.github.com/repos/{REPO}/releases?per_page=10"
    )) else {
        return done(Status::Failed);
    };
    let none = serde_json::Value::Null;
    let release = releases
        .as_array()
        .and_then(|l| {
            l.iter()
                .find(|r| r["prerelease"] == false && r["draft"] == false)
        })
        .unwrap_or(&none);
    let Some(version) = release["tag_name"]
        .as_str()
        .map(|t| t.trim_start_matches('v').to_string())
    else {
        // No release yet.
        return done(Status::UpToDate);
    };
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return done(Status::UpToDate);
    }
    report(Status::Downloading(version.clone()));
    let asset = |name: &str| {
        release["assets"]
            .as_array()?
            .iter()
            .find(|a| a["name"].as_str() == Some(name))?["browser_download_url"]
            .as_str()
            .map(str::to_string)
    };
    let installed = (|| {
        let exe_url = asset(ASSET)?;
        let hash_url = asset(&format!("{ASSET}.sha256"))?;
        let want = String::from_utf8(get_bytes(&hash_url, 1024)?).ok()?;
        let want = want.split_whitespace().next()?.to_lowercase();
        let bytes = get_bytes(&exe_url, 200 * 1024 * 1024)?;
        let got: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if got != want {
            log::warn!("update {version}: hash mismatch, not installed");
            return None;
        }
        match install(&bytes) {
            Ok(()) => Some(()),
            Err(e) => {
                log::warn!("update {version}: {e}");
                None
            }
        }
    })();
    match installed {
        Some(()) => {
            log::info!("update {version} installed; runs from the next start");
            done(Status::Ready(version))
        }
        None => done(Status::Failed),
    }
}

/// Set when the user asked to restart into the update; read once the window has closed.
pub static RESTART: AtomicBool = AtomicBool::new(false);

/// One check at a time; none once an update is installed.
static BUSY: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Checks once on its own thread, unless a check is running or an update is already in.
/// `report` hears each step (on that thread: hand it to the UI thread yourself).
pub fn check_now(report: impl Fn(Status) + Send + 'static) {
    if INSTALLED.load(Ordering::Relaxed) || BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("update".into())
        .spawn(move || {
            if matches!(check(&report), Status::Ready(_)) {
                INSTALLED.store(true, Ordering::Relaxed);
            }
            BUSY.store(false, Ordering::Release);
        });
}

/// Checks right away at start, then every six hours while the app runs.
pub fn watch(report: impl Fn(Status) + Send + Sync + Clone + 'static) {
    let _ = std::thread::Builder::new()
        .name("update-timer".into())
        .spawn(move || {
            loop {
                check_now(report.clone());
                std::thread::sleep(std::time::Duration::from_secs(6 * 3600));
            }
        });
}

/// Starts the installed version (call as this one exits).
pub fn restart() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

fn install(bytes: &[u8]) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let new = sibling(&exe, ".new");
    let old = sibling(&exe, ".old");
    std::fs::write(&new, bytes)?;
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old)?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        // Put the running one back rather than leave no exe at all.
        let _ = std::fs::rename(&old, &exe);
        return Err(e);
    }
    Ok(())
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// `a` is a later version than `b` (dotted numbers; anything else counts as 0).
fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.split(['.', '-'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parts(a) > parts(b)
}

fn get_json(url: &str) -> Option<Value> {
    serde_json::from_slice(&get_bytes(url, 4 * 1024 * 1024)?).ok()
}

fn get_bytes(url: &str, limit: u64) -> Option<Vec<u8>> {
    let mut r = music_core::net::agent()
        .get(url)
        .header("User-Agent", UA)
        .header(
            "Accept",
            "application/vnd.github+json, application/octet-stream",
        )
        .call()
        .ok()?;
    if r.status().as_u16() != 200 {
        log::debug!("update check: HTTP {}", r.status());
        return None;
    }
    r.body_mut().with_config().limit(limit).read_to_vec().ok()
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn versions() {
        assert!(newer("0.2.0", "0.1.0"));
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
    }
}
