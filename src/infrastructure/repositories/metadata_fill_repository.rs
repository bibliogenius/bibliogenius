//! SeaORM implementation of [`MetadataFillRepository`] (ADR-041).
//!
//! The `books` selection/stat queries reason about "incompleteness" with a
//! single shared SQL predicate (`INCOMPLETE_PRED`) so the dashboard stat, the
//! work-list and the run total can never drift apart. The run/journal tables
//! have no SeaORM entity (they are an internal feature concern, not part of the
//! `models/*` API contract), so they are driven with parameterized raw SQL.

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement, TransactionTrait, Value};

use crate::domain::DomainError;
use crate::domain::metadata_fill::{
    CompletenessStats, FillRun, FilledField, GapValues, IncompleteBook, IncompleteBookDetail,
    MetadataFillRepository, RecentFilledBook, RecentFilledField, UndoOutcome,
};

/// A book counts as "incomplete" when any of the five gap-fill fields is empty.
/// Text fields treat NULL or whitespace-only as empty; integer fields treat
/// NULL as empty. Kept as one fragment so stat/selection/total stay consistent.
const INCOMPLETE_PRED: &str = "(summary IS NULL OR TRIM(summary) = '' \
     OR publisher IS NULL OR TRIM(publisher) = '' \
     OR cover_url IS NULL OR TRIM(cover_url) = '' \
     OR publication_year IS NULL \
     OR page_count IS NULL)";

/// A book "has an ISBN" when the column is non-null and not whitespace-only.
const HAS_ISBN_PRED: &str = "(isbn IS NOT NULL AND TRIM(isbn) <> '')";
const NO_ISBN_PRED: &str = "(isbn IS NULL OR TRIM(isbn) = '')";

/// The two integer-typed gap-fill fields (compared/stored as decimal strings).
fn is_int_field(field: &str) -> bool {
    field == "page_count" || field == "publication_year"
}

/// A text field is empty when NULL or whitespace-only (matches `INCOMPLETE_PRED`).
fn text_is_empty(v: &Option<String>) -> bool {
    v.as_deref().map(str::trim).unwrap_or("").is_empty()
}

/// Whitelist guard: only the five known columns may be interpolated into SQL.
fn is_fill_field(field: &str) -> bool {
    crate::domain::metadata_fill::FILL_FIELDS.contains(&field)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub struct SeaOrmMetadataFillRepository {
    db: DatabaseConnection,
}

impl SeaOrmMetadataFillRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    fn backend(&self) -> sea_orm::DatabaseBackend {
        self.db.get_database_backend()
    }

    async fn count(&self, where_clause: &str) -> Result<i64, DomainError> {
        let sql = format!("SELECT COUNT(*) AS cnt FROM books WHERE {where_clause}");
        let row = self
            .db
            .query_one(Statement::from_string(self.backend(), sql))
            .await?
            .ok_or_else(|| DomainError::Database("count returned no row".into()))?;
        Ok(row.try_get::<i64>("", "cnt")?)
    }

    /// Total number of empty gap-fill fields across all owned books. Sums the
    /// per-book empty-field count using the same emptiness rules as
    /// `INCOMPLETE_PRED` (text: NULL/blank; integer: NULL).
    async fn count_empty_fields(&self) -> Result<i64, DomainError> {
        let sql = "SELECT COALESCE(SUM(\
             (CASE WHEN summary IS NULL OR TRIM(summary) = '' THEN 1 ELSE 0 END) \
             + (CASE WHEN publisher IS NULL OR TRIM(publisher) = '' THEN 1 ELSE 0 END) \
             + (CASE WHEN cover_url IS NULL OR TRIM(cover_url) = '' THEN 1 ELSE 0 END) \
             + (CASE WHEN publication_year IS NULL THEN 1 ELSE 0 END) \
             + (CASE WHEN page_count IS NULL THEN 1 ELSE 0 END)\
             ), 0) AS empty_fields FROM books WHERE owned = 1";
        let row = self
            .db
            .query_one(Statement::from_string(self.backend(), sql.to_owned()))
            .await?
            .ok_or_else(|| DomainError::Database("empty_fields returned no row".into()))?;
        Ok(row.try_get::<i64>("", "empty_fields")?)
    }
}

