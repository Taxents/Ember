//! Updates from GitHub releases. A release is a `vX.Y.Z` tag with three assets: `ember.exe`,
//! `ember.exe.minisig` (its minisign signature, made by the release workflow with the key whose
//! public half is `PUBLIC_KEY`) and `ember.exe.sha256` (its SHA-256 in hex).
//!
//! Checking only looks: it reads the release list and reports `Available`, and the sidebar says so.
//! Nothing is downloaded until the user chooses "Download and install" (`install_now`). Then the
//! exe is downloaded beside the running one (resuming a `.part` left by an earlier try), its
//! signature and hash are checked, and it's swapped in: Windows lets a running exe be renamed, so
//! the old one becomes `ember.exe.old` (deleted on the next start) and the new one takes its name.
//! It runs from the next launch; the app offers to restart into it.
//!
//! The release repo must be public: a private one can't be read without a token, and no token
//! ships in the app. Until then the check simply finds nothing.

use crate::{Ui, with};
use minisign_verify::{PublicKey, Signature};
use serde_json::Value;
use sha2::{Digest, Sha256};
use slint::{ComponentHandle, SharedString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Where releases are published ("owner/name").
const REPO: &str = "Taxents/Ember";
const UA: &str = concat!("Ember/", env!("CARGO_PKG_VERSION"));
const ASSET: &str = "ember.exe";
/// The largest exe accepted.
const MAX_EXE: u64 = 200 * 1024 * 1024;

/// The minisign public key releases are signed with (the second line of `ember.pub`; its secret
/// half is the `MINISIGN_SECRET_KEY` repository secret). See `tools/release.ps1`.
const PUBLIC_KEY: &str = "RWRF2StWuqEeLNcq2Lp7wdDSfXBMRGlkoEUGuIkltIfYFw9E3tr26y9y";

/// Deletes what the last update left behind. Call once at start.
pub fn clean_up() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(sibling(&exe, ".old"));
        let _ = std::fs::remove_file(sibling(&exe, ".new"));
    }
}

/// Where an update is.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Checking,
    UpToDate,
    /// A newer version is out; nothing is downloaded until the user asks.
    Available(String),
    /// Downloading the version, this many percent in.
    Downloading(String, u8),
    /// A newer version is installed and runs from the next start.
    Ready(String),
    /// The download's signature or hash didn't match (or no key is built in): not installed.
    Unverified(String),
    /// The download failed (offline, a broken connection); trying again resumes it.
    DownloadFailed(String),
    /// The check failed (offline, GitHub unreachable).
    Failed,
    /// Development builds aren't installed, so they don't update.
    Off,
}

/// The newest release the last check found.
#[derive(Clone)]
struct Release {
    version: String,
    exe_url: String,
    exe_size: u64,
    sig_url: String,
    hash_url: Option<String>,
}

static FOUND: Mutex<Option<Release>> = Mutex::new(None);

/// Looks for a newer release, telling `report` each step. Downloads nothing. Blocks (network);
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
        // It names files: digits and dots only.
        .filter(|v| plain_version(v))
    else {
        // No release yet.
        return done(Status::UpToDate);
    };
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return done(Status::UpToDate);
    }
    let asset = |name: &str| {
        release["assets"]
            .as_array()?
            .iter()
            .find(|a| a["name"].as_str() == Some(name))
    };
    let url = |a: &Value| a["browser_download_url"].as_str().map(str::to_string);
    let found = (|| {
        let exe = asset(ASSET)?;
        Some(Release {
            version: version.clone(),
            exe_url: url(exe)?,
            exe_size: exe["size"].as_u64().unwrap_or(0),
            sig_url: url(asset(&format!("{ASSET}.minisig"))?)?,
            hash_url: asset(&format!("{ASSET}.sha256")).and_then(url),
        })
    })();
    let Some(found) = found else {
        // An unsigned release (from before signing) is never offered.
        log::info!("update {version}: no signed ember.exe in the release");
        return done(Status::UpToDate);
    };
    *FOUND.lock().unwrap() = Some(found);
    done(Status::Available(version))
}

