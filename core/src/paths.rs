//! Where the app keeps its files.

use std::path::PathBuf;

const APP_DIR: &str = "MusicApp";

/// `%APPDATA%\MusicApp`: settings, session, queue.
pub fn config_dir() -> PathBuf {
    ensure(
        dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(APP_DIR),
    )
}

/// `%LOCALAPPDATA%\MusicApp\cache`: anything that can be thrown away.
pub fn cache_dir() -> PathBuf {
    ensure(
        dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(APP_DIR)
            .join("cache"),
    )
}

fn ensure(p: PathBuf) -> PathBuf {
    let _ = std::fs::create_dir_all(&p);
    p
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write a file atomically (temp file + rename), so a crash never leaves half a file. The temp
/// name is unique to the call, so two threads writing the same file don't share one temp.
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(
        ".{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(name);
    if let Err(e) = std::fs::write(&tmp, data).and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}