fn row_to_incomplete(row: &sea_orm::QueryResult) -> Result<IncompleteBook, DomainError> {
    Ok(IncompleteBook {
        id: row.try_get::<i32>("", "id")?,
        title: row.try_get::<String>("", "title")?,
        isbn: row.try_get::<Option<String>>("", "isbn")?,
    })
}

fn row_to_run(row: &sea_orm::QueryResult) -> Result<FillRun, DomainError> {
    Ok(FillRun {
        batch_id: row.try_get::<String>("", "batch_id")?,
        status: row.try_get::<String>("", "status")?,
        total: row.try_get::<i64>("", "total")?,
        done: row.try_get::<i64>("", "done")?,
        filled: row.try_get::<i64>("", "filled")?,
        skipped: row.try_get::<i64>("", "skipped")?,
        errored: row.try_get::<i64>("", "errored")?,
        cursor_book_id: row.try_get::<i32>("", "cursor_book_id")?,
        current_title: row.try_get::<Option<String>>("", "current_title")?,
    })
}

#[async_trait]
impl MetadataFillRepository for SeaOrmMetadataFillRepository {
    async fn completeness_stats(&self) -> Result<CompletenessStats, DomainError> {
        let owned_total = self.count("owned = 1").await?;
        let incomplete = self
            .count(&format!("owned = 1 AND {INCOMPLETE_PRED}"))
            .await?;
        let no_isbn = self
            .count(&format!(
                "owned = 1 AND {INCOMPLETE_PRED} AND {NO_ISBN_PRED}"
            ))
            .await?;
        let empty_fields = self.count_empty_fields().await?;
        Ok(CompletenessStats {
            owned_total,
            complete: owned_total - incomplete,
            incomplete,
            no_isbn,
            empty_fields,
        })
    }

    async fn list_incomplete_with_isbn(
        &self,
        after_id: i32,
        limit: u64,
    ) -> Result<Vec<IncompleteBook>, DomainError> {
        let sql = format!(
            "SELECT id, title, isbn FROM books \
             WHERE owned = 1 AND {INCOMPLETE_PRED} AND {HAS_ISBN_PRED} AND id > ? \
             ORDER BY id ASC LIMIT ?"
        );
        let rows = self
            .db
            .query_all(Statement::from_sql_and_values(
                self.backend(),
                sql,
                [Value::from(after_id), Value::from(limit as i64)],
            ))
            .await?;
        rows.iter().map(row_to_incomplete).collect()
    }

    async fn count_incomplete_with_isbn(&self) -> Result<i64, DomainError> {
        self.count(&format!(
            "owned = 1 AND {INCOMPLETE_PRED} AND {HAS_ISBN_PRED}"
        ))
        .await
    }

    async fn list_incomplete_without_isbn(&self) -> Result<Vec<IncompleteBook>, DomainError> {
        let sql = format!(
            "SELECT id, title, isbn FROM books \
             WHERE owned = 1 AND {INCOMPLETE_PRED} AND {NO_ISBN_PRED} \
             ORDER BY id ASC"
        );
        let rows = self
            .db
            .query_all(Statement::from_string(self.backend(), sql))
            .await?;
        rows.iter().map(row_to_incomplete).collect()
    }

