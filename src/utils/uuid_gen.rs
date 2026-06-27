//! Centralized stable-identifier generation.
//!
//! Every replicated entity (`books`, `copies`, `authors`, `contacts`, `tags`,
//! `loans`) carries a stable `uuid` that is valid across devices. The hub
//! E2EE-sync epic (ADR-011 root cause, decisions D3/D6) merges rows by this
//! identifier, so it must be generated in exactly one place.
//!
//! We use UUID **v7**: time-ordered, which keeps freshly created rows roughly
//! sortable (useful for debugging and as a natural ordering hint for the
//! cr-sqlite merge layer) while staying globally unique.

use uuid::Uuid;

/// Generate a fresh, time-ordered stable identifier (UUID v7) as a lowercase
/// hyphenated string, e.g. `0190b3c1-7f8a-7c2d-9e4f-1a2b3c4d5e6f`.
///
/// This is the single source of truth for new entity UUIDs. Insert paths rely
/// on the `before_save` ActiveModel hooks, which call this; ad-hoc callers
/// (backfill, raw-SQL test fixtures) should also use it rather than rolling
/// their own.
pub fn new_uuid_v7() -> String {
    Uuid::now_v7().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_unique_lowercase_hyphenated_v7() {
        let a = new_uuid_v7();
        let b = new_uuid_v7();
        assert_ne!(a, b, "two generated uuids must differ");
        assert_eq!(a.len(), 36, "canonical hyphenated form is 36 chars");
        assert_eq!(a.matches('-').count(), 4);
        assert_eq!(a, a.to_lowercase());

        let parsed = Uuid::parse_str(&a).expect("must parse back");
        assert_eq!(parsed.get_version_num(), 7, "must be a v7 uuid");
    }

    #[test]
    fn no_collisions_in_a_burst() {
        // A small burst stays unique (v7 mixes random bits into same-ms ids,
        // so we do not assert strict ordering within a millisecond).
        let mut ids: Vec<String> = (0..64).map(|_| new_uuid_v7()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 64, "no collisions in a small burst");
    }
}
