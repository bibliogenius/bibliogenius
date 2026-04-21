//! Relay mailbox session provenance flag.
//!
//! Tracks whether the current process has successfully created or recreated
//! a relay mailbox since startup. The `my_relay_config` table is a singleton
//! row — restoring a stale `mailbox_uuid` from it after the hub has purged
//! the matching mailbox (device-fingerprint dedup, admin purge, orphan
//! cleanup) produces silent "deposit to non-existent mailbox" warnings on
//! the hub that never surface on the client.
//!
//! The flag is process-global because `my_relay_config` is a singleton and
//! the two `HubDirectoryService` instances in the binary (FFI `OnceLock`
//! and `AppState`) must agree. It is purely diagnostic: the mailbox is
//! still sent to the hub either way.

use std::sync::atomic::{AtomicBool, Ordering};

static MAILBOX_CREATED_THIS_SESSION: AtomicBool = AtomicBool::new(false);

/// Mark the relay mailbox as freshly created in the current process session.
///
/// Call after a successful insert into `my_relay_config` — both on explicit
/// user-triggered setup (`apply_relay_setup`) and on implicit recreation
/// after the hub returned 404 (`recreate_mailbox`).
pub fn mark_mailbox_created_this_session() {
    MAILBOX_CREATED_THIS_SESSION.store(true, Ordering::Relaxed);
}

/// Whether the relay mailbox persisted in `my_relay_config` was created
/// in the current process session (as opposed to restored from disk).
pub fn mailbox_created_this_session() -> bool {
    MAILBOX_CREATED_THIS_SESSION.load(Ordering::Relaxed)
}

/// Provenance of a `relay_mailbox_id` about to be sent to the hub directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxProvenance {
    /// No `relay_mailbox_id` in the payload — nothing to classify.
    Absent,
    /// Mailbox was created or recreated in the current session.
    Fresh,
    /// Mailbox was restored from `my_relay_config` on startup. If the hub
    /// has since purged it, peers will hit "deposit to non-existent mailbox".
    Restored,
}

/// Classify whether the `relay_mailbox_id` carried in a profile upsert was
/// minted in this session or restored from persistent storage.
///
/// Pure function so the branch in `register_or_update` stays trivial and
/// the logic can be unit-tested without standing up a mock hub.
pub fn classify_mailbox_provenance(relay_mailbox_id: Option<&str>) -> MailboxProvenance {
    match relay_mailbox_id {
        None => MailboxProvenance::Absent,
        Some(_) if mailbox_created_this_session() => MailboxProvenance::Fresh,
        Some(_) => MailboxProvenance::Restored,
    }
}

/// Reset the session flag. Exposed for tests only; integration tests in
/// the `tests/` directory are compiled as a separate crate and cannot see
/// `#[cfg(test)]` items, so this helper is `pub` + `#[doc(hidden)]` rather
/// than `#[cfg(test)] pub(crate)`. Must not be called from production code.
#[doc(hidden)]
pub fn reset_for_tests() {
    MAILBOX_CREATED_THIS_SESSION.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn flag_default_and_set() {
        reset_for_tests();
        assert!(!mailbox_created_this_session());

        mark_mailbox_created_this_session();
        assert!(mailbox_created_this_session());
    }

    #[test]
    #[serial]
    fn classify_absent() {
        reset_for_tests();
        assert_eq!(classify_mailbox_provenance(None), MailboxProvenance::Absent);
    }

    #[test]
    #[serial]
    fn classify_restored_when_not_marked() {
        reset_for_tests();
        assert_eq!(
            classify_mailbox_provenance(Some("mbx-1")),
            MailboxProvenance::Restored
        );
    }

    #[test]
    #[serial]
    fn classify_fresh_after_mark() {
        reset_for_tests();
        mark_mailbox_created_this_session();
        assert_eq!(
            classify_mailbox_provenance(Some("mbx-1")),
            MailboxProvenance::Fresh
        );
    }
}