    async fn list_incomplete(&self, limit: u64) -> Result<Vec<IncompleteBookDetail>, DomainError> {
        let sql = format!(
            "SELECT id, title, isbn, cover_url, summary, publisher, publication_year, page_count \
             FROM books WHERE owned = 1 AND {INCOMPLETE_PRED} ORDER BY title ASC LIMIT ?"
        );
        let rows = self
            .db
            .query_all(Statement::from_sql_and_values(
                self.backend(),
                sql,
                [Value::from(limit as i64)],
            ))
            .await?;

        let mut out: Vec<IncompleteBookDetail> = Vec::with_capacity(rows.len());
        for row in &rows {
            let summary = row.try_get::<Option<String>>("", "summary")?;
            let publisher = row.try_get::<Option<String>>("", "publisher")?;
            let cover = row.try_get::<Option<String>>("", "cover_url")?;
            let year = row.try_get::<Option<i32>>("", "publication_year")?;
            let pages = row.try_get::<Option<i32>>("", "page_count")?;

            let mut missing = Vec::new();
            if text_is_empty(&summary) {
                missing.push("summary".to_string());
            }
            if text_is_empty(&publisher) {
                missing.push("publisher".to_string());
            }
            if pages.is_none() {
                missing.push("page_count".to_string());
            }
            if year.is_none() {
                missing.push("publication_year".to_string());
            }
            if text_is_empty(&cover) {
                missing.push("cover_url".to_string());
            }

            out.push(IncompleteBookDetail {
                id: row.try_get::<i32>("", "id")?,
                title: row.try_get::<String>("", "title")?,
                isbn: row.try_get::<Option<String>>("", "isbn")?,
                cover_url: cover,
                missing,
            });
        }
        // Closest-to-complete first (fewest missing fields), then alphabetical.
        out.sort_by_key(|b| b.missing.len());
        Ok(out)
    }

    async fn apply_fill(
        &self,
        batch_id: &str,
        book_id: i32,
        candidate: GapValues,
    ) -> Result<Vec<FilledField>, DomainError> {
        if candidate.is_empty() {
            return Ok(vec![]);
        }
        let txn = self.db.begin().await?;
        let backend = self.backend();

        // Snapshot the current values so we only fill what is empty.
        let row = txn
            .query_one(Statement::from_sql_and_values(
                backend,
                "SELECT summary, publisher, publication_year, cover_url, page_count \
                 FROM books WHERE id = ?",
                [Value::from(book_id)],
            ))
            .await?;
        let Some(row) = row else {
            // Book vanished (deleted concurrently): nothing to do.
            txn.rollback().await?;
            return Ok(vec![]);
        };

        let cur_summary = row.try_get::<Option<String>>("", "summary")?;
        let cur_publisher = row.try_get::<Option<String>>("", "publisher")?;
        let cur_year = row.try_get::<Option<i32>>("", "publication_year")?;
        let cur_cover = row.try_get::<Option<String>>("", "cover_url")?;
        let cur_pages = row.try_get::<Option<i32>>("", "page_count")?;

        let text_empty = |v: &Option<String>| v.as_deref().map(str::trim).unwrap_or("").is_empty();

        let mut filled: Vec<FilledField> = Vec::new();
        let now = now_rfc3339();

        // Each entry: (field, is-empty, value-to-write-as-Value, value-string).
        let mut writes: Vec<(&str, Value, String)> = Vec::new();
        if text_empty(&cur_summary)
            && let Some(v) = candidate.summary.filter(|s| !s.trim().is_empty())
        {
            writes.push(("summary", Value::from(v.clone()), v));
        }
        if text_empty(&cur_publisher)
            && let Some(v) = candidate.publisher.filter(|s| !s.trim().is_empty())
        {
            writes.push(("publisher", Value::from(v.clone()), v));
        }
        if text_empty(&cur_cover)
            && let Some(v) = candidate.cover_url.filter(|s| !s.trim().is_empty())
        {
            writes.push(("cover_url", Value::from(v.clone()), v));
        }
        if cur_year.is_none()
            && let Some(v) = candidate.publication_year
        {
            writes.push(("publication_year", Value::from(v), v.to_string()));
        }
        if cur_pages.is_none()
            && let Some(v) = candidate.page_count
        {
            writes.push(("page_count", Value::from(v), v.to_string()));
        }

        for (field, value, value_str) in writes {
            // `field` is a compile-time literal from the set above; never user input.
            txn.execute(Statement::from_sql_and_values(
                backend,
                format!("UPDATE books SET {field} = ?, updated_at = ? WHERE id = ?"),
                [value, Value::from(now.clone()), Value::from(book_id)],
            ))
            .await?;
            txn.execute(Statement::from_sql_and_values(
                backend,
                "INSERT INTO metadata_fill_journal \
                 (batch_id, book_id, field, value_set, created_at) VALUES (?, ?, ?, ?, ?)",
                [
                    Value::from(batch_id.to_string()),
                    Value::from(book_id),
                    Value::from(field.to_string()),
                    Value::from(value_str.clone()),
                    Value::from(now.clone()),
                ],
            ))
            .await?;
            filled.push(FilledField {
                field: field.to_string(),
                value: value_str,
            });
        }

        txn.commit().await?;
        Ok(filled)
    }

