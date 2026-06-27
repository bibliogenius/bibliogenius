//! Resolve the six replicated entities by their cross-device-stable `uuid`
//! column (added by migration 078) instead of the device-local integer `id`.
//!
//! The integer `id` is an autoincrement value that differs from one device to
//! the next, so it cannot identify a row across devices. The `uuid` can. These
//! lookups are the bridge that lets callers accept a uuid at the boundary while
//! the rest of the code still keys on the integer `id`: resolve the uuid to the
//! row, then read `.id` for the existing integer-keyed path, or use the model
//! directly. Each function returns `Ok(None)` when no row carries that uuid.
//!
//! All six tables (`books, copies, authors, contacts, tags, loans`) expose a
//! unique `uuid TEXT` column, so every lookup is a single indexed equality.

use sea_orm::{ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter};

use crate::models::{author, book, contact, copy, loan, tag};

macro_rules! find_by_uuid {
    ($name:ident, $module:ident, $doc:literal) => {
        #[doc = $doc]
        pub async fn $name(
            db: &DatabaseConnection,
            uuid: &str,
        ) -> Result<Option<$module::Model>, DbErr> {
            $module::Entity::find()
                .filter($module::Column::Uuid.eq(uuid))
                .one(db)
                .await
        }
    };
}

find_by_uuid!(find_book_by_uuid, book, "Find a book by its `uuid`.");
find_by_uuid!(find_copy_by_uuid, copy, "Find a copy by its `uuid`.");
find_by_uuid!(find_author_by_uuid, author, "Find an author by its `uuid`.");
find_by_uuid!(
    find_contact_by_uuid,
    contact,
    "Find a contact by its `uuid`."
);
find_by_uuid!(find_tag_by_uuid, tag, "Find a tag by its `uuid`.");
find_by_uuid!(find_loan_by_uuid, loan, "Find a loan by its `uuid`.");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db;
    use sqlx::Row;

    /// Build an in-memory DB at the current schema and seed one row per entity.
    /// Inserts run with foreign keys off so no parent rows are needed; the
    /// migration-078 trigger fills each `uuid`. Returns nothing: tests read the
    /// generated uuids back through the lookups under test.
    async fn seeded_db() -> DatabaseConnection {
        let db = db::init_db("sqlite::memory:").await.expect("init db");
        let now = chrono::Utc::now().to_rfc3339();
        let pool = db.get_sqlite_connection_pool();
        let mut conn = pool.acquire().await.expect("acquire");
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *conn)
            .await
            .unwrap();
        for sql in [
            format!(
                "INSERT INTO books (id, title, created_at, updated_at) VALUES (1, 'B', '{now}', '{now}')"
            ),
            format!(
                "INSERT INTO authors (id, name, created_at, updated_at) VALUES (1, 'A', '{now}', '{now}')"
            ),
            format!(
                "INSERT INTO tags (id, name, created_at, updated_at) VALUES (1, 'T', '{now}', '{now}')"
            ),
            format!(
                "INSERT INTO contacts (id, type, name, library_owner_id, created_at, updated_at) VALUES (1, 'borrower', 'C', 1, '{now}', '{now}')"
            ),
            format!(
                "INSERT INTO copies (id, book_id, library_id, status, created_at, updated_at) VALUES (1, 1, 1, 'available', '{now}', '{now}')"
            ),
            format!(
                "INSERT INTO loans (id, copy_id, contact_id, library_id, loan_date, due_date) VALUES (1, 1, 1, 1, '{now}', '{now}')"
            ),
        ] {
            sqlx::query(&sql)
                .execute(&mut *conn)
                .await
                .unwrap_or_else(|e| panic!("seed: {sql}\n{e}"));
        }
        drop(conn);
        db
    }

    /// Read the trigger-generated uuid for a seeded row.
    async fn uuid_of(db: &DatabaseConnection, table: &str) -> String {
        let pool = db.get_sqlite_connection_pool();
        let mut conn = pool.acquire().await.unwrap();
        let row = sqlx::query(&format!("SELECT uuid FROM \"{table}\" WHERE id = 1"))
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        row.get::<String, _>("uuid")
    }

    #[tokio::test]
    async fn every_lookup_resolves_its_seeded_row() {
        let db = seeded_db().await;

        let book_uuid = uuid_of(&db, "books").await;
        assert_eq!(
            find_book_by_uuid(&db, &book_uuid)
                .await
                .unwrap()
                .unwrap()
                .id,
            1
        );
        assert_eq!(
            find_author_by_uuid(&db, &uuid_of(&db, "authors").await)
                .await
                .unwrap()
                .unwrap()
                .id,
            1
        );
        assert_eq!(
            find_tag_by_uuid(&db, &uuid_of(&db, "tags").await)
                .await
                .unwrap()
                .unwrap()
                .id,
            1
        );
        assert_eq!(
            find_contact_by_uuid(&db, &uuid_of(&db, "contacts").await)
                .await
                .unwrap()
                .unwrap()
                .id,
            1
        );
        assert_eq!(
            find_copy_by_uuid(&db, &uuid_of(&db, "copies").await)
                .await
                .unwrap()
                .unwrap()
                .id,
            1
        );
        assert_eq!(
            find_loan_by_uuid(&db, &uuid_of(&db, "loans").await)
                .await
                .unwrap()
                .unwrap()
                .id,
            1
        );

        // The resolved book really is the seeded one.
        assert_eq!(
            find_book_by_uuid(&db, &book_uuid)
                .await
                .unwrap()
                .unwrap()
                .uuid,
            book_uuid
        );
    }

    #[tokio::test]
    async fn unknown_uuid_resolves_to_none() {
        let db = seeded_db().await;
        assert!(
            find_book_by_uuid(&db, "00000000-0000-0000-0000-000000000000")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_loan_by_uuid(&db, "not-a-real-uuid")
                .await
                .unwrap()
                .is_none()
        );
    }
}
