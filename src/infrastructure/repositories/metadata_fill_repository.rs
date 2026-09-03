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
    CompletenessStats, FieldGap, FillRun, FilledField, GapValues, IncompleteBook,
    IncompleteBookDetail, MetadataFillRepository, RecentFilledBook, RecentFilledField, UndoOutcome,
    is_fill_field, is_journal_field,
};

/// A book counts as "incomplete" when any gap-fill field is empty.
/// Text fields treat NULL or whitespace-only as empty; integer fields treat
/// NULL as empty. Kept as one fragment so stat/selection/total stay consistent.
///
/// `title` is a gap like any other here: it is `NOT NULL` in the schema, so a
/// missing one is the empty string, and it is the gap the owner most needs to
/// see (nothing else in the app reports it).
const INCOMPLETE_PRED: &str = "(TRIM(title) = '' \
     OR summary IS NULL OR TRIM(summary) = '' \
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

/// SQL fragment restricting a selection to the books missing one given field
/// (the run scope), as ` AND (...)` ready to append. `None` yields an empty
/// fragment. The field is whitelisted before interpolation; an unknown one is
/// an error rather than a silently unscoped run.
///
/// The emptiness test per field type mirrors `INCOMPLETE_PRED` exactly, so a
/// scoped run can never pick a book the unscoped one would consider complete.
fn missing_field_pred(field: Option<&str>) -> Result<String, DomainError> {
    let Some(field) = field else {
        return Ok(String::new());
    };
    if !is_fill_field(field) {
        return Err(DomainError::Validation(format!(
            "unknown gap-fill field: {field}"
        )));
    }
    Ok(if field == "title" {
        " AND TRIM(title) = ''".to_string()
    } else if is_int_field(field) {
        format!(" AND {field} IS NULL")
    } else {
        format!(" AND ({field} IS NULL OR TRIM({field}) = '')")
    })
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

    /// Owned books still missing each gap-fill field, in `FILL_FIELDS` order,
    /// using the same emptiness rules as `INCOMPLETE_PRED` (text: NULL/blank;
    /// integer: NULL). One query: the total shown by the completeness teaser is
    /// the sum of these, so the two can never disagree.
    async fn field_gaps(&self) -> Result<Vec<FieldGap>, DomainError> {
        let sql = "SELECT \
             COALESCE(SUM(CASE WHEN TRIM(title) = '' THEN 1 ELSE 0 END), 0) AS title, \
             COALESCE(SUM(CASE WHEN summary IS NULL OR TRIM(summary) = '' THEN 1 ELSE 0 END), 0) \
                 AS summary, \
             COALESCE(SUM(CASE WHEN publisher IS NULL OR TRIM(publisher) = '' THEN 1 ELSE 0 END), 0) \
                 AS publisher, \
             COALESCE(SUM(CASE WHEN page_count IS NULL THEN 1 ELSE 0 END), 0) AS page_count, \
             COALESCE(SUM(CASE WHEN publication_year IS NULL THEN 1 ELSE 0 END), 0) \
                 AS publication_year, \
             COALESCE(SUM(CASE WHEN cover_url IS NULL OR TRIM(cover_url) = '' THEN 1 ELSE 0 END), 0) \
                 AS cover_url \
             FROM books WHERE owned = 1";
        let row = self
            .db
            .query_one(Statement::from_string(self.backend(), sql.to_owned()))
            .await?
            .ok_or_else(|| DomainError::Database("field_gaps returned no row".into()))?;
        crate::domain::metadata_fill::FILL_FIELDS
            .iter()
            .map(|field| {
                Ok(FieldGap {
                    field: (*field).to_string(),
                    missing: row.try_get::<i64>("", field)?,
                })
            })
            .collect()
    }
}