    async fn create_run(&self, batch_id: &str, total: i64) -> Result<(), DomainError> {
        let now = now_rfc3339();
        self.db
            .execute(Statement::from_sql_and_values(
                self.backend(),
                "INSERT INTO metadata_fill_run \
                 (batch_id, status, total, done, filled, skipped, errored, cursor_book_id, \
                  current_title, started_at, updated_at) \
                 VALUES (?, 'running', ?, 0, 0, 0, 0, 0, NULL, ?, ?)",
                [
                    Value::from(batch_id.to_string()),
                    Value::from(total),
                    Value::from(now.clone()),
                    Value::from(now),
                ],
            ))
            .await?;
        Ok(())
    }

    async fn get_active_run(&self) -> Result<Option<FillRun>, DomainError> {
        let row = self
            .db
            .query_one(Statement::from_string(
                self.backend(),
                "SELECT * FROM metadata_fill_run \
                 WHERE status IN ('running', 'interrupted') ORDER BY started_at DESC LIMIT 1"
                    .to_owned(),
            ))
            .await?;
        row.as_ref().map(row_to_run).transpose()
    }

    async fn last_run(&self) -> Result<Option<FillRun>, DomainError> {
        let row = self
            .db
            .query_one(Statement::from_string(
                self.backend(),
                "SELECT * FROM metadata_fill_run ORDER BY started_at DESC LIMIT 1".to_owned(),
            ))
            .await?;
        row.as_ref().map(row_to_run).transpose()
    }

    async fn get_run(&self, batch_id: &str) -> Result<Option<FillRun>, DomainError> {
        let row = self
            .db
            .query_one(Statement::from_sql_and_values(
                self.backend(),
                "SELECT * FROM metadata_fill_run WHERE batch_id = ?",
                [Value::from(batch_id.to_string())],
            ))
            .await?;
        row.as_ref().map(row_to_run).transpose()
    }

    async fn update_run_progress(&self, run: &FillRun) -> Result<(), DomainError> {
        self.db
            .execute(Statement::from_sql_and_values(
                self.backend(),
                "UPDATE metadata_fill_run SET status = ?, total = ?, done = ?, filled = ?, \
                 skipped = ?, errored = ?, cursor_book_id = ?, current_title = ?, updated_at = ? \
                 WHERE batch_id = ?",
                [
                    Value::from(run.status.clone()),
                    Value::from(run.total),
                    Value::from(run.done),
                    Value::from(run.filled),
                    Value::from(run.skipped),
                    Value::from(run.errored),
                    Value::from(run.cursor_book_id),
                    Value::from(run.current_title.clone()),
                    Value::from(now_rfc3339()),
                    Value::from(run.batch_id.clone()),
                ],
            ))
            .await?;
        Ok(())
    }

    async fn set_run_status(&self, batch_id: &str, status: &str) -> Result<(), DomainError> {
        self.db
            .execute(Statement::from_sql_and_values(
                self.backend(),
                "UPDATE metadata_fill_run SET status = ?, updated_at = ? WHERE batch_id = ?",
                [
                    Value::from(status.to_string()),
                    Value::from(now_rfc3339()),
                    Value::from(batch_id.to_string()),
                ],
            ))
            .await?;
        Ok(())
    }

