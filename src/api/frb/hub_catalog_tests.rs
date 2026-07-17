// Tests for the hub catalog cache (merge_directory_entry, upsert_directory_catalog_cache).
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

#[cfg(test)]
mod merge_directory_entry_tests {
    use super::{CatalogEntry, merge_directory_entry};
    use crate::models::peer_book;

    fn cached_row() -> peer_book::Model {
        peer_book::Model {
            id: 1,
            peer_id: 0,
            remote_book_id: "42".to_string(),
            title: "La Peste".to_string(),
            isbn: Some("9782020086929".to_string()),
            author: Some("Albert Camus".to_string()),
            cover_url: Some("https://hub/covers/n/42.jpg".to_string()),
            summary: None,
            synced_at: "2026-06-01T00:00:00Z".to_string(),
            node_id: Some("node-eve".to_string()),
            first_seen_at: None,
            notified_at: None,
            added_at: Some("2026-05-01T00:00:00Z".to_string()),
            owned: true,
            available_copies: None,
        }
    }

    fn isbn_only_entry() -> CatalogEntry {
        CatalogEntry {
            isbn: "9782020086929".to_string(),
            book_id: None,
            title: String::new(),
            author: None,
            cover_url: None,
            added_at: None,
        }
    }

    #[test]
    fn degraded_entry_preserves_cached_metadata() {
        let merged = merge_directory_entry(&cached_row(), &isbn_only_entry());
        assert_eq!(merged.title, "La Peste");
        assert_eq!(merged.author.as_deref(), Some("Albert Camus"));
        assert_eq!(
            merged.cover_url.as_deref(),
            Some("https://hub/covers/n/42.jpg")
        );
        assert_eq!(merged.added_at.as_deref(), Some("2026-05-01T00:00:00Z"));
    }

    #[test]
    fn fresh_metadata_still_overwrites_the_cache() {
        let entry = CatalogEntry {
            isbn: "9782020086929".to_string(),
            book_id: Some("0197f2a4".to_string()),
            title: "La Peste (nouvelle éd.)".to_string(),
            author: Some("A. Camus".to_string()),
            cover_url: Some("https://hub/covers/n/uuid.jpg".to_string()),
            added_at: Some("2026-07-01T00:00:00Z".to_string()),
        };
        let merged = merge_directory_entry(&cached_row(), &entry);
        assert_eq!(merged.title, "La Peste (nouvelle éd.)");
        assert_eq!(merged.author.as_deref(), Some("A. Camus"));
        assert_eq!(
            merged.cover_url.as_deref(),
            Some("https://hub/covers/n/uuid.jpg")
        );
        assert_eq!(merged.added_at.as_deref(), Some("2026-07-01T00:00:00Z"));
    }

    #[test]
    fn blank_cache_takes_whatever_the_entry_has() {
        let mut cached = cached_row();
        cached.title = String::new();
        cached.author = None;
        cached.cover_url = None;
        cached.added_at = None;
        let merged = merge_directory_entry(&cached, &isbn_only_entry());
        assert_eq!(merged.title, "");
        assert_eq!(merged.author, None);
        assert_eq!(merged.cover_url, None);
    }
}

#[cfg(test)]
mod upsert_directory_catalog_cache_tests {
    //! The cache↔catalog match is keyed on the canonical ISBN-13 form: the
    //! same edition circulates as ISBN-10 on one side and ISBN-13 on the
    //! other, and a raw-string comparison would duplicate the row, miss the
    //! metadata update, and let the prune pass delete the matching row stored
    //! under the other form.
    use super::{CatalogEntry, upsert_directory_catalog_cache};
    use crate::models::peer_book;
    use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

    const NODE: &str = "node-under-test";
    // Same edition in both forms (canonical pair from the Wikipedia ISBN article).
    const ISBN10: &str = "0306406152";
    const ISBN13: &str = "9780306406157";

    async fn test_db() -> DatabaseConnection {
        crate::infrastructure::db::init_db("sqlite::memory:")
            .await
            .expect("init db")
    }

    fn entry(isbn: &str, title: &str) -> CatalogEntry {
        CatalogEntry {
            isbn: isbn.to_string(),
            book_id: None,
            title: title.to_string(),
            author: None,
            cover_url: None,
            added_at: None,
        }
    }

    async fn cached_rows(db: &DatabaseConnection) -> Vec<peer_book::Model> {
        peer_book::Entity::find()
            .filter(peer_book::Column::NodeId.eq(NODE))
            .filter(peer_book::Column::PeerId.eq(0))
            .all(db)
            .await
            .unwrap()
    }

