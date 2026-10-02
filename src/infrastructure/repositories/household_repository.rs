//! SeaORM implementation of HouseholdRepository (see `domain::household`).
//!
//! The repository borrows its connection instead of living in `AppState`: the
//! book write paths record a reading inside their own transaction, and the FFI
//! surface has no `AppState`, so each caller builds one over the connection or
//! the transaction it holds.

use std::collections::HashMap;

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DbErr, Statement};

use crate::domain::{DomainError, HouseholdRepository, Reader, Reading};
use crate::models::Book;

pub struct SeaOrmHouseholdRepository<'a, C: ConnectionTrait> {
    db: &'a C,
}

impl<'a, C: ConnectionTrait> SeaOrmHouseholdRepository<'a, C> {
    pub fn new(db: &'a C) -> Self {
        Self { db }
    }

    /// Lay the current reader's state over `books`, best-effort: a failure
    /// leaves the books as stored, the household-wide answer, rather than
    /// failing the read.
    pub async fn overlay_or_stored(&self, books: &mut [Book]) {
        if let Err(e) = self.overlay(books).await {
            tracing::warn!("household overlay skipped: {e}");
        }
    }

    async fn execute(&self, sql: &str, values: Vec<sea_orm::Value>) -> Result<(), DbErr> {
        self.db
            .execute(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                sql,
                values,
            ))
            .await?;
        Ok(())
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn reading_from_row(row: &sea_orm::QueryResult) -> Result<(String, Reading), DbErr> {
    Ok((
        row.try_get("", "book_uuid")?,
        Reading {
            reading_status: row.try_get("", "reading_status")?,
            started_reading_at: row.try_get("", "started_reading_at")?,
            finished_reading_at: row.try_get("", "finished_reading_at")?,
            user_rating: row.try_get("", "user_rating")?,
        },
    ))
}

#[async_trait]
impl<C: ConnectionTrait> HouseholdRepository for SeaOrmHouseholdRepository<'_, C> {
    async fn list_readers(&self) -> Result<Vec<Reader>, DomainError> {
        let rows = self
            .db
            .query_all(Statement::from_string(
                self.db.get_database_backend(),
                "SELECT id, name FROM readers ORDER BY created_at, id".to_owned(),
            ))
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                Ok(Reader {
                    id: r.try_get("", "id")?,
                    name: r.try_get("", "name")?,
                })
            })
            .collect::<Result<_, DbErr>>()?)
    }

    async fn current_reader_id(&self) -> Result<Option<String>, DomainError> {
        let row = self
            .db
            .query_one(Statement::from_string(
                self.db.get_database_backend(),
                "SELECT reader_id FROM reader_local WHERE id = 1".to_owned(),
            ))
            .await?;
        Ok(row
            .map(|r| r.try_get::<String>("", "reader_id"))
            .transpose()?)
    }

    async fn store_current_reader_id(&self, reader_id: &str) -> Result<(), DomainError> {
        Ok(self
            .execute(
                "INSERT INTO reader_local (id, reader_id) VALUES (1, ?) \
                 ON CONFLICT(id) DO UPDATE SET reader_id = excluded.reader_id",
                vec![reader_id.into()],
            )
            .await?)
    }

    async fn clear_current_reader(&self) -> Result<(), DomainError> {
        Ok(self.execute("DELETE FROM reader_local", vec![]).await?)
    }

    async fn insert_reader(&self, reader: &Reader) -> Result<(), DomainError> {
        Ok(self
            .execute(
                "INSERT INTO readers (id, name, created_at) VALUES (?, ?, ?)",
                vec![
                    reader.id.clone().into(),
                    reader.name.clone().into(),
                    now().into(),
                ],
            )
            .await?)
    }

    async fn store_reader_name(&self, reader_id: &str, name: &str) -> Result<(), DomainError> {
        Ok(self
            .execute(
                "UPDATE readers SET name = ? WHERE id = ?",
                vec![name.into(), reader_id.into()],
            )
            .await?)
    }

    async fn inherit_book_readings(&self, reader_id: &str) -> Result<(), DomainError> {
        Ok(self
            .execute(
                "INSERT INTO book_readings \
                 (book_uuid, reader_id, reading_status, started_reading_at, \
                  finished_reading_at, user_rating, updated_at) \
                 SELECT uuid, ?, \
                        CASE WHEN reading_status = 'wanting' THEN '' ELSE reading_status END, \
                        started_reading_at, finished_reading_at, user_rating, ? \
                 FROM books \
                 WHERE reading_status NOT IN ('', 'wanting') \
                    OR started_reading_at IS NOT NULL \
                    OR finished_reading_at IS NOT NULL \
                    OR user_rating IS NOT NULL",
                vec![reader_id.into(), now().into()],
            )
            .await?)
    }

    async fn readings_of(
        &self,
        reader_id: &str,
        book_ids: Option<&[String]>,
    ) -> Result<HashMap<String, Reading>, DomainError> {
        let mut sql = "SELECT book_uuid, reading_status, started_reading_at, \
                       finished_reading_at, user_rating \
                       FROM book_readings WHERE reader_id = ?"
            .to_owned();
        let mut values: Vec<sea_orm::Value> = vec![reader_id.into()];
        if let Some(ids) = book_ids {
            if ids.is_empty() {
                return Ok(HashMap::new());
            }
            sql.push_str(&format!(
                " AND book_uuid IN ({})",
                vec!["?"; ids.len()].join(", ")
            ));
            values.extend(ids.iter().map(|id| id.clone().into()));
        }
        let rows = self
            .db
            .query_all(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                &sql,
                values,
            ))
            .await?;
        Ok(rows
            .iter()
            .map(reading_from_row)
            .collect::<Result<HashMap<_, _>, _>>()?)
    }

    async fn store_reading(
        &self,
        reader_id: &str,
        book_uuid: &str,
        reading: &Reading,
        exists: bool,
    ) -> Result<(), DomainError> {
        // A plain INSERT or UPDATE rather than an upsert: cr-sqlite captures
        // both through its triggers, and each statement stays obvious to it.
        let sql = if exists {
            "UPDATE book_readings SET reading_status = ?, started_reading_at = ?, \
             finished_reading_at = ?, user_rating = ?, updated_at = ? \
             WHERE book_uuid = ? AND reader_id = ?"
        } else {
            "INSERT INTO book_readings (reading_status, started_reading_at, \
             finished_reading_at, user_rating, updated_at, book_uuid, reader_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?)"
        };
        Ok(self
            .execute(
                sql,
                vec![
                    reading.reading_status.clone().into(),
                    reading.started_reading_at.clone().into(),
                    reading.finished_reading_at.clone().into(),
                    reading.user_rating.into(),
                    now().into(),
                    book_uuid.into(),
                    reader_id.into(),
                ],
            )
            .await?)
    }

    async fn count_read(&self, reader_id: &str, year: Option<&str>) -> Result<i64, DomainError> {
        let (condition, value) = match year {
            Some(year) => ("finished_reading_at LIKE ?", format!("{year}%")),
            None => ("reading_status = ?", "read".to_owned()),
        };
        let row = self
            .db
            .query_one(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                format!(
                    "SELECT count(*) AS n FROM book_readings \
                     WHERE reader_id = ? AND {condition} \
                       AND book_uuid IN (SELECT uuid FROM books)"
                ),
                [reader_id.into(), value.into()],
            ))
            .await?;
        Ok(row.map_or(Ok(0), |r| r.try_get("", "n"))?)
    }
}

