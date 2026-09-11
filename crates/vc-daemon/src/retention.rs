//! Cleaning up old recordings.
//!
//! Left alone, this directory grows forever: a year of push-to-talk is tens of thousands of
//! sessions. Both limits default to off, because deleting someone's recordings without being
//! asked is worse than using disk.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tracing::{debug, info, warn};
use vc_core::config::StorageConfig;

/// What a sweep removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub removed: usize,
    pub bytes_freed: u64,
}

/// One recording on disk.
#[derive(Debug)]
struct Recording {
    dir: PathBuf,
    modified: SystemTime,
    bytes: u64,
}

/// Remove recordings that exceed the configured limits.
///
/// Age first, then total size, oldest first. Running age before size means a size sweep only
/// has to deal with what age did not already take, which is the order a user would expect if
/// they set both.
pub fn sweep(root: &Path, config: &StorageConfig, now: SystemTime) -> Swept {
    let mut swept = Swept::default();
    if config.max_age_days == 0 && config.max_total_bytes == 0 {
        return swept;
    }

    let mut recordings = collect(&root.join("recordings"));
    if recordings.is_empty() {
        return swept;
    }
    recordings.sort_by_key(|recording| recording.modified);

    if config.max_age_days > 0 {
        let cutoff = Duration::from_secs(u64::from(config.max_age_days) * 24 * 60 * 60);
        recordings.retain(|recording| {
            let age = now
                .duration_since(recording.modified)
                .unwrap_or(Duration::ZERO);
            if age > cutoff {
                if remove(&recording.dir) {
                    swept.removed += 1;
                    swept.bytes_freed += recording.bytes;
                }
                false
            } else {
                true
            }
        });
    }

    if config.max_total_bytes > 0 {
        let mut total: u64 = recordings.iter().map(|recording| recording.bytes).sum();
        // Oldest first, since `recordings` is sorted by modification time.
        for recording in &recordings {
            if total <= config.max_total_bytes {
                break;
            }
            if remove(&recording.dir) {
                total -= recording.bytes.min(total);
                swept.removed += 1;
                swept.bytes_freed += recording.bytes;
            }
        }
    }

    if swept.removed > 0 {
        info!(
            removed = swept.removed,
            mb = swept.bytes_freed / 1_000_000,
            "pruned old recordings"
        );
    }
    swept
}

/// Every session directory under `recordings`, with its size and age.
///
/// A session directory is one containing `session.json`. Anything else in the tree is left
/// alone: this deletes the user's recordings, and it is not the place to be clever about
/// files it does not recognise.
fn collect(root: &Path) -> Vec<Recording> {
    let mut found = Vec::new();
    walk(root, &mut found, 0);
    found
}

fn walk(dir: &Path, found: &mut Vec<Recording>, depth: usize) {
    // The layout is recordings/YYYY/MM/DD/<session>, so nothing of interest is deeper.
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("session.json").is_file() {
            let (bytes, modified) = measure(&path);
            found.push(Recording {
                dir: path,
                modified,
                bytes,
            });
        } else {
            walk(&path, found, depth + 1);
        }
    }
}

/// Total size and newest modification time of a session directory.
fn measure(dir: &Path) -> (u64, SystemTime) {
    let mut bytes = 0;
    let mut modified = SystemTime::UNIX_EPOCH;

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                bytes += meta.len();
                if let Ok(time) = meta.modified() {
                    modified = modified.max(time);
                }
            }
        }
    }
    (bytes, modified)
}

