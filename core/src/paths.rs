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
/// nothing seems lost. An `Ember` folder that was already there (from some other build) is
/// renamed aside first, never deleted. A marker in the moved folder keeps this to once, so a
/// `MusicApp` an old copy makes later is left alone.
fn home(base: PathBuf) -> PathBuf {
    const MOVED: &str = "moved-from-MusicApp";
    let new = base.join(APP_DIR);
    let old = base.join(OLD_DIR);
    if !old.exists() || new.join(MOVED).exists() {
        return new;
    }
    if new.exists() {
        let aside = base.join(format!("{APP_DIR} (before {OLD_DIR} moved in)"));
        if aside.exists() || std::fs::rename(&new, &aside).is_err() {
            return old;
        }
    }
    match std::fs::rename(&old, &new) {
        Ok(()) => {
            let _ = std::fs::write(new.join(MOVED), b"");
            new
        }
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
        assert!(base.join(OLD_DIR).exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_stray_ember_folder_is_set_aside() {
        let base = std::env::temp_dir().join(format!("ember-paths-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(OLD_DIR)).unwrap();
        std::fs::create_dir_all(base.join(APP_DIR)).unwrap();
        std::fs::write(base.join(OLD_DIR).join("settings.json"), b"current").unwrap();
        std::fs::write(base.join(APP_DIR).join("settings.json"), b"stray").unwrap();
        let dir = home(base.clone());
        assert_eq!(dir, base.join(APP_DIR));
        assert_eq!(
            std::fs::read(dir.join("settings.json")).unwrap(),
            b"current"
        );
        let aside = base.join(format!("{APP_DIR} (before {OLD_DIR} moved in)"));
        assert_eq!(
            std::fs::read(aside.join("settings.json")).unwrap(),
            b"stray"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
