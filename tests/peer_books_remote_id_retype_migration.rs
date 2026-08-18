//! Regression tests for migration 095 (`migrate_peer_books_text_remote_ids`):
//! databases created before the uuid-PK era declare
//! `peer_books.remote_book_id` as INTEGER, and the directory-cache sentinel
//! writer stores the literal integer 0 there. sqlx refuses to decode an
//! INTEGER value into the entity's `String`, so every SeaORM read touching a
//! sentinel row failed and was swallowed by `unwrap_or_default()`: the
//! offline directory-catalog fallback always came back empty, and the
//! upsert's dedup pass re-inserted the full catalog on every fetch
//! (unbounded duplication).
//!
//! The first test reproduces the legacy shape, proves the decode failure
//! against it (the bug), then drives the REAL `run_migrations` entrypoint and
//! proves the rebuild: TEXT declaration, decodable rows, duplicates collapsed
//! newest-first, LAN rows untouched, FK cascade still live, idempotence.

use rust_lib_app::db;
use rust_lib_app::models::peer_book;
use sea_orm::{
    ColumnTrait, ConnectOptions, ConnectionTrait, Database, DatabaseConnection, EntityTrait,
    QueryFilter, Statement,
};

fn backend(db: &DatabaseConnection) -> sea_orm::DatabaseBackend {
    db.get_database_backend()
}

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute(Statement::from_string(backend(db), sql.to_owned()))
        .await
        .unwrap_or_else(|e| panic!("exec `{sql}` failed: {e}"));
}

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one(Statement::from_string(backend(db), sql.to_owned()))
        .await
        .unwrap_or_else(|e| panic!("query `{sql}` failed: {e}"))
        .unwrap_or_else(|| panic!("query `{sql}` returned no row"));
    row.try_get::<i64>("", "v").expect("decode v as i64")
}

async fn scalar_string(db: &DatabaseConnection, sql: &str) -> String {
    let row = db
        .query_one(Statement::from_string(backend(db), sql.to_owned()))
        .await
        .unwrap_or_else(|e| panic!("query `{sql}` failed: {e}"))
        .unwrap_or_else(|| panic!("query `{sql}` returned no row"));
    row.try_get::<String>("", "v").expect("decode v as String")
}

async fn single_conn(url: &str) -> DatabaseConnection {
    let mut opts = ConnectOptions::new(url.to_owned());
    opts.max_connections(1).min_connections(1);
    Database::connect(opts).await.expect("connect")
}

const NODE: &str = "06390e8c-3d6e-421b-8009-4df7be44540f";
const LAN_UUID: &str = "b7e2c9d4-1111-2222-3333-444455556666";

/// Replace the freshly-created `peer_books` with the legacy shape (INTEGER
/// `remote_book_id`), as every database predating the uuid-PK era still has
/// it, then seed one LAN row and a duplicated sentinel population written the
/// way the pre-fix sentinel INSERT wrote it (integer 0).
async fn reshape_to_legacy_and_seed(db: &DatabaseConnection) {
    exec(db, "PRAGMA foreign_keys = OFF").await;
    exec(db, "DROP TABLE peer_books").await;
    exec(
        db,
        r#"CREATE TABLE peer_books (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            peer_id INTEGER NOT NULL,
            remote_book_id INTEGER NOT NULL,
            title TEXT NOT NULL,
            isbn TEXT,
            author TEXT,
            cover_url TEXT,
            summary TEXT,
            synced_at TEXT NOT NULL,
            node_id TEXT,
            first_seen_at TEXT,
            notified_at TEXT,
            added_at TEXT,
            owned INTEGER NOT NULL DEFAULT 1,
            available_copies INTEGER,
            FOREIGN KEY (peer_id) REFERENCES peers(id) ON DELETE CASCADE
        )"#,
    )
    .await;
    exec(
        db,
        "CREATE INDEX IF NOT EXISTS idx_peer_books_peer_id ON peer_books(peer_id)",
    )
    .await;
    exec(
        db,
        "CREATE INDEX IF NOT EXISTS idx_peer_books_isbn ON peer_books(isbn)",
    )
    .await;

    exec(
        db,
        "INSERT INTO peers (id, name, url) VALUES (1, 'LAN peer', 'http://peer-1')",
    )
    .await;
    // A LAN row from the uuid era: remote_book_id already holds a uuid string
    // (kept as TEXT by the INTEGER column's affinity, since it is not
    // numeric). The rebuild must carry it over byte-identical.
    exec(
        db,
        &format!(
            "INSERT INTO peer_books \
             (id, peer_id, remote_book_id, title, isbn, synced_at) \
             VALUES (1, 1, '{LAN_UUID}', 'LAN book', '9780000000002', '2026-08-01')"
        ),
    )
    .await;
    // Sentinel rows, written as the pre-fix INSERT wrote them (integer 0).
    // The same catalog re-inserted on three successive fetches: the same
    // ISBN-bearing book three times (rising id = later fetch, distinct
    // cover_url to prove the newest row wins), an ISBN-less book twice, and
    // one single-copy book.
    for (id, isbn, title, cover) in [
        (10, "9781234567897", "Dune", "c-old"),
        (11, "", "Sans ISBN", "c-noisbn-old"),
        (12, "9791234567896", "Solo", "c-solo"),
        (20, "9781234567897", "Dune", "c-mid"),
        (21, "", "Sans ISBN", "c-noisbn-new"),
        (30, "9781234567897", "Dune", "c-new"),
    ] {
        exec(
            db,
            &format!(
                "INSERT INTO peer_books \
                 (id, peer_id, remote_book_id, title, isbn, cover_url, synced_at, node_id) \
                 VALUES ({id}, 0, 0, '{title}', '{isbn}', '{cover}', '2026-08-01', '{NODE}')"
            ),
        )
        .await;
    }
    exec(db, "PRAGMA foreign_keys = ON").await;
}

