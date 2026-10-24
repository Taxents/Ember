//! Where the app keeps its files.

use std::path::PathBuf;
use std::sync::OnceLock;

const APP_DIR: &str = "Ember";
/// The folder name before the app was called Ember; moved to `APP_DIR` on first start.
const OLD_DIR: &str = "MusicApp";

/// `%APPDATA%\Ember`: settings, session, queue.
pub fn config_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| home(dirs::config_dir().unwrap_or_else(std::env::temp_dir)));
    ensure(dir.clone())
}

/// `%LOCALAPPDATA%\Ember\cache`: anything that can be thrown away.
pub fn cache_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| home(dirs::cache_dir().unwrap_or_else(std::env::temp_dir)));
    ensure(dir.join("cache"))
}

/// The app's folder under `base`. A `MusicApp` folder from before the rename becomes `Ember`;
/// if it can't be moved (an older copy of the app still has files open), it stays in use, so
/// nothing seems lost.
fn home(base: PathBuf) -> PathBuf {
    let new = base.join(APP_DIR);
    let old = base.join(OLD_DIR);
    if new.exists() || !old.exists() {
        return new;
    }
    match std::fs::rename(&old, &new) {
        Ok(()) => new,
        Err(_) => old,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_folder_moves_once() {
        let base = std::env::temp_dir().join(format!("ember-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(OLD_DIR)).unwrap();
        std::fs::write(base.join(OLD_DIR).join("settings.json"), b"{}").unwrap();
        let dir = home(base.clone());
        assert_eq!(dir, base.join(APP_DIR));
        assert!(dir.join("settings.json").exists());
        assert!(!base.join(OLD_DIR).exists());
        // Already moved: stays put, even if an old copy made a new MusicApp since.
        std::fs::create_dir_all(base.join(OLD_DIR)).unwrap();
        assert_eq!(home(base.clone()), base.join(APP_DIR));
        let _ = std::fs::remove_dir_all(&base);
    }
}