/// Downloads `version` (as found by the last check), verifies it and swaps it in, telling `report`
/// each step. Blocks (network); call on a worker.
pub fn download_and_install(version: &str, report: &dyn Fn(Status)) -> Status {
    let done = |s: Status| {
        report(s.clone());
        s
    };
    let release = FOUND.lock().unwrap().clone();
    let Some(release) = release.filter(|r| r.version == version && plain_version(version)) else {
        return done(Status::DownloadFailed(version.to_string()));
    };
    let unverified = |why: &str| {
        log::warn!("update {version}: {why}; not installed");
        done(Status::Unverified(version.to_string()))
    };
    // Refuse before downloading anything when there's no key to check against.
    let key = match public_key(PUBLIC_KEY) {
        Ok(k) => k,
        Err(e) => return unverified(&e),
    };
    let Ok(exe) = std::env::current_exe() else {
        return done(Status::DownloadFailed(version.to_string()));
    };
    let Some(sig) = get_bytes(&release.sig_url, 4096).and_then(|b| String::from_utf8(b).ok())
    else {
        return done(Status::DownloadFailed(version.to_string()));
    };
    let want_hash = match &release.hash_url {
        Some(u) => match get_bytes(u, 1024).and_then(|b| String::from_utf8(b).ok()) {
            Some(h) => h.split_whitespace().next().map(str::to_lowercase),
            None => return done(Status::DownloadFailed(version.to_string())),
        },
        None => None,
    };
    let part = sibling(&exe, &format!(".{version}.part"));
    remove_other_parts(&exe, &part);
    if let Err(e) = download(&release, &part, report) {
        log::warn!("update {version}: download: {e}");
        return done(Status::DownloadFailed(version.to_string()));
    }
    let Ok(bytes) = std::fs::read(&part) else {
        return done(Status::DownloadFailed(version.to_string()));
    };
    if let Err(e) = verify(&key, &bytes, &sig, version) {
        // A bad file isn't resumed from; the next try starts over.
        let _ = std::fs::remove_file(&part);
        return unverified(&e);
    }
    if let Some(want) = want_hash
        && hex(&Sha256::digest(&bytes)) != want
    {
        let _ = std::fs::remove_file(&part);
        return unverified("hash mismatch");
    }
    drop(bytes);
    match install(&exe, &part) {
        Ok(()) => {
            log::info!("update {version} installed; runs from the next start");
            done(Status::Ready(version.to_string()))
        }
        Err(e) => {
            log::warn!("update {version}: {e}");
            done(Status::DownloadFailed(version.to_string()))
        }
    }
}

/// Set when the user asked to restart into the update; read once the window has closed.
pub static RESTART: AtomicBool = AtomicBool::new(false);

type Job = Box<dyn FnOnce() -> Status + Send>;

/// One check or download at a time. Set and cleared under `QUEUED`'s lock.
static BUSY: AtomicBool = AtomicBool::new(false);
/// A download asked for while something else ran; it runs next.
static QUEUED: Mutex<Option<Job>> = Mutex::new(None);
/// The version installed this session; nothing more is checked or downloaded after it.
static INSTALLED: Mutex<Option<String>> = Mutex::new(None);