fn remove(dir: &Path) -> bool {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {
            debug!(path = %dir.display(), "removed a recording");
            true
        }
        Err(error) => {
            warn!(%error, path = %dir.display(), "could not remove a recording");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(days: u32, bytes: u64) -> StorageConfig {
        StorageConfig {
            dir: None,
            max_age_days: days,
            max_total_bytes: bytes,
            keep_audio_after_transcribe: true,
        }
    }

    /// Create a session directory `age_days` old, holding `bytes` of audio.
    fn make(root: &Path, name: &str, age_days: u64, bytes: usize) -> PathBuf {
        let dir = root.join("recordings/2026/09/11").join(name);
        std::fs::create_dir_all(&dir).expect("create");
        std::fs::write(dir.join("session.json"), "{}").expect("write");
        std::fs::write(dir.join("audio.wav"), vec![0u8; bytes]).expect("write");

        let when = SystemTime::now() - Duration::from_secs(age_days * 24 * 60 * 60);
        let when = filetime::FileTime::from_system_time(when);
        for file in ["session.json", "audio.wav"] {
            filetime::set_file_mtime(dir.join(file), when).expect("set mtime");
        }
        dir
    }

    #[test]
    fn nothing_is_removed_when_both_limits_are_off() {
        // Deleting someone's recordings without being asked is worse than using disk.
        let root = tempfile::tempdir().expect("temp dir");
        let kept = make(root.path(), "old", 400, 100);

        assert_eq!(
            sweep(root.path(), &config(0, 0), SystemTime::now()),
            Swept::default()
        );
        assert!(kept.exists());
    }

    #[test]
    fn recordings_past_the_age_limit_are_removed() {
        let root = tempfile::tempdir().expect("temp dir");
        let old = make(root.path(), "old", 40, 100);
        let recent = make(root.path(), "recent", 5, 100);

        let swept = sweep(root.path(), &config(30, 0), SystemTime::now());

        assert_eq!(swept.removed, 1);
        assert!(!old.exists());
        assert!(recent.exists(), "a recent recording was taken too");
    }

    #[test]
    fn the_size_limit_removes_the_oldest_first() {
        let root = tempfile::tempdir().expect("temp dir");
        let oldest = make(root.path(), "a", 30, 1_000);
        let middle = make(root.path(), "b", 20, 1_000);
        let newest = make(root.path(), "c", 10, 1_000);

        // Room for roughly two of them.
        let swept = sweep(root.path(), &config(0, 2_200), SystemTime::now());

        assert_eq!(swept.removed, 1);
        assert!(!oldest.exists(), "the oldest should go first");
        assert!(middle.exists());
        assert!(newest.exists());
    }

    #[test]
    fn both_limits_apply_together() {
        let root = tempfile::tempdir().expect("temp dir");
        let ancient = make(root.path(), "ancient", 100, 1_000);
        let old = make(root.path(), "old", 20, 5_000);
        let recent = make(root.path(), "recent", 1, 1_000);

        let swept = sweep(root.path(), &config(30, 2_000), SystemTime::now());

        assert_eq!(swept.removed, 2);
        assert!(!ancient.exists(), "removed by age");
        assert!(!old.exists(), "removed by size");
        assert!(recent.exists());
    }

    #[test]
    fn a_directory_that_is_not_a_session_is_left_alone() {
        // This deletes the user's recordings; it is not the place to be clever about files
        // it does not recognise.
        let root = tempfile::tempdir().expect("temp dir");
        let stray = root.path().join("recordings/2026/09/11/notes");
        std::fs::create_dir_all(&stray).expect("create");
        std::fs::write(stray.join("scratch.txt"), "mine").expect("write");

        sweep(root.path(), &config(1, 1), SystemTime::now());
        assert!(stray.join("scratch.txt").exists());
    }

    #[test]
    fn an_absent_recordings_directory_is_not_an_error() {
        let root = tempfile::tempdir().expect("temp dir");
        assert_eq!(
            sweep(root.path(), &config(30, 1_000), SystemTime::now()),
            Swept::default()
        );
    }

    #[test]
    fn a_size_limit_that_everything_already_fits_under_removes_nothing() {
        let root = tempfile::tempdir().expect("temp dir");
        let kept = make(root.path(), "a", 1, 100);
        assert_eq!(
            sweep(root.path(), &config(0, 1_000_000), SystemTime::now()).removed,
            0
        );
        assert!(kept.exists());
    }
}