    async fn mark_running_as_interrupted(&self) -> Result<(), DomainError> {
        self.db
            .execute(Statement::from_sql_and_values(
                self.backend(),
                "UPDATE metadata_fill_run SET status = 'interrupted', updated_at = ? \
                 WHERE status = 'running'",
                [Value::from(now_rfc3339())],
            ))
            .await?;
        Ok(())
    }

    async fn recent_filled(&self, limit: u64) -> Result<Vec<RecentFilledBook>, DomainError> {
        // All active entries newest-first, joined to the book title. Grouped in
        // Rust so the per-book field list preserves the newest-first order and
        // the book cap applies to distinct books, not rows.
        let rows = self
            .db
            .query_all(Statement::from_string(
                self.backend(),
                "SELECT j.id AS jid, j.batch_id AS batch_id, j.book_id AS book_id, \
                 j.field AS field, j.value_set AS value_set, j.created_at AS created_at, \
                 b.title AS title, b.cover_url AS cover_url \
                 FROM metadata_fill_journal j LEFT JOIN books b ON b.id = j.book_id \
                 WHERE j.undone_at IS NULL ORDER BY j.created_at DESC, j.id DESC"
                    .to_owned(),
            ))
            .await?;

        let mut out: Vec<RecentFilledBook> = Vec::new();
        for row in &rows {
            let book_id = row.try_get::<i32>("", "book_id")?;
            let field = RecentFilledField {
                journal_id: row.try_get::<i64>("", "jid")?,
                batch_id: row.try_get::<String>("", "batch_id")?,
                field: row.try_get::<String>("", "field")?,
                value: row.try_get::<String>("", "value_set")?,
            };
            if let Some(existing) = out.iter_mut().find(|b| b.book_id == book_id) {
                existing.fields.push(field);
            } else {
                if out.len() as u64 >= limit {
                    continue;
                }
                let title = row
                    .try_get::<Option<String>>("", "title")?
                    .unwrap_or_default();
                let cover_url = row.try_get::<Option<String>>("", "cover_url")?;
                out.push(RecentFilledBook {
                    book_id,
                    title,
                    cover_url,
                    fields: vec![field],
                });
            }
        }
        Ok(out)
    }

    async fn undo_field(&self, journal_id: i64) -> Result<UndoOutcome, DomainError> {
        let txn = self.db.begin().await?;
        let backend = self.backend();

        let entry = txn
            .query_one(Statement::from_sql_and_values(
                backend,
                "SELECT book_id, field, value_set, undone_at \
                 FROM metadata_fill_journal WHERE id = ?",
                [Value::from(journal_id)],
            ))
            .await?;
        let Some(entry) = entry else {
            txn.rollback().await?;
            return Ok(UndoOutcome::NotFound);
        };
        if entry.try_get::<Option<String>>("", "undone_at")?.is_some() {
            txn.rollback().await?;
            return Ok(UndoOutcome::NotFound);
        }

        let book_id = entry.try_get::<i32>("", "book_id")?;
        let field = entry.try_get::<String>("", "field")?;
        let value_set = entry.try_get::<String>("", "value_set")?;
        if !is_fill_field(&field) {
            txn.rollback().await?;
            return Err(DomainError::Validation(format!("unknown field: {field}")));
        }

        // Read the book's current value in string form for the "still ours" test.
        let book_row = txn
            .query_one(Statement::from_sql_and_values(
                backend,
                format!("SELECT {field} AS val FROM books WHERE id = ?"),
                [Value::from(book_id)],
            ))
            .await?;
        let current: Option<String> = match book_row {
            Some(r) if is_int_field(&field) => {
                r.try_get::<Option<i32>>("", "val")?.map(|v| v.to_string())
            }
            Some(r) => r.try_get::<Option<String>>("", "val")?,
            None => None,
        };

        let still_ours = current.as_deref() == Some(value_set.as_str());

        // Retire the journal entry either way so it leaves the "recently
        // completed" list; only revert the book column when it is still ours.
        let now = now_rfc3339();
        if still_ours {
            txn.execute(Statement::from_sql_and_values(
                backend,
                format!("UPDATE books SET {field} = NULL, updated_at = ? WHERE id = ?"),
                [Value::from(now.clone()), Value::from(book_id)],
            ))
            .await?;
        }
        txn.execute(Statement::from_sql_and_values(
            backend,
            "UPDATE metadata_fill_journal SET undone_at = ? WHERE id = ?",
            [Value::from(now), Value::from(journal_id)],
        ))
        .await?;
        txn.commit().await?;

        Ok(if still_ours {
            UndoOutcome::Reverted
        } else {
            UndoOutcome::Superseded
        })
    }