/// Runs `job` on the update thread. While another job runs it's dropped, or with `queue` kept
/// to run after it. False: it will never run.
fn run(job: Job, queue: bool) -> bool {
    let mut queued = QUEUED.lock().unwrap();
    if BUSY.load(Ordering::Acquire) {
        if queue {
            *queued = Some(job);
        }
        return queue;
    }
    BUSY.store(true, Ordering::Release);
    let spawned = std::thread::Builder::new()
        .name("update".into())
        .spawn(move || {
            let mut job = job;
            loop {
                if let Status::Ready(v) = job() {
                    *INSTALLED.lock().unwrap() = Some(v);
                }
                let mut queued = QUEUED.lock().unwrap();
                match queued.take() {
                    Some(next) => job = next,
                    None => {
                        BUSY.store(false, Ordering::Release);
                        return;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("update: no thread: {e}");
        BUSY.store(false, Ordering::Release);
        return false;
    }
    true
}

fn installed() -> Option<String> {
    INSTALLED.lock().unwrap().clone()
}

/// Checks once on its own thread, unless a check or download is running. `report` hears each
/// step (on that thread: hand it to the UI thread yourself). Once an update is installed it
/// just says so.
pub fn check_now(report: impl Fn(Status) + Send + 'static) {
    if let Some(v) = installed() {
        return report(Status::Ready(v));
    }
    run(Box::new(move || check(&report)), false);
}

/// Downloads and installs `version` on its own thread; only ever on the user's say-so. Asked
/// while a check runs, it starts as soon as that's done.
pub fn install_now(version: String, report: impl Fn(Status) + Send + Clone + 'static) {
    if let Some(v) = installed() {
        return report(Status::Ready(v));
    }
    // Shown at once, even when it has to wait for a check to finish.
    report(Status::Downloading(version.clone(), 0));
    let r = report.clone();
    let v = version.clone();
    if !run(
        Box::new(move || {
            // Installed by an earlier queued click meanwhile.
            match installed() {
                Some(v) => {
                    r(Status::Ready(v.clone()));
                    Status::Ready(v)
                }
                None => download_and_install(&v, &r),
            }
        }),
        true,
    ) {
        report(Status::DownloadFailed(version));
    }
}

/// Checks right away at start, then every six hours while the app runs. Only checks.
pub fn watch(report: impl Fn(Status) + Send + Sync + Clone + 'static) {
    let _ = std::thread::Builder::new()
        .name("update-timer".into())
        .spawn(move || {
            loop {
                check_now(report.clone());
                std::thread::sleep(Duration::from_secs(6 * 3600));
            }
        });
}

/// Starts the installed version (call as this one exits).
pub fn restart() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

/// Hooks up the sidebar banner's buttons.
pub fn wire(g: &Ui) {
    g.on_update_install(|| {
        let v = with(|a| a.ui().global::<Ui>().get_update_available().to_string());
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            install_now(v, show);
        }
    });
    g.on_update_dismiss(|| {
        with(|a| {
            let ui = a.ui();
            let g = ui.global::<Ui>();
            let v = g.get_update_available();
            if !v.is_empty() {
                let _ = std::fs::write(dismissed_path(), v.as_bytes());
            }
            g.set_update_dismissed(true);
            g.set_update_error(SharedString::new());
        });
    });
}

/// Shows an update's progress: the sidebar's banner and Settings' line. Any thread.
pub fn show(s: Status) {
    let _ = slint::invoke_from_event_loop(move || {
        with(|a| {
            let ui = a.ui();
            let g = ui.global::<Ui>();
            let none = SharedString::new;
            let status = match &s {
                Status::Checking | Status::Off | Status::Failed => return,
                Status::UpToDate => {
                    g.set_update_available(none());
                    "Up to date".to_string()
                }
                Status::Available(v) => {
                    // A later check of the same version leaves a download or its failure shown.
                    let same = g.get_update_available() == v.as_str();
                    if !(same && (!g.get_update_progress().is_empty())) {
                        if !same {
                            g.set_update_error(none());
                            g.set_update_unverified(false);
                        }
                        g.set_update_available(v.into());
                        g.set_update_dismissed(dismissed().as_deref() == Some(v.as_str()));
                    }
                    format!("Ember {v} is available")
                }
                Status::Downloading(v, pct) => {
                    g.set_update_error(none());
                    g.set_update_unverified(false);
                    g.set_update_progress(format!("Downloading Ember {v}… {pct}%").into());
                    format!("Downloading Ember {v}")
                }
                Status::Ready(v) => {
                    g.set_update_available(none());
                    g.set_update_progress(none());
                    g.set_update_ready(v.into());
                    format!("Ember {v} is installed")
                }
                Status::Unverified(v) => {
                    g.set_update_progress(none());
                    g.set_update_dismissed(false);
                    g.set_update_unverified(true);
                    g.set_update_error(format!("Ember {v} couldn't be verified").into());
                    "The update couldn't be verified".to_string()
                }
                Status::DownloadFailed(v) => {
                    g.set_update_progress(none());
                    g.set_update_dismissed(false);
                    g.set_update_error(format!("Couldn't download Ember {v}").into());
                    "The update didn't download".to_string()
                }
            };
            g.set_update_status(status.into());
        });
    });
}

/// The version whose banner was put away.
fn dismissed() -> Option<String> {
    let s = std::fs::read_to_string(dismissed_path()).ok()?;
    Some(s.trim().to_string())
}

fn dismissed_path() -> PathBuf {
    music_core::paths::config_dir().join("update-dismissed")
}

/// Fetches the release exe into `part`, carrying on from what's already there.
fn download(release: &Release, part: &Path, report: &dyn Fn(Status)) -> Result<(), String> {
    let mut have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if release.exe_size > 0 && have == release.exe_size {
        return Ok(());
    }
    if release.exe_size > 0 && have > release.exe_size {
        // Not a part of this file.
        let _ = std::fs::remove_file(part);
        have = 0;
    }
    // Large and possibly slow: no limit on the whole body, only on each read going quiet.
    let mut rb = music_core::net::agent()
        .get(&release.exe_url)
        .header("User-Agent", UA)
        .header("Accept", "application/octet-stream");
    if have > 0 {
        rb = rb.header("Range", format!("bytes={have}-"));
    }
    let mut r = rb
        .config()
        .timeout_global(None)
        .timeout_per_call(None)
        .timeout_recv_body(None)
        .timeout_per_read(Some(Duration::from_secs(15)))
        .build()
        .call()
        .map_err(|e| e.to_string())?;
    let (mut file, mut got) = match r.status().as_u16() {
        206 if have > 0 => (
            std::fs::OpenOptions::new()
                .append(true)
                .open(part)
                .map_err(|e| e.to_string())?,
            have,
        ),
        // The server ignored the range (or there was nothing yet): start over.
        200 => (std::fs::File::create(part).map_err(|e| e.to_string())?, 0),
        // The part doesn't fit the file (it changed, or the part is bad): once more from zero.
        416 if have > 0 => {
            drop(r);
            std::fs::remove_file(part).map_err(|e| e.to_string())?;
            return download(release, part, report);
        }
        s => return Err(format!("HTTP {s}")),
    };
    let total = release.exe_size;
    let mut body = r.body_mut().with_config().limit(MAX_EXE).reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut shown = u8::MAX;
    loop {
        let n = body.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        got += n as u64;
        if got > MAX_EXE {
            return Err("too large".into());
        }
        let pct = (got * 100).checked_div(total).unwrap_or(0).min(100) as u8;
        if pct != shown {
            shown = pct;
            report(Status::Downloading(release.version.clone(), pct));
        }
    }
    file.sync_all().map_err(|e| e.to_string())?;
    if total > 0 && got != total {
        return Err(format!("got {got} of {total} bytes"));
    }
    Ok(())
}

/// The built-in key, or why there's none.
fn public_key(b64: &str) -> Result<PublicKey, String> {
    if b64.starts_with("PLACEHOLDER") {
        return Err("no update signing key is built in".into());
    }
    PublicKey::from_base64(b64).map_err(|e| format!("bad public key: {e}"))
}

/// Checks `sig` (a `.minisig` file) is `key`'s signature of `data`, made for `version`: its trusted
/// comment must name it (`version:1.2.3`), so an older signed exe can't pass as a newer one.
fn verify(key: &PublicKey, data: &[u8], sig: &str, version: &str) -> Result<(), String> {
    let sig = Signature::decode(sig).map_err(|e| format!("bad signature file: {e}"))?;
    key.verify(data, &sig, false)
        .map_err(|e| format!("signature: {e}"))?;
    let want = format!("version:{version}");
    if !sig.trusted_comment().split_whitespace().any(|w| w == want) {
        return Err(format!(
            "signature is for {:?}, not {version}",
            sig.trusted_comment()
        ));
    }
    Ok(())
}

/// Swaps the verified download at `new` in for `exe`.
fn install(exe: &Path, new: &Path) -> std::io::Result<()> {
    let old = sibling(exe, ".old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old)?;
    if let Err(e) = std::fs::rename(new, exe) {
        // Put the running one back rather than leave no exe at all.
        let _ = std::fs::rename(&old, exe);
        return Err(e);
    }
    Ok(())
}

/// Deletes partial downloads of other versions.
fn remove_other_parts(exe: &Path, keep: &Path) {
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name()) else {
        return;
    };
    let name = name.to_string_lossy().to_string();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with(&name) && n.ends_with(".part") && e.path() != keep {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `v` is `1.2.3`: three dot-separated runs of digits, nothing else.
fn plain_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| (1..=9).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
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
    use super::*;

    // A throwaway key made for these tests; its secret half was never kept.
    const TEST_KEY: &str = "RWQBAgMEBQYHCI1gz4QdLQBO5ogfRRBa579KjhC7L1+0gVxCHDsU32hS";
    const TEST_DATA: &[u8] = b"ember test update";
    const TEST_SIG: &str = "untrusted comment: signature from minisign secret key
RUQBAgMEBQYHCBaovTM689HYcem5/1tUxiWPa0k0b7gLcBN5uxWgBGhqimlMnTcfbPcW/35WReVr1nJCjkQGosM+pX7W5HQ/1wc=
trusted comment: timestamp:1761300000\tversion:1.2.3 file:ember.exe
sMIVovIC6uCpVtbX+JtFy05OQWaoX2TCKIylUTu65dz1kYWDLHdtISlTq3rkJfee1yV/k5SCFDz0s5Vd/sKQBg==";

    #[test]
    fn versions() {
        assert!(newer("0.2.0", "0.1.0"));
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn plain_versions() {
        assert!(plain_version("1.2.3"));
        assert!(plain_version("0.10.0"));
        assert!(!plain_version("1.2"));
        assert!(!plain_version("1.2.3-beta"));
        assert!(!plain_version("1.2.3/../x"));
        assert!(!plain_version("1..3"));
        assert!(!plain_version("1.2.3.4"));
        assert!(!plain_version("1.2.99999999999"));
    }

    #[test]
    fn placeholder_key_refuses() {
        assert!(public_key("PLACEHOLDER-run-minisign-G").is_err());
    }

    #[test]
    fn release_key_is_a_key() {
        assert!(public_key(PUBLIC_KEY).is_ok());
    }

    #[test]
    fn signature() {
        let key = public_key(TEST_KEY).unwrap();
        assert_eq!(verify(&key, TEST_DATA, TEST_SIG, "1.2.3"), Ok(()));
        // Another version, other bytes, a tampered signature.
        assert!(verify(&key, TEST_DATA, TEST_SIG, "1.2.4").is_err());
        assert!(verify(&key, b"ember test updatf", TEST_SIG, "1.2.3").is_err());
        let bad = TEST_SIG.replacen("RUQB", "RUQC", 1);
        assert!(verify(&key, TEST_DATA, &bad, "1.2.3").is_err());
        assert!(verify(&key, TEST_DATA, "not a signature", "1.2.3").is_err());
    }
}
