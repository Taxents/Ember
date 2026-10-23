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

/// Write a file atomically (temp file + rename), so a crash never leaves half a file.
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}