    async fn undo_book(&self, batch_id: &str, book_id: i32) -> Result<usize, DomainError> {
        let ids = self
            .db
            .query_all(Statement::from_sql_and_values(
                self.backend(),
                "SELECT id FROM metadata_fill_journal \
                 WHERE batch_id = ? AND book_id = ? AND undone_at IS NULL",
                [Value::from(batch_id.to_string()), Value::from(book_id)],
            ))
            .await?;
        let mut reverted = 0;
        for row in &ids {
            let jid = row.try_get::<i64>("", "id")?;
            if self.undo_field(jid).await? == UndoOutcome::Reverted {
                reverted += 1;
            }
        }
        Ok(reverted)
    }

    async fn undo_run(&self, batch_id: &str) -> Result<usize, DomainError> {
        let ids = self
            .db
            .query_all(Statement::from_sql_and_values(
                self.backend(),
                "SELECT id FROM metadata_fill_journal \
                 WHERE batch_id = ? AND undone_at IS NULL",
                [Value::from(batch_id.to_string())],
            ))
            .await?;
        let mut reverted = 0;
        for row in &ids {
            let jid = row.try_get::<i64>("", "id")?;
            if self.undo_field(jid).await? == UndoOutcome::Reverted {
                reverted += 1;
            }
        }
        Ok(reverted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::Database;

    async fn db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::infrastructure::db::run_migrations(&db)
            .await
            .unwrap();
        db
    }

    /// Insert a book and set the five gap-fill fields explicitly (NULL when None).
    #[allow(clippy::too_many_arguments)]
    async fn seed_book(
        db: &DatabaseConnection,
        title: &str,
        isbn: Option<&str>,
        owned: bool,
        summary: Option<&str>,
        publisher: Option<&str>,
        year: Option<i32>,
        cover: Option<&str>,
        pages: Option<i32>,
    ) -> i32 {
        let now = now_rfc3339();
        db.execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "INSERT INTO books (title, isbn, owned, reading_status, shelf_position, private, \
             summary, publisher, publication_year, cover_url, page_count, uuid, created_at, updated_at) \
             VALUES (?, ?, ?, 'to_read', 0, 0, ?, ?, ?, ?, ?, ?, ?, ?)",
            [
                Value::from(title.to_string()),
                Value::from(isbn.map(|s| s.to_string())),
                Value::from(owned),
                Value::from(summary.map(|s| s.to_string())),
                Value::from(publisher.map(|s| s.to_string())),
                Value::from(year),
                Value::from(cover.map(|s| s.to_string())),
                Value::from(pages),
                // Raw insert bypasses before_save; set a uuid so model reads don't hit NULL.
                Value::from(crate::utils::uuid_gen::new_uuid_v7()),
                Value::from(now.clone()),
                Value::from(now),
            ],
        ))
        .await
        .unwrap();
        let row = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "SELECT last_insert_rowid() AS id".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get::<i32>("", "id").unwrap()
    }

    async fn book_field(db: &DatabaseConnection, id: i32, field: &str) -> Option<String> {
        let row = db
            .query_one(Statement::from_sql_and_values(
                db.get_database_backend(),
                format!("SELECT {field} AS v FROM books WHERE id = ?"),
                [Value::from(id)],
            ))
            .await
            .unwrap()
            .unwrap();
        if is_int_field(field) {
            row.try_get::<Option<i32>>("", "v")
                .unwrap()
                .map(|v| v.to_string())
        } else {
            row.try_get::<Option<String>>("", "v").unwrap()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stats_count_owned_incomplete_and_no_isbn() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        // complete owned book
        seed_book(
            &db,
            "Complete",
            Some("111"),
            true,
            Some("s"),
            Some("p"),
            Some(2000),
            Some("c"),
            Some(100),
        )
        .await;
        // incomplete owned with isbn (missing summary)
        seed_book(
            &db,
            "Incomplete",
            Some("222"),
            true,
            None,
            Some("p"),
            Some(2000),
            Some("c"),
            Some(100),
        )
        .await;
        // incomplete owned without isbn
        seed_book(&db, "NoIsbn", None, true, None, None, None, None, None).await;
        // incomplete but NOT owned (must be excluded everywhere)
        seed_book(
            &db,
            "Borrowed",
            Some("333"),
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await;

        let stats = repo.completeness_stats().await.unwrap();
        assert_eq!(stats.owned_total, 3);
        assert_eq!(stats.incomplete, 2);
        assert_eq!(stats.complete, 1);
        assert_eq!(stats.no_isbn, 1);
        // empty fields: complete=0, incomplete(missing summary)=1, no-isbn(all 5)=5
        assert_eq!(stats.empty_fields, 6);
        assert_eq!(repo.count_incomplete_with_isbn().await.unwrap(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn selection_filters_and_orders() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        let id1 = seed_book(&db, "A", Some("111"), true, None, None, None, None, None).await;
        let id2 = seed_book(&db, "B", Some("222"), true, None, None, None, None, None).await;
        seed_book(&db, "C-noisbn", None, true, None, None, None, None, None).await;
        seed_book(
            &db,
            "D-complete",
            Some("444"),
            true,
            Some("s"),
            Some("p"),
            Some(1),
            Some("c"),
            Some(1),
        )
        .await;

        let with_isbn = repo.list_incomplete_with_isbn(0, 50).await.unwrap();
        assert_eq!(
            with_isbn.iter().map(|b| b.id).collect::<Vec<_>>(),
            vec![id1, id2]
        );

        // after_id cursor excludes already-processed ids
        let after = repo.list_incomplete_with_isbn(id1, 50).await.unwrap();
        assert_eq!(after.iter().map(|b| b.id).collect::<Vec<_>>(), vec![id2]);

        let no_isbn = repo.list_incomplete_without_isbn().await.unwrap();
        assert_eq!(no_isbn.len(), 1);
        assert_eq!(no_isbn[0].title, "C-noisbn");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_incomplete_reports_missing_fields_closest_first() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        // missing only cover (1 gap) -> should sort first
        seed_book(
            &db,
            "OneGap",
            Some("111"),
            true,
            Some("s"),
            Some("p"),
            Some(2000),
            None,
            Some(100),
        )
        .await;
        // missing summary + pages (2 gaps)
        seed_book(
            &db,
            "TwoGaps",
            Some("222"),
            true,
            None,
            Some("p"),
            Some(2000),
            Some("c"),
            None,
        )
        .await;
        // complete -> excluded
        seed_book(
            &db,
            "Done",
            Some("333"),
            true,
            Some("s"),
            Some("p"),
            Some(1),
            Some("c"),
            Some(1),
        )
        .await;
        // not owned -> excluded
        seed_book(
            &db,
            "Borrowed",
            Some("444"),
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await;

        let list = repo.list_incomplete(50).await.unwrap();
        assert_eq!(list.len(), 2);
        // closest-to-complete (fewest gaps) first
        assert_eq!(list[0].title, "OneGap");
        assert_eq!(list[0].missing, vec!["cover_url".to_string()]);
        assert_eq!(list[1].title, "TwoGaps");
        assert_eq!(
            list[1].missing,
            vec!["summary".to_string(), "page_count".to_string()]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn apply_fill_is_none_only() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        // publisher already set; summary/year/pages/cover empty
        let id = seed_book(
            &db,
            "Book",
            Some("111"),
            true,
            None,
            Some("KeepMe"),
            None,
            None,
            None,
        )
        .await;

        let candidate = GapValues {
            summary: Some("New summary".into()),
            publisher: Some("ShouldBeIgnored".into()),
            page_count: Some(321),
            publication_year: Some(1999),
            cover_url: Some("http://cover".into()),
        };
        let filled = repo.apply_fill("batch1", id, candidate).await.unwrap();

        // publisher must NOT be overwritten
        assert_eq!(
            book_field(&db, id, "publisher").await.as_deref(),
            Some("KeepMe")
        );
        assert!(!filled.iter().any(|f| f.field == "publisher"));
        // the four empty fields are filled
        assert_eq!(
            book_field(&db, id, "summary").await.as_deref(),
            Some("New summary")
        );
        assert_eq!(
            book_field(&db, id, "publication_year").await.as_deref(),
            Some("1999")
        );
        assert_eq!(
            book_field(&db, id, "page_count").await.as_deref(),
            Some("321")
        );
        assert_eq!(
            book_field(&db, id, "cover_url").await.as_deref(),
            Some("http://cover")
        );
        assert_eq!(filled.len(), 4);

        // After filling, the book is no longer incomplete (self-draining work-list).
        assert!(
            repo.list_incomplete_with_isbn(0, 50)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn undo_reverts_when_unchanged_and_supersedes_when_edited() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        let id = seed_book(
            &db,
            "Book",
            Some("111"),
            true,
            None,
            None,
            Some(2000),
            Some("c"),
            Some(10),
        )
        .await;

        // fill summary + publisher
        repo.apply_fill(
            "batch1",
            id,
            GapValues {
                summary: Some("auto summary".into()),
                publisher: Some("auto publisher".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // user re-edits the summary, leaves publisher as written
        db.execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "UPDATE books SET summary = ? WHERE id = ?",
            [Value::from("user edit".to_string()), Value::from(id)],
        ))
        .await
        .unwrap();

        let reverted = repo.undo_run("batch1").await.unwrap();
        // only publisher reverts; summary is the user's edit and is left intact
        assert_eq!(reverted, 1);
        assert_eq!(
            book_field(&db, id, "summary").await.as_deref(),
            Some("user edit")
        );
        assert_eq!(book_field(&db, id, "publisher").await, None);

        // both entries retired from the recently-completed list
        assert!(repo.recent_filled(50).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recent_filled_groups_by_book() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        let id = seed_book(
            &db,
            "Grouped",
            Some("111"),
            true,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        repo.apply_fill(
            "b1",
            id,
            GapValues {
                summary: Some("s".into()),
                publisher: Some("p".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let recent = repo.recent_filled(50).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].book_id, id);
        assert_eq!(recent[0].title, "Grouped");
        assert_eq!(recent[0].fields.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_lifecycle_and_interrupt() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        repo.create_run("b1", 5).await.unwrap();
        let mut run = repo.get_run("b1").await.unwrap().unwrap();
        assert_eq!(run.status, "running");
        assert_eq!(run.total, 5);

        run.done = 2;
        run.filled = 1;
        run.cursor_book_id = 42;
        run.current_title = Some("Current".into());
        repo.update_run_progress(&run).await.unwrap();
        let reloaded = repo.get_run("b1").await.unwrap().unwrap();
        assert_eq!(reloaded.done, 2);
        assert_eq!(reloaded.cursor_book_id, 42);

        // simulate a kill: a leftover running run becomes resumable
        repo.mark_running_as_interrupted().await.unwrap();
        let active = repo.get_active_run().await.unwrap().unwrap();
        assert_eq!(active.status, "interrupted");
        assert_eq!(active.cursor_book_id, 42);
    }
}
