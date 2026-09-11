//! Where recordings land on disk.
//!
//! ```text
//! $XDG_DATA_HOME/voice-commander/
//! └── recordings/2026/09/11/20260911T144812Z-dictate-2hc8b/
//!     ├── audio.wav
//!     └── session.json
//! ```
//!
//! Dated directories rather than one flat pile: a year of push-to-talk is tens of thousands
//! of sessions, and a directory with that many entries is unpleasant to work with from a
//! shell and slow to list.

use std::path::{Path, PathBuf};

use anyhow::Context;
use time::OffsetDateTime;
use vc_core::session::{SessionId, SessionRecord};

/// The directory tree for recordings.
#[derive(Debug, Clone)]
pub struct Storage {
    root: PathBuf,
}

impl Storage {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create and return the directory for one session.
    pub fn session_dir(&self, id: &SessionId, at: OffsetDateTime) -> anyhow::Result<PathBuf> {
        let utc = at.to_offset(time::UtcOffset::UTC);
        let dir = self
            .root
            .join("recordings")
            .join(format!("{:04}", utc.year()))
            .join(format!("{:02}", u8::from(utc.month())))
            .join(format!("{:02}", utc.day()))
            .join(id.as_str());

        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating session directory {}", dir.display()))?;
        Ok(dir)
    }

    /// Create the data root with permissions that keep it to its owner.
    ///
    /// These are recordings of the user speaking. On a shared machine the default umask is
    /// not a strong enough reason for anyone else to be able to read them.
    pub fn prepare(&self) -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting permissions on {}", self.root.display()))?;
        Ok(())
    }
}

/// Write `session.json` beside a recording.
pub fn write_record(dir: &Path, record: &SessionRecord) -> anyhow::Result<PathBuf> {
    let path = dir.join("session.json");
    // Pretty-printed on purpose: this file is read by people debugging why a callback did
    // not fire, and by shell scripts that receive it on stdin.
    let json = serde_json::to_string_pretty(record).context("serializing the session record")?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn sessions_are_filed_by_date() {
        let dir = tempfile::tempdir().expect("temp dir");
        let storage = Storage::new(dir.path().to_owned());
        let id = SessionId::from_raw("20260911T144812Z-dictate-2hc8b");

        let path = storage
            .session_dir(&id, datetime!(2026-09-11 14:48:12 UTC))
            .expect("create");

        assert!(path.ends_with("recordings/2026/09/11/20260911T144812Z-dictate-2hc8b"));
        assert!(path.is_dir());
    }

    #[test]
    fn the_date_directory_uses_utc_not_local_time() {
        // Otherwise a listing interleaves timezones and stops being sortable.
        let dir = tempfile::tempdir().expect("temp dir");
        let storage = Storage::new(dir.path().to_owned());
        let id = SessionId::from_raw("x");

        // 00:30 on the 12th in +02:00 is still the 11th in UTC.
        let path = storage
            .session_dir(&id, datetime!(2026-09-12 00:30:00 +2))
            .expect("create");
        assert!(path.to_string_lossy().contains("/2026/09/11/"), "{path:?}");
    }

    #[test]
    fn the_data_root_is_not_readable_by_others() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let storage = Storage::new(dir.path().join("data"));
        storage.prepare().expect("prepare");

        let mode = std::fs::metadata(storage.root())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "recordings were left at mode {mode:o}");
    }

    #[test]
    fn creating_a_session_directory_twice_is_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let storage = Storage::new(dir.path().to_owned());
        let id = SessionId::from_raw("same");
        let at = datetime!(2026-09-11 14:48:12 UTC);

        let first = storage.session_dir(&id, at).expect("first");
        let second = storage.session_dir(&id, at).expect("second");
        assert_eq!(first, second);
    }
}
