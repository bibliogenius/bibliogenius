//! Integration tests for the `library_config` seed produced at migration
//! time by `infrastructure/db.rs`. The seed must never be the legacy
//! placeholder `"My Library"` and must always be readable.
//!
//! See `utils/default_library_name.rs` for the seed generator.

use rust_lib_app::db;
use rust_lib_app::models::library_config;
use sea_orm::EntityTrait;

const TAG_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";

fn assert_seed_is_well_formed(name: &str) {
    assert!(!name.trim().is_empty(), "seed must not be empty");
    assert_ne!(
        name, "My Library",
        "seed must not be the legacy placeholder"
    );
    assert!(
        !name.contains("My Library"),
        "seed must not embed the legacy placeholder, got: {name:?}"
    );
    let (base, tag) = name
        .rsplit_once(" #")
        .unwrap_or_else(|| panic!("seed missing ' #<tag>' suffix, got: {name:?}"));
    assert!(!base.trim().is_empty(), "seed base must not be empty");
    assert_eq!(tag.len(), 4, "tag must be 4 chars, got: {tag:?}");
    for ch in tag.chars() {
        assert!(
            TAG_ALPHABET.contains(&(ch as u8)),
            "tag char {ch:?} not in safe alphabet"
        );
    }
}

#[tokio::test]
async fn fresh_db_seeds_library_config_with_non_placeholder_name() {
    let conn = db::init_db("sqlite::memory:").await.expect("init_db");
    let cfg = library_config::Entity::find()
        .one(&conn)
        .await
        .expect("query library_config")
        .expect("library_config row should exist after migration");

    assert_seed_is_well_formed(&cfg.name);
}

#[tokio::test]
async fn rerunning_migrations_preserves_existing_library_config_name() {
    let conn = db::init_db("sqlite::memory:").await.expect("init_db");

    // Simulate a prior write (e.g. user customization or Flutter overwrite).
    let custom = "Federico's Library";
    let existing = library_config::Entity::find()
        .one(&conn)
        .await
        .expect("query")
        .expect("row exists");
    let mut active: library_config::ActiveModel = existing.into();
    use sea_orm::{ActiveModelTrait, Set};
    active.name = Set(custom.to_string());
    active.update(&conn).await.expect("update");

    // Re-run migrations: INSERT OR IGNORE must not overwrite the existing row.
    db::run_migrations(&conn).await.expect("re-run migrations");

    let cfg = library_config::Entity::find()
        .one(&conn)
        .await
        .expect("query")
        .expect("row still exists");
    assert_eq!(
        cfg.name, custom,
        "migration re-run must not overwrite an existing library_config name"
    );
}