fn row_to_incomplete(row: &sea_orm::QueryResult) -> Result<IncompleteBook, DomainError> {
    Ok(IncompleteBook {
        id: row.try_get::<String>("", "id")?,
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
        cursor_book_id: row.try_get::<String>("", "cursor_book_id")?,
        current_title: row.try_get::<Option<String>>("", "current_title")?,
        missing_field: row.try_get::<Option<String>>("", "missing_field")?,
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
        let gaps = self.field_gaps().await?;
        Ok(CompletenessStats {
            owned_total,
            complete: owned_total - incomplete,
            incomplete,
            no_isbn,
            empty_fields: gaps.iter().map(|g| g.missing).sum(),
            gaps,
        })
    }

    async fn list_incomplete_with_isbn(
        &self,
        after_id: &str,
        limit: u64,
        missing_field: Option<&str>,
    ) -> Result<Vec<IncompleteBook>, DomainError> {
        let scope = missing_field_pred(missing_field)?;
        let sql = format!(
            "SELECT uuid AS id, title, isbn FROM books \
             WHERE owned = 1 AND {INCOMPLETE_PRED} AND {HAS_ISBN_PRED}{scope} AND uuid > ? \
             ORDER BY uuid ASC LIMIT ?"
        );
        let rows = self
            .db
            .query_all(Statement::from_sql_and_values(
                self.backend(),
                sql,
                [Value::from(after_id.to_string()), Value::from(limit as i64)],
            ))
            .await?;
        rows.iter().map(row_to_incomplete).collect()
    }

    async fn count_covers_sources_have_not(&self) -> Result<i64, DomainError> {
        // `book_local` is device-local and not a CRR: the marker says what THIS
        // device's enabled sources answered, which is exactly the scope of the
        // explanation shown to the reader.
        let sql = "SELECT COUNT(*) AS cnt FROM books b \
             JOIN book_local bl ON bl.book_uuid = b.uuid \
             WHERE b.owned = 1 \
               AND (b.cover_url IS NULL OR TRIM(b.cover_url) = '') \
               AND bl.cover_lookup_failed_at IS NOT NULL";
        let row = self
            .db
            .query_one(Statement::from_string(self.backend(), sql.to_owned()))
            .await?
            .ok_or_else(|| DomainError::Database("cover marker count returned no row".into()))?;
        Ok(row.try_get::<i64>("", "cnt")?)
    }

    async fn count_incomplete_with_isbn(
        &self,
        missing_field: Option<&str>,
    ) -> Result<i64, DomainError> {
        let scope = missing_field_pred(missing_field)?;
        self.count(&format!(
            "owned = 1 AND {INCOMPLETE_PRED} AND {HAS_ISBN_PRED}{scope}"
        ))
        .await
    }

    async fn list_incomplete_without_isbn(&self) -> Result<Vec<IncompleteBook>, DomainError> {
        let sql = format!(
            "SELECT uuid AS id, title, isbn FROM books \
             WHERE owned = 1 AND {INCOMPLETE_PRED} AND {NO_ISBN_PRED} \
             ORDER BY uuid ASC"
        );
        let rows = self
            .db
            .query_all(Statement::from_string(self.backend(), sql))
            .await?;
        rows.iter().map(row_to_incomplete).collect()
    }

    async fn list_incomplete(
        &self,
        limit: u64,
        missing_field: Option<&str>,
        no_isbn_only: bool,
    ) -> Result<Vec<IncompleteBookDetail>, DomainError> {
        let scope = missing_field_pred(missing_field)?;
        let isbn = if no_isbn_only {
            format!(" AND {NO_ISBN_PRED}")
        } else {
            String::new()
        };
        let sql = format!(
            "SELECT uuid AS id, title, isbn, cover_url, summary, publisher, publication_year, page_count \
             FROM books WHERE owned = 1 AND {INCOMPLETE_PRED}{scope}{isbn} ORDER BY title ASC LIMIT ?"
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
            let title = row.try_get::<String>("", "title")?;
            let summary = row.try_get::<Option<String>>("", "summary")?;
            let publisher = row.try_get::<Option<String>>("", "publisher")?;
            let cover = row.try_get::<Option<String>>("", "cover_url")?;
            let year = row.try_get::<Option<i32>>("", "publication_year")?;
            let pages = row.try_get::<Option<i32>>("", "page_count")?;

            let mut missing = Vec::new();
            // Title first: it is the gap that makes the row unidentifiable,
            // and the list is read top-down.
            if title.trim().is_empty() {
                missing.push("title".to_string());
            }
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
                id: row.try_get::<String>("", "id")?,
                title,
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
        book_id: &str,
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
                "SELECT title, summary, publisher, publication_year, cover_url, page_count, isbn \
                 FROM books WHERE uuid = ?",
                [Value::from(book_id.to_string())],
            ))
            .await?;
        let Some(row) = row else {
            // Book vanished (deleted concurrently): nothing to do.
            txn.rollback().await?;
            return Ok(vec![]);
        };

        let cur_title = row.try_get::<String>("", "title")?;
        let cur_summary = row.try_get::<Option<String>>("", "summary")?;
        let cur_publisher = row.try_get::<Option<String>>("", "publisher")?;
        let cur_year = row.try_get::<Option<i32>>("", "publication_year")?;
        let cur_cover = row.try_get::<Option<String>>("", "cover_url")?;
        let cur_pages = row.try_get::<Option<i32>>("", "page_count")?;
        let cur_isbn = row.try_get::<Option<String>>("", "isbn")?;

        let text_empty = |v: &Option<String>| v.as_deref().map(str::trim).unwrap_or("").is_empty();

        let mut filled: Vec<FilledField> = Vec::new();
        let now = now_rfc3339();

        // Each entry: (field, is-empty, value-to-write-as-Value, value-string).
        let mut writes: Vec<(&str, Value, String)> = Vec::new();
        // Title is NOT NULL, so its "empty" is the blank string rather than
        // NULL; the None-only invariant is otherwise identical.
        if cur_title.trim().is_empty()
            && let Some(v) = candidate.title.filter(|s| !s.trim().is_empty())
        {
            writes.push(("title", Value::from(v.clone()), v));
        }
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
        // The ISBN is absent when NULL *or* blank: migration 057 normalized the
        // empty strings away, but a sync from an older device can still land one
        // (`book_dedup_key` applies the same rule).
        if text_empty(&cur_isbn)
            && let Some(v) = candidate.isbn.filter(|s| !s.trim().is_empty())
        {
            writes.push(("isbn", Value::from(v.clone()), v));
        }

        for (field, value, value_str) in writes {
            // `field` is a compile-time literal from the set above; never user input.
            txn.execute(Statement::from_sql_and_values(
                backend,
                format!("UPDATE books SET {field} = ?, updated_at = ? WHERE uuid = ?"),
                [
                    value,
                    Value::from(now.clone()),
                    Value::from(book_id.to_string()),
                ],
            ))
            .await?;
            txn.execute(Statement::from_sql_and_values(
                backend,
                "INSERT INTO metadata_fill_journal \
                 (batch_id, book_id, field, value_set, created_at) VALUES (?, ?, ?, ?, ?)",
                [
                    Value::from(batch_id.to_string()),
                    Value::from(book_id.to_string()),
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

    async fn create_run(
        &self,
        batch_id: &str,
        total: i64,
        missing_field: Option<&str>,
    ) -> Result<(), DomainError> {
        if let Some(field) = missing_field
            && !is_fill_field(field)
        {
            return Err(DomainError::Validation(format!(
                "unknown gap-fill field: {field}"
            )));
        }
        let now = now_rfc3339();
        self.db
            .execute(Statement::from_sql_and_values(
                self.backend(),
                "INSERT INTO metadata_fill_run \
                 (batch_id, status, total, done, filled, skipped, errored, cursor_book_id, \
                  current_title, started_at, updated_at, missing_field) \
                 VALUES (?, 'running', ?, 0, 0, 0, 0, '', NULL, ?, ?, ?)",
                [
                    Value::from(batch_id.to_string()),
                    Value::from(total),
                    Value::from(now.clone()),
                    Value::from(now),
                    Value::from(missing_field.map(|f| f.to_string())),
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
                    Value::from(run.cursor_book_id.clone()),
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
        //
        // An inner join, deliberately: this table has no declared foreign key,
        // so deleting a book leaves its entries behind. Under a left join they
        // surfaced as a list of untitled cards offering to revert a field on a
        // book that is gone, and an "undo the whole run" on a run whose books
        // no longer exist. What has no book to return to is not undoable.
        let rows = self
            .db
            .query_all(Statement::from_string(
                self.backend(),
                "SELECT j.id AS jid, j.batch_id AS batch_id, j.book_id AS book_id, \
                 j.field AS field, j.value_set AS value_set, j.created_at AS created_at, \
                 b.title AS title, b.cover_url AS cover_url \
                 FROM metadata_fill_journal j JOIN books b ON b.uuid = j.book_id \
                 WHERE j.undone_at IS NULL ORDER BY j.created_at DESC, j.id DESC"
                    .to_owned(),
            ))
            .await?;

        let mut out: Vec<RecentFilledBook> = Vec::new();
        for row in &rows {
            let book_id = row.try_get::<String>("", "book_id")?;
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

        let book_id = entry.try_get::<String>("", "book_id")?;
        let field = entry.try_get::<String>("", "field")?;
        let value_set = entry.try_get::<String>("", "value_set")?;
        // The journal may name a field a gap-fill run never writes (the ISBN of
        // a reimport), so the undo whitelist is the wider one. It is still the
        // only gate before this name reaches SQL interpolation below.
        if !is_journal_field(&field) {
            txn.rollback().await?;
            return Err(DomainError::Validation(format!("unknown field: {field}")));
        }

        // Read the book's current value in string form for the "still ours" test.
        let book_row = txn
            .query_one(Statement::from_sql_and_values(
                backend,
                format!("SELECT {field} AS val FROM books WHERE uuid = ?"),
                [Value::from(book_id.clone())],
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
            // `books.title` is NOT NULL: reverting it to NULL would abort the
            // transaction. Its empty form is the blank string, which is
            // exactly the state the fill found it in.
            let empty_value = if field == "title" {
                Value::from(String::new())
            } else {
                Value::from(Option::<String>::None)
            };
            txn.execute(Statement::from_sql_and_values(
                backend,
                format!("UPDATE books SET {field} = ?, updated_at = ? WHERE uuid = ?"),
                [empty_value, Value::from(now.clone()), Value::from(book_id)],
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

    async fn undo_book(&self, batch_id: &str, book_id: &str) -> Result<usize, DomainError> {
        let ids = self
            .db
            .query_all(Statement::from_sql_and_values(
                self.backend(),
                "SELECT id FROM metadata_fill_journal \
                 WHERE batch_id = ? AND book_id = ? AND undone_at IS NULL",
                [
                    Value::from(batch_id.to_string()),
                    Value::from(book_id.to_string()),
                ],
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

    /// Insert a book with an explicit title and the five lookup-fillable
    /// fields (NULL when None). Pass an empty title to seed the title gap.
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
    ) -> String {
        let now = now_rfc3339();
        // The books PK is now the uuid String; generate it here, bind it, and
        // return it (do not rely on last_insert_rowid — there is no integer id).
        let uuid = crate::utils::uuid_gen::new_uuid_v7();
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
                Value::from(uuid.clone()),
                Value::from(now.clone()),
                Value::from(now),
            ],
        ))
        .await
        .unwrap();
        uuid
    }

    async fn book_field(db: &DatabaseConnection, id: &str, field: &str) -> Option<String> {
        let row = db
            .query_one(Statement::from_sql_and_values(
                db.get_database_backend(),
                format!("SELECT {field} AS v FROM books WHERE uuid = ?"),
                [Value::from(id.to_string())],
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
        assert_eq!(repo.count_incomplete_with_isbn(None).await.unwrap(), 1);
    }

    /// A book whose title is empty is incomplete even when every other field
    /// is filled. Nothing else in the app tells the owner about it: it is the
    /// row that renders as a blank tile, here and on any peer that caches it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_title_counts_as_a_gap() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        seed_book(
            &db,
            "",
            Some("9782070612918"),
            true,
            Some("s"),
            Some("p"),
            Some(2000),
            Some("c"),
            Some(100),
        )
        .await;

        let stats = repo.completeness_stats().await.unwrap();
        assert_eq!(stats.incomplete, 1, "a title-less book is incomplete");
        assert_eq!(stats.complete, 0);
        assert_eq!(stats.empty_fields, 1, "exactly one gap: the title");

        let detail = repo.list_incomplete(50, None, false).await.unwrap();
        assert_eq!(detail.len(), 1);
        assert_eq!(detail[0].missing, vec!["title".to_string()]);

        // It has an ISBN, so the bulk run can look it up and repair it.
        assert_eq!(repo.count_incomplete_with_isbn(None).await.unwrap(), 1);
    }

    /// A whitespace-only title is a missing title, matching the creation gate
    /// in `book_service::validate_title`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_blank_title_counts_as_a_gap() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        seed_book(
            &db,
            "   ",
            Some("111"),
            true,
            Some("s"),
            Some("p"),
            Some(2000),
            Some("c"),
            Some(100),
        )
        .await;

        assert_eq!(repo.completeness_stats().await.unwrap().incomplete, 1);
    }

    /// The bulk run fills a missing title from the ISBN lookup like any other
    /// gap. Without this the book would never leave the work-list: it would be
    /// re-looked-up on every run and stay incomplete forever.
    #[tokio::test(flavor = "multi_thread")]
    async fn apply_fill_writes_a_missing_title_and_undo_restores_it_empty() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        let id = seed_book(
            &db,
            "",
            Some("9782070612918"),
            true,
            Some("s"),
            Some("p"),
            Some(2000),
            Some("c"),
            Some(100),
        )
        .await;

        let filled = repo
            .apply_fill(
                "batch-1",
                &id,
                GapValues {
                    title: Some("Le Mythe de Sisyphe".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].field, "title");
        assert_eq!(
            book_field(&db, &id, "title").await.as_deref(),
            Some("Le Mythe de Sisyphe"),
        );
        assert_eq!(
            repo.completeness_stats().await.unwrap().incomplete,
            0,
            "the filled book drains out of the work-list",
        );

        // Undoing must land on the empty string: `books.title` is NOT NULL, so
        // the generic revert-to-NULL would fail and leave the journal entry
        // retired against an unchanged book.
        let reverted = repo.undo_book("batch-1", &id).await.unwrap();
        assert_eq!(reverted, 1);
        assert_eq!(book_field(&db, &id, "title").await.as_deref(), Some(""));
    }

    /// A journal entry whose book was deleted has nothing to revert: it must
    /// leave the undo list rather than show up as an untitled card. The table
    /// carries no foreign key, so the rows do survive the book.
    #[tokio::test(flavor = "multi_thread")]
    async fn recent_filled_drops_entries_whose_book_is_gone() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        let kept = seed_book(&db, "Kept", None, true, None, None, None, None, None).await;
        let doomed = seed_book(&db, "Doomed", None, true, None, None, None, None, None).await;

        for id in [&kept, &doomed] {
            repo.apply_fill(
                "batch-1",
                id,
                GapValues {
                    publisher: Some("Folio".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        assert_eq!(repo.recent_filled(50).await.unwrap().len(), 2);

        db.execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "DELETE FROM books WHERE uuid = ?",
            [Value::from(doomed.clone())],
        ))
        .await
        .unwrap();

        let recent = repo.recent_filled(50).await.unwrap();
        assert_eq!(recent.len(), 1, "the deleted book leaves the undo list");
        assert_eq!(recent[0].book_id, kept);
    }

    /// The ISBN is journalled and undoable like any other field, even though it
    /// is not a gap-fill field (ADR-071 D6).
    #[tokio::test(flavor = "multi_thread")]
    async fn apply_fill_writes_a_missing_isbn_and_undo_clears_it() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        let id = seed_book(
            &db,
            "Martin Eden",
            None,
            true,
            Some("s"),
            Some("p"),
            Some(1909),
            Some("c"),
            Some(100),
        )
        .await;

        let filled = repo
            .apply_fill(
                "batch-isbn",
                &id,
                GapValues {
                    isbn: Some("9782264024848".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].field, "isbn");
        assert_eq!(
            book_field(&db, &id, "isbn").await.as_deref(),
            Some("9782264024848"),
        );

        // The undo whitelist has to accept a field FILL_FIELDS does not carry,
        // or the reimport would be journalled and not reversible.
        let reverted = repo.undo_run("batch-isbn").await.unwrap();
        assert_eq!(reverted, 1);
        assert_eq!(book_field(&db, &id, "isbn").await, None);
    }

    /// A book that carries an ISBN keeps it: the reimport fills absences only.
    #[tokio::test(flavor = "multi_thread")]
    async fn apply_fill_never_overwrites_an_existing_isbn() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        let id = seed_book(
            &db,
            "Dune",
            Some("9780441013593"),
            true,
            None,
            None,
            None,
            None,
            None,
        )
        .await;

        let filled = repo
            .apply_fill(
                "batch-isbn",
                &id,
                GapValues {
                    isbn: Some("9780441172719".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(filled.is_empty());
        assert_eq!(
            book_field(&db, &id, "isbn").await.as_deref(),
            Some("9780441013593"),
        );
    }

    /// A blank ISBN is an absence (migration 057), and a book that only lacks
    /// its ISBN is NOT incomplete: filling it must not move the completeness
    /// figures, which are what FILL_FIELDS defines.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_blank_isbn_is_filled_without_touching_the_completeness_stat() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        let id = seed_book(
            &db,
            "Complete but unidentified",
            Some("   "),
            true,
            Some("s"),
            Some("p"),
            Some(2000),
            Some("c"),
            Some(100),
        )
        .await;

        let before = repo.completeness_stats().await.unwrap();
        assert_eq!(before.incomplete, 0, "no gap-fill field is empty");
        assert_eq!(before.no_isbn, 0, "no_isbn only counts incomplete books");

        let filled = repo
            .apply_fill(
                "batch-isbn",
                &id,
                GapValues {
                    isbn: Some("9782264024848".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(filled.len(), 1, "blank counts as absent");

        let after = repo.completeness_stats().await.unwrap();
        assert_eq!(
            after, before,
            "the ISBN is journalled, not part of the completeness definition"
        );
    }

    /// The None-only invariant holds for the title too: a real title is never
    /// replaced by whatever an ISBN lookup believes the book is called.
    #[tokio::test(flavor = "multi_thread")]
    async fn apply_fill_never_overwrites_an_existing_title() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        let id = seed_book(
            &db,
            "Ma reliure maison",
            None,
            true,
            None,
            None,
            None,
            None,
            None,
        )
        .await;

        let filled = repo
            .apply_fill(
                "batch-1",
                &id,
                GapValues {
                    title: Some("Something Else".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert!(filled.is_empty());
        assert_eq!(
            book_field(&db, &id, "title").await.as_deref(),
            Some("Ma reliure maison"),
        );
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

        // The work-list orders by uuid; uuid v7 is not strictly ordered within a
        // millisecond, so derive the expected order from the ids themselves
        // rather than assuming insertion order.
        let (lo, hi) = if id1 < id2 {
            (id1.clone(), id2.clone())
        } else {
            (id2.clone(), id1.clone())
        };

        let with_isbn = repo.list_incomplete_with_isbn("", 50, None).await.unwrap();
        assert_eq!(
            with_isbn.iter().map(|b| b.id.clone()).collect::<Vec<_>>(),
            vec![lo.clone(), hi.clone()]
        );

        // after_id cursor excludes already-processed ids (everything <= lo)
        let after = repo.list_incomplete_with_isbn(&lo, 50, None).await.unwrap();
        assert_eq!(
            after.iter().map(|b| b.id.clone()).collect::<Vec<_>>(),
            vec![hi]
        );

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

        let list = repo.list_incomplete(50, None, false).await.unwrap();
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
            title: None,
            summary: Some("New summary".into()),
            publisher: Some("ShouldBeIgnored".into()),
            page_count: Some(321),
            publication_year: Some(1999),
            cover_url: Some("http://cover".into()),
            isbn: None,
        };
        let filled = repo.apply_fill("batch1", &id, candidate).await.unwrap();

        // publisher must NOT be overwritten
        assert_eq!(
            book_field(&db, &id, "publisher").await.as_deref(),
            Some("KeepMe")
        );
        assert!(!filled.iter().any(|f| f.field == "publisher"));
        // the four empty fields are filled
        assert_eq!(
            book_field(&db, &id, "summary").await.as_deref(),
            Some("New summary")
        );
        assert_eq!(
            book_field(&db, &id, "publication_year").await.as_deref(),
            Some("1999")
        );
        assert_eq!(
            book_field(&db, &id, "page_count").await.as_deref(),
            Some("321")
        );
        assert_eq!(
            book_field(&db, &id, "cover_url").await.as_deref(),
            Some("http://cover")
        );
        assert_eq!(filled.len(), 4);

        // After filling, the book is no longer incomplete (self-draining work-list).
        assert!(
            repo.list_incomplete_with_isbn("", 50, None)
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
            &id,
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
            "UPDATE books SET summary = ? WHERE uuid = ?",
            [
                Value::from("user edit".to_string()),
                Value::from(id.clone()),
            ],
        ))
        .await
        .unwrap();

        let reverted = repo.undo_run("batch1").await.unwrap();
        // only publisher reverts; summary is the user's edit and is left intact
        assert_eq!(reverted, 1);
        assert_eq!(
            book_field(&db, &id, "summary").await.as_deref(),
            Some("user edit")
        );
        assert_eq!(book_field(&db, &id, "publisher").await, None);

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
            &id,
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

    /// The "the sources have no cover for these" count must key on the marker
    /// AND on the cover still being empty: a book whose cover arrived later
    /// carries a stale marker until the next sweep clears it, and counting it
    /// would overstate the explanation.
    #[tokio::test(flavor = "multi_thread")]
    async fn covers_the_sources_have_not_counts_marked_and_still_empty_books() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        let marked_empty = seed_book(
            &db,
            "No cover",
            Some("111"),
            true,
            Some("S"),
            Some("P"),
            Some(2001),
            None,
            Some(10),
        )
        .await;
        let marked_but_covered = seed_book(
            &db,
            "Cover arrived since",
            Some("222"),
            true,
            Some("S"),
            Some("P"),
            Some(2002),
            Some("https://example.org/c.jpg"),
            Some(20),
        )
        .await;
        // Coverless but never asked: not part of the explanation.
        seed_book(
            &db,
            "Never asked",
            Some("333"),
            true,
            Some("S"),
            Some("P"),
            Some(2003),
            None,
            Some(30),
        )
        .await;
        for id in [&marked_empty, &marked_but_covered] {
            crate::infrastructure::book_local::set_cover_lookup_failed_at(
                &db,
                id,
                "2026-08-01T00:00:00Z",
            )
            .await
            .unwrap();
        }

        assert_eq!(repo.count_covers_sources_have_not().await.unwrap(), 1);
    }

    /// The overview list is capped, so the cap must apply to the *filtered*
    /// set: a filter that announces books the list cannot show is a filter that
    /// looks broken.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_overview_list_takes_the_same_filters_as_the_pills() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        seed_book(
            &db,
            "Missing summary",
            Some("111"),
            true,
            None,
            Some("Pub"),
            Some(2001),
            Some("c"),
            Some(100),
        )
        .await;
        seed_book(
            &db,
            "Missing publisher, no isbn",
            None,
            true,
            Some("S"),
            None,
            Some(2002),
            Some("c"),
            Some(120),
        )
        .await;

        let all = repo.list_incomplete(50, None, false).await.unwrap();
        assert_eq!(all.len(), 2);

        let scoped = repo
            .list_incomplete(50, Some("summary"), false)
            .await
            .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].title, "Missing summary");

        let no_isbn = repo.list_incomplete(50, None, true).await.unwrap();
        assert_eq!(no_isbn.len(), 1);
        assert_eq!(no_isbn[0].title, "Missing publisher, no isbn");

        assert!(
            repo.list_incomplete(50, Some("nope"), false).await.is_err(),
            "an unknown field is refused here too"
        );
    }

    /// The teaser reads its number from the per-field breakdown, so the
    /// breakdown must be exact and its sum must be the total it replaces.
    #[tokio::test(flavor = "multi_thread")]
    async fn stats_break_the_empty_fields_down_per_field() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        // Missing summary + cover (blank counts as missing).
        seed_book(
            &db,
            "A",
            Some("111"),
            true,
            None,
            Some("Pub"),
            Some(2001),
            Some("  "),
            Some(100),
        )
        .await;
        // Missing summary + year.
        seed_book(
            &db,
            "B",
            Some("222"),
            true,
            Some(""),
            Some("Pub"),
            None,
            Some("c"),
            Some(120),
        )
        .await;
        // Not owned: out of every count.
        seed_book(&db, "C", Some("333"), false, None, None, None, None, None).await;

        let stats = repo.completeness_stats().await.unwrap();
        let gap = |f: &str| {
            stats
                .gaps
                .iter()
                .find(|g| g.field == f)
                .map(|g| g.missing)
                .unwrap()
        };
        assert_eq!(gap("summary"), 2);
        assert_eq!(gap("cover_url"), 1);
        assert_eq!(gap("publication_year"), 1);
        assert_eq!(gap("publisher"), 0);
        assert_eq!(gap("title"), 0);
        assert_eq!(
            stats.empty_fields,
            stats.gaps.iter().map(|g| g.missing).sum::<i64>(),
            "the teaser total is the sum of the breakdown"
        );
    }

    /// A scoped run walks only the books missing the field it was scoped to,
    /// with the same "empty" definition as the unscoped work-list (a
    /// whitespace-only value is a gap), and still skips the books with no ISBN.
    #[tokio::test(flavor = "multi_thread")]
    async fn work_list_can_be_scoped_to_one_missing_field() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        // Missing only the summary (blank, not NULL: still a gap).
        let no_summary = seed_book(
            &db,
            "No summary",
            Some("111"),
            true,
            Some("   "),
            Some("Pub"),
            Some(2001),
            Some("c"),
            Some(100),
        )
        .await;
        // Missing only the publisher: out of a summary-scoped run.
        seed_book(
            &db,
            "No publisher",
            Some("222"),
            true,
            Some("S"),
            None,
            Some(2002),
            Some("c"),
            Some(120),
        )
        .await;
        // Missing the summary but with no ISBN: never processable.
        seed_book(
            &db,
            "No isbn",
            None,
            true,
            None,
            Some("Pub"),
            Some(2003),
            Some("c"),
            Some(90),
        )
        .await;
        // Missing the publication year: exercises the integer-typed scope.
        let no_year = seed_book(
            &db,
            "No year",
            Some("333"),
            true,
            Some("S"),
            Some("Pub"),
            None,
            Some("c"),
            Some(80),
        )
        .await;

        let unscoped = repo.list_incomplete_with_isbn("", 50, None).await.unwrap();
        assert_eq!(unscoped.len(), 3, "three incomplete books have an ISBN");

        let scoped = repo
            .list_incomplete_with_isbn("", 50, Some("summary"))
            .await
            .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, no_summary);
        assert_eq!(
            repo.count_incomplete_with_isbn(Some("summary"))
                .await
                .unwrap(),
            1,
            "the announced count matches what the work-list will walk"
        );

        let scoped_year = repo
            .list_incomplete_with_isbn("", 50, Some("publication_year"))
            .await
            .unwrap();
        assert_eq!(scoped_year.len(), 1);
        assert_eq!(scoped_year[0].id, no_year);
    }

    /// The scope reaches SQL by interpolation, so anything outside the
    /// gap-fill whitelist must be refused rather than silently dropped.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_scope_is_refused_everywhere() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        let hostile = Some("summary) OR 1=1 --");
        assert!(
            repo.list_incomplete_with_isbn("", 50, hostile)
                .await
                .is_err()
        );
        assert!(repo.count_incomplete_with_isbn(hostile).await.is_err());
        assert!(repo.create_run("bad", 0, hostile).await.is_err());
    }

    /// The scope is persisted with the run: the resume cursor only means
    /// anything against the work-list it was built from.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_remembers_its_scope() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        repo.create_run("scoped", 7, Some("summary")).await.unwrap();
        let run = repo.get_active_run().await.unwrap().unwrap();
        assert_eq!(run.missing_field.as_deref(), Some("summary"));

        repo.set_run_status("scoped", "done").await.unwrap();
        repo.create_run("whole", 9, None).await.unwrap();
        let run = repo.get_active_run().await.unwrap().unwrap();
        assert_eq!(run.missing_field, None, "an unscoped run stores no field");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_lifecycle_and_interrupt() {
        let db = db().await;
        let repo = SeaOrmMetadataFillRepository::new(db.clone());
        repo.create_run("b1", 5, None).await.unwrap();
        let mut run = repo.get_run("b1").await.unwrap().unwrap();
        assert_eq!(run.status, "running");
        assert_eq!(run.total, 5);

        run.done = 2;
        run.filled = 1;
        // The cursor is now a book uuid String (not an int). Use a real uuid so it
        // keeps TEXT affinity in the INTEGER column and round-trips as a String.
        let cursor = crate::utils::uuid_gen::new_uuid_v7();
        run.cursor_book_id = cursor.clone();
        run.current_title = Some("Current".into());
        repo.update_run_progress(&run).await.unwrap();
        let reloaded = repo.get_run("b1").await.unwrap().unwrap();
        assert_eq!(reloaded.done, 2);
        assert_eq!(reloaded.cursor_book_id, cursor);

        // simulate a kill: a leftover running run becomes resumable
        repo.mark_running_as_interrupted().await.unwrap();
        let active = repo.get_active_run().await.unwrap().unwrap();
        assert_eq!(active.status, "interrupted");
        assert_eq!(active.cursor_book_id, cursor);
    }
}