/// The SQL condition on `books` matching `status` for `reader_id`, standing in
/// for `reading_status = ?` when the device has a reader. A reading status
/// matches the reader's own; `wanting` matches the household's wish, whatever
/// the reader's status.
///
/// Columns are qualified with `books.`: the callers join the authors, whose
/// table has a `uuid` column too.
pub fn status_condition(reader_id: &str, status: &str) -> sea_orm::sea_query::SimpleExpr {
    use sea_orm::sea_query::Expr;

    const NO_OWN_STATUS: &str = "books.uuid NOT IN \
         (SELECT book_uuid FROM book_readings \
          WHERE reader_id = ? AND reading_status != '')";

    // The wishlist is the household's: every reader finds the wished books
    // there, including the ones they hold a status of their own for.
    if status == crate::domain::WANTING {
        return Expr::cust_with_values("books.reading_status = ?", [status.to_owned()]);
    }
    if status.is_empty() {
        return Expr::cust_with_values(
            format!("(books.reading_status != 'wanting' AND {NO_OWN_STATUS})"),
            [reader_id.to_owned()],
        );
    }
    Expr::cust_with_values(
        "books.uuid IN \
         (SELECT book_uuid FROM book_readings \
          WHERE reader_id = ? AND reading_status = ?)",
        [reader_id.to_owned(), status.to_owned()],
    )
}

/// Remove every reader's state for a book. Part of `delete_book_cascade`.
pub async fn delete_readings_of_book<C: ConnectionTrait>(
    db: &C,
    book_uuid: &str,
) -> Result<(), DbErr> {
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "DELETE FROM book_readings WHERE book_uuid = ?",
        [book_uuid.into()],
    ))
    .await?;
    Ok(())
}
