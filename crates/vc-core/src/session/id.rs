//! Session identity.

use serde::{Deserialize, Serialize};
use std::fmt;
use time::OffsetDateTime;

/// A session's identifier, which is also the name of its directory on disk.
///
/// The format — `20260911T144812Z-dictate-3f9a2b` — is sortable by time, says which profile
/// produced it without opening anything, and carries enough entropy that two recordings
/// started in the same second cannot collide. That matters more than it sounds: the cooldown
/// window means a user can genuinely start two sessions inside one second.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Build an id from the moment the session began and the profile that started it.
    ///
    /// `nonce` distinguishes sessions that share a second; the daemon derives it from the
    /// sub-second part of the clock so that no random source is needed.
    pub fn new(started_at: OffsetDateTime, profile: &str, nonce: u32) -> Self {
        let utc = started_at.to_offset(time::UtcOffset::UTC);
        let stamp = format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            utc.year(),
            u8::from(utc.month()),
            utc.day(),
            utc.hour(),
            utc.minute(),
            utc.second(),
        );
        Self(format!("{stamp}-{}-{}", sanitize(profile), base36(nonce)))
    }

    /// Reconstruct an id from an existing directory name.
    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reduce a profile name to something safe in a path and unambiguous in an id.
///
/// Profile names come from a config file, so they can contain anything. A name with a `/`
/// in it must not be able to write a session outside the recordings directory.
fn sanitize(profile: &str) -> String {
    let cleaned: String = profile
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if cleaned.is_empty() {
        "profile".to_owned()
    } else {
        cleaned
    }
}

fn base36(mut value: u32) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_owned();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_else(|_| "0".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn ids_are_sortable_by_time() {
        let earlier = SessionId::new(datetime!(2026-09-11 14:48:12 UTC), "dictate", 1);
        let later = SessionId::new(datetime!(2026-09-11 14:48:13 UTC), "dictate", 1);
        assert!(earlier < later, "{earlier} should sort before {later}");
    }

    #[test]
    fn ids_carry_the_profile_and_a_utc_stamp() {
        let id = SessionId::new(datetime!(2026-09-11 14:48:12 UTC), "dictate", 0x3f9a2b);
        assert_eq!(id.as_str(), "20260911T144812Z-dictate-2hc8b");
    }

    #[test]
    fn local_times_are_normalised_to_utc() {
        let utc = SessionId::new(datetime!(2026-09-11 14:48:12 UTC), "p", 1);
        let offset = SessionId::new(datetime!(2026-09-11 16:48:12 +2), "p", 1);
        // Otherwise a directory listing would interleave two timezones and stop sorting.
        assert_eq!(utc, offset);
    }

    #[test]
    fn sessions_in_the_same_second_do_not_collide() {
        let at = datetime!(2026-09-11 14:48:12 UTC);
        assert_ne!(SessionId::new(at, "p", 1), SessionId::new(at, "p", 2));
    }

    #[test]
    fn a_profile_name_cannot_escape_the_recordings_directory() {
        let id = SessionId::new(datetime!(2026-09-11 14:48:12 UTC), "../../etc", 1);
        assert!(!id.as_str().contains('/'));
        assert!(!id.as_str().contains(".."));
    }

    #[test]
    fn an_unusable_profile_name_still_yields_an_id() {
        let id = SessionId::new(datetime!(2026-09-11 14:48:12 UTC), "///", 1);
        assert!(id.as_str().contains("___"));
    }
}