    /// Seed a sentinel cache row directly (bypassing the upsert) so tests can
    /// reproduce legacy states the fixed upsert can no longer create, e.g. the
    /// same book cached once per ISBN form. Mirrors the production insert
    /// (FK off on a dedicated connection, restored before release).
    async fn seed_row(db: &DatabaseConnection, isbn: &str, title: &str, cover: Option<&str>) {
        let mut conn = db.get_sqlite_connection_pool().acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO peer_books \
             (peer_id, remote_book_id, title, isbn, author, cover_url, \
              summary, synced_at, node_id, first_seen_at, added_at, notified_at) \
             VALUES (0, 0, ?, ?, NULL, ?, NULL, '2026-01-01T00:00:00Z', ?, \
                     '2026-01-01T00:00:00Z', NULL, '2026-01-01T00:00:00Z')",
        )
        .bind(title)
        .bind(isbn)
        .bind(cover)
        .bind(NODE)
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&mut *conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn isbn13_entry_updates_row_cached_as_isbn10() {
        let db = test_db().await;
        seed_row(&db, ISBN10, "Old title", None).await;

        let result = upsert_directory_catalog_cache(&db, NODE, &[entry(ISBN13, "New title")]).await;

        let rows = cached_rows(&db).await;
        assert_eq!(rows.len(), 1, "must match, not duplicate or prune");
        assert_eq!(rows[0].title, "New title");
        // The stored form is never rewritten; only the comparison is canonical.
        assert_eq!(rows[0].isbn.as_deref(), Some(ISBN10));
        assert_eq!(result.len(), 1);
    }

    #[tokio::test]
    async fn isbn10_entry_updates_row_cached_as_isbn13() {
        let db = test_db().await;
        seed_row(&db, ISBN13, "Old title", None).await;

        upsert_directory_catalog_cache(&db, NODE, &[entry(ISBN10, "New title")]).await;

        let rows = cached_rows(&db).await;
        assert_eq!(rows.len(), 1, "must match, not duplicate or prune");
        assert_eq!(rows[0].title, "New title");
        assert_eq!(rows[0].isbn.as_deref(), Some(ISBN13));
    }

    #[tokio::test]
    async fn prune_spares_the_row_matched_under_the_other_form() {
        let db = test_db().await;
        seed_row(&db, ISBN10, "Kept", None).await;
        seed_row(&db, "9782264024848", "Gone", None).await;

        // The catalog now serves the kept book in its ISBN-13 form only: the
        // other book is pruned, the kept one survives the form change.
        upsert_directory_catalog_cache(&db, NODE, &[entry(ISBN13, "Kept")]).await;

        let rows = cached_rows(&db).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Kept");
        assert_eq!(rows[0].isbn.as_deref(), Some(ISBN10));
    }

    #[tokio::test]
    async fn invalid_and_empty_isbns_compare_raw_without_collision() {
        let db = test_db().await;
        let entries = [entry("not-an-isbn", "Invalid"), entry("", "No ISBN")];

        upsert_directory_catalog_cache(&db, NODE, &entries).await;
        assert_eq!(cached_rows(&db).await.len(), 2);

        // Same raw values again: stable (matched raw), no prune, no dup.
        upsert_directory_catalog_cache(&db, NODE, &entries).await;
        let rows = cached_rows(&db).await;
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn legacy_duplicate_rows_fold_into_one() {
        let db = test_db().await;
        // Legacy state created by the old raw-form matching: the same book
        // cached once per ISBN form, with knowledge split across the rows.
        seed_row(&db, ISBN10, "Dup", None).await;
        seed_row(&db, ISBN13, "Dup", Some("https://hub/covers/n/1.jpg")).await;

        upsert_directory_catalog_cache(&db, NODE, &[entry(ISBN13, "Dup")]).await;

        let rows = cached_rows(&db).await;
        assert_eq!(rows.len(), 1, "shadowed duplicate must be deleted");
        // The fold is additive: the surviving row keeps the duplicate's cover.
        assert_eq!(
            rows[0].cover_url.as_deref(),
            Some("https://hub/covers/n/1.jpg")
        );
    }

    #[tokio::test]
    async fn catalog_listing_both_forms_creates_a_single_row() {
        let db = test_db().await;

        upsert_directory_catalog_cache(
            &db,
            NODE,
            &[entry(ISBN10, "Same book"), entry(ISBN13, "Same book")],
        )
        .await;

        assert_eq!(cached_rows(&db).await.len(), 1);
    }

    #[tokio::test]
    async fn empty_catalog_does_not_wipe_existing_cache() {
        let db = test_db().await;
        seed_row(&db, ISBN13, "Cached title", None).await;

        let result = upsert_directory_catalog_cache(&db, NODE, &[]).await;

        assert!(result.is_empty());
        let rows = cached_rows(&db).await;
        assert_eq!(
            rows.len(),
            1,
            "an empty incoming catalog must not prune cached rows"
        );
        assert_eq!(rows[0].title, "Cached title");
    }
}
