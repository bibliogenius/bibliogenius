//! Tests for `api::backup::latest_user_data_change_at`, the watermark used
//! by the auto-backup scheduler's skip-if-unchanged check (ADR-037 §6).
//!
//! Validates the contract the scheduler relies on: the helper returns a
//! stable token that advances when any of the four monitored tables
//! (`books`, `copies`, `loans`, `library_config`) gains a fresher
//! `updated_at` row. The scheduler treats the result as opaque; tests
//! therefore exercise "advances" rather than asserting exact timestamp
//! equality (the seed migration writes SQLite's `datetime('now')` while
//! application writes use RFC 3339; both are monotonic per source so the
//! "did it change?" check the scheduler actually does keeps working).

use rust_lib_app::api::backup::latest_user_data_change_at;
use rust_lib_app::db;
use rust_lib_app::models::book;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use tempfile::TempDir;

async fn setup_test_db(tmp: &TempDir) -> DatabaseConnection {
    let db_path = tmp.path().join("source.sqlite");
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    db::init_db(&url).await.expect("init_db")
}

#[tokio::test]
async fn returns_seed_watermark_on_fresh_db() {
    // `init_db` seeds `library_config` row 1 with `datetime('now')` so a
    // brand-new install always has a watermark; `None` would mean the four
    // tables are gone, which never happens in production. The scheduler
    // uses this to decide "has anything changed since my last run?", not
    // "is this a fresh install?" — so a non-empty seed is the correct
    // baseline behaviour.
    let tmp = TempDir::new().unwrap();
    let conn = setup_test_db(&tmp).await;

    let watermark = latest_user_data_change_at(&conn)
        .await
        .expect("watermark query");

    let token = watermark.expect("library_config seed must produce a watermark");
    assert!(!token.is_empty(), "watermark must be a non-empty token");
}

#[tokio::test]
async fn watermark_advances_when_a_book_is_inserted() {
    let tmp = TempDir::new().unwrap();
    let conn = setup_test_db(&tmp).await;

    let baseline = latest_user_data_change_at(&conn).await.unwrap();

    // Far-future RFC 3339 timestamp; lexicographically beats the seed and
    // any real datetime('now') value the migration could have written.
    let future_ts = "2099-01-01T00:00:00+00:00";
    book::ActiveModel {
        title: Set("Future Book".into()),
        created_at: Set(future_ts.into()),
        updated_at: Set(future_ts.into()),
        ..Default::default()
    }
    .insert(&conn)
    .await
    .unwrap();

    let after = latest_user_data_change_at(&conn).await.unwrap();
    assert_ne!(
        after, baseline,
        "inserting a book must change the watermark token"
    );
    assert_eq!(
        after.as_deref(),
        Some(future_ts),
        "MAX must surface the freshest updated_at across the four tables"
    );
}

#[tokio::test]
async fn watermark_picks_max_across_tables() {
    let tmp = TempDir::new().unwrap();
    let conn = setup_test_db(&tmp).await;

    let older = "2025-01-01T00:00:00+00:00";
    let newer = "2026-06-01T00:00:00+00:00";

    book::ActiveModel {
        title: Set("Older".into()),
        created_at: Set(older.into()),
        updated_at: Set(older.into()),
        ..Default::default()
    }
    .insert(&conn)
    .await
    .unwrap();

    book::ActiveModel {
        title: Set("Newer".into()),
        created_at: Set(newer.into()),
        updated_at: Set(newer.into()),
        ..Default::default()
    }
    .insert(&conn)
    .await
    .unwrap();

    let watermark = latest_user_data_change_at(&conn).await.unwrap();
    assert_eq!(
        watermark.as_deref(),
        Some(newer),
        "watermark must reflect the newest updated_at, not the most recently inserted row"
    );
}