async fn sentinel_rows(db: &DatabaseConnection) -> Result<Vec<peer_book::Model>, sea_orm::DbErr> {
    peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(0))
        .all(db)
        .await
}

/// Legacy shape: the decode fails (the bug the offline fallback died of),
/// migration 095 rebuilds to TEXT, collapses duplicates newest-first, keeps
/// LAN rows and the FK cascade, and re-runs as a no-op.
#[tokio::test]
async fn migration_095_retypes_remote_book_id_and_dedups_sentinels() {
    let db = single_conn("sqlite::memory:").await;
    db::run_migrations(&db).await.expect("initial migrations");

    reshape_to_legacy_and_seed(&db).await;

    // The bug, proven against the legacy shape: integer sentinel values make
    // the whole read fail. This is what `unwrap_or_default()` silently turned
    // into an always-empty offline fallback.
    assert!(
        sentinel_rows(&db).await.is_err(),
        "reading integer sentinel rows through the String-typed entity must fail \
         on the legacy shape (this failure IS the bug migration 095 fixes)"
    );

    db::run_migrations(&db).await.expect("migration 095 run");

    assert_eq!(
        scalar_string(
            &db,
            "SELECT type AS v FROM pragma_table_info('peer_books') WHERE name = 'remote_book_id'"
        )
        .await,
        "TEXT",
        "remote_book_id must be declared TEXT after the rebuild"
    );

    let sentinels = sentinel_rows(&db)
        .await
        .expect("sentinel rows must decode after the rebuild");
    assert_eq!(
        sentinels.len(),
        3,
        "duplicates collapse to one row per book (ISBN key, title for ISBN-less)"
    );
    for row in &sentinels {
        assert_eq!(row.remote_book_id, "0", "sentinel id survives as text");
        assert_eq!(row.node_id.as_deref(), Some(NODE));
    }
    let dune = sentinels
        .iter()
        .find(|r| r.isbn.as_deref() == Some("9781234567897"))
        .expect("Dune kept");
    assert_eq!(
        dune.cover_url.as_deref(),
        Some("c-new"),
        "the newest duplicate (last fetch) must win"
    );
    let noisbn = sentinels
        .iter()
        .find(|r| r.title == "Sans ISBN")
        .expect("ISBN-less book kept");
    assert_eq!(
        noisbn.cover_url.as_deref(),
        Some("c-noisbn-new"),
        "ISBN-less duplicates dedup on title, newest wins"
    );

    // LAN row: untouched by the dedup, uuid carried over byte-identical.
    let lan = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(1))
        .all(&db)
        .await
        .expect("LAN rows decode");
    assert_eq!(lan.len(), 1, "LAN rows are never deduplicated");
    assert_eq!(lan[0].remote_book_id, LAN_UUID);

    // The rebuilt table must keep the ON DELETE CASCADE on peers while
    // sparing the sentinel rows (they reference no peer).
    exec(&db, "DELETE FROM peers WHERE id = 1").await;
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT COUNT(*) AS v FROM peer_books WHERE peer_id = 1"
        )
        .await,
        0,
        "deleting the peer must cascade to its cached books"
    );
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT COUNT(*) AS v FROM peer_books WHERE peer_id = 0"
        )
        .await,
        3,
        "the cascade must not touch sentinel rows"
    );

    // Both indexes exist on the rebuilt table.
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT COUNT(*) AS v FROM sqlite_master WHERE type = 'index' \
             AND tbl_name = 'peer_books' AND name IN \
             ('idx_peer_books_peer_id', 'idx_peer_books_isbn')"
        )
        .await,
        2,
        "both peer_books indexes must be recreated"
    );

    // Idempotence: the gate sees TEXT and the re-run changes nothing.
    db::run_migrations(&db).await.expect("re-run is a no-op");
    assert_eq!(
        sentinel_rows(&db).await.expect("still decodable").len(),
        3,
        "a re-run must not touch the rebuilt table"
    );
}

/// Fresh installs are born with the TEXT declaration: migration 095 must not
/// touch them, and a sentinel row written by the current INSERT (text '0')
/// decodes fine.
#[tokio::test]
async fn migration_095_is_a_noop_on_text_native_schemas() {
    let db = single_conn("sqlite::memory:").await;
    db::run_migrations(&db).await.expect("initial migrations");

    exec(&db, "PRAGMA foreign_keys = OFF").await;
    exec(
        &db,
        &format!(
            "INSERT INTO peer_books \
             (peer_id, remote_book_id, title, isbn, synced_at, node_id) \
             VALUES (0, '0', 'Dune', '9781234567897', '2026-08-01', '{NODE}')"
        ),
    )
    .await;
    exec(&db, "PRAGMA foreign_keys = ON").await;

    db::run_migrations(&db).await.expect("re-run migrations");

    let sentinels = sentinel_rows(&db).await.expect("rows decode");
    assert_eq!(sentinels.len(), 1);
    assert_eq!(sentinels[0].remote_book_id, "0");
}
