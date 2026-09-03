//! Duplicate-book merge (ADR-070).
//!
//! Two devices that each added the same physical book before they shared an
//! account minted two different uuids for it. cr-sqlite merges by primary key,
//! so those rows stay distinct forever and the joined library shows the book
//! twice. This module is the merge that `account-sync-dedup-rule.md` decided on
//! 2026-06-25 and left to a later lot: the correlation key
//! ([`crate::utils::dedup_key::book_dedup_key`]) has been in the tree, tested,
//! and called by nothing ever since.
//!
//! What this module owns:
//! - grouping the library by natural identity, splitting what may merge on its
//!   own (a shared ISBN) from what may only be proposed (a shared title, author
//!   and year: within ONE library that is as likely to be two editions);
//! - the canonical survivor, `created_at` asc then `id` asc, the same total
//!   order `favorites_service` uses so two devices repairing in parallel elect
//!   the same row and converge;
//! - rewiring every reference, discovered from the live schema rather than
//!   listed, because two of them (`hangman_scores`, `metadata_fill_journal`)
//!   carry no declared foreign key and are invisible to the uuid migration's
//!   own drift guard;
//! - collapsing copies that nothing distinguishes, so a repaired library does
//!   not read "2 exemplaires" on every book it just fixed.
//!
//! Everything here is an ordinary row write, never an `ALTER` on a CRR, so it
//! is safe on an enrolled device. Deletions replicate: repairing one device
//! repairs the account.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    QuerySelect, Set, Statement, TransactionTrait, Value,
};

use crate::domain::DomainError;
use crate::models::{book, collection_book, copy, loan, sale};
use crate::utils::cover_url::{is_local_cover, local_cover_filename, own_local_cover_path};
use crate::utils::dedup_key::book_dedup_key;

/// Prefix `book_dedup_key` gives a group correlated by ISBN. Only these merge
/// without asking (ADR-070 D2).
const ISBN_KEY_PREFIX: &str = "isbn:";

/// Tables whose primary key IS the book reference: per-book device-local
/// sidecars. Their rows are DELETED with the duplicate, never carried over,
/// because they record what THIS device knows about a row that is ceasing to
/// exist (ADR-070 D7). Any other table of that shape aborts the merge: only a
/// human can say whether such a row should move or die.
const BOOK_KEYED_SIDECARS: &[&str] = &["book_local", "cover_sync_state"];

/// How a duplicate group was correlated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum MatchKind {
    /// Same ISBN, canonicalized to ISBN-13. Merges automatically.
    Isbn,
    /// Same normalized title, author and year. Proposed, never automatic.
    TitleAuthorYear,
}

/// One book inside a duplicate group, as the preview shows it.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DuplicateBook {
    pub id: String,
    pub title: String,
    pub isbn: Option<String>,
    pub author: Option<String>,
    pub created_at: String,
    pub cover_url: Option<String>,
}

/// A set of book rows that describe the same book.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DuplicateGroup {
    /// The natural-identity key, and the handle the caller passes back to merge
    /// one proposed group.
    pub key: String,
    pub kind: MatchKind,
    /// The row that survives (ADR-070 D3).
    pub canonical: DuplicateBook,
    /// The rows that are folded into it. Never empty.
    pub duplicates: Vec<DuplicateBook>,
}

/// What a repair would do, computed without writing anything.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct DuplicateScan {
    /// Groups correlated by ISBN: merged by `merge_automatic`.
    pub automatic: Vec<DuplicateGroup>,
    /// Groups correlated by title/author/year: each needs its own confirmation.
    pub proposed: Vec<DuplicateGroup>,
    /// Book rows `merge_automatic` would remove.
    pub books_removed_by_automatic: u32,
}

/// What a repair actually did.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct MergeReport {
    pub groups_merged: u32,
    pub books_removed: u32,
    pub copies_collapsed: u32,
    pub covers_recovered: u32,
}

fn db_err<E: std::fmt::Display>(e: E) -> DomainError {
    DomainError::Database(e.to_string())
}

// -----------------------------------------------------------------------------
// Public API
// -----------------------------------------------------------------------------

/// Group the library by natural identity and report what a repair would do.
/// Writes nothing.
pub async fn scan_duplicates(db: &DatabaseConnection) -> Result<DuplicateScan, DomainError> {
    // Fail here rather than half-way through a merge: a reference shape the
    // engine cannot classify must stop the whole feature, not one group.
    reference_plan(db).await?;

    let mut scan = DuplicateScan::default();
    for group in load_groups(db).await? {
        match group.kind {
            MatchKind::Isbn => {
                scan.books_removed_by_automatic += group.duplicates.len() as u32;
                scan.automatic.push(group);
            }
            MatchKind::TitleAuthorYear => scan.proposed.push(group),
        }
    }
    Ok(scan)
}

/// How many surplus book rows a repair would remove: every row of every
/// duplicate group but its survivor, the automatic and the proposed alike.
///
/// The account screen needs this number and nothing else, and asks for it on
/// every visit and after every sync cycle, so this skips the work
/// [`scan_duplicates`] does purely for display: no per-row payload, no ordering,
/// and four columns read instead of whole book rows.
///
/// It does NOT skip the [`reference_plan`] gate. A schema this engine cannot
/// classify must stop the feature before it is OFFERED, not merely before
/// anything is written: a banner promising a repair that errors on opening is
/// worse than no banner. Measured on a 492-book library of 48 tables, that walk
/// costs about a millisecond, so the guarantee is free.
pub async fn count_surplus(db: &DatabaseConnection) -> Result<u32, DomainError> {
    reference_plan(db).await?;

    let rows: Vec<(String, String, Option<String>, Option<i32>)> = book::Entity::find()
        .select_only()
        .column(book::Column::Id)
        .column(book::Column::Title)
        .column(book::Column::Isbn)
        .column(book::Column::PublicationYear)
        .into_tuple()
        .all(db)
        .await
        .map_err(db_err)?;
    let authors = primary_authors(db).await?;

    let mut sizes: HashMap<String, u32> = HashMap::new();
    for (id, title, isbn, publication_year) in rows {
        let key = book_dedup_key(
            isbn.as_deref(),
            &title,
            authors.get(&id).map(String::as_str),
            publication_year,
        );
        *sizes.entry(key).or_insert(0) += 1;
    }
    Ok(sizes.values().filter(|n| **n > 1).map(|n| n - 1).sum())
}

/// Merge every ISBN-correlated group. Idempotent: a library with no duplicate
/// yields an empty report.
pub async fn merge_automatic(
    db: &DatabaseConnection,
    covers_dir: Option<&Path>,
) -> Result<MergeReport, DomainError> {
    let plan = reference_plan(db).await?;
    let groups: Vec<DuplicateGroup> = load_groups(db)
        .await?
        .into_iter()
        .filter(|g| g.kind == MatchKind::Isbn)
        .collect();

    let mut report = MergeReport::default();
    for group in groups {
        merge_one(db, covers_dir, &plan, &group, &mut report).await?;
    }
    Ok(report)
}

/// Merge the single group carrying `key`. This is how a proposed
/// (title/author/year) group is accepted, one confirmation at a time.
pub async fn merge_group(
    db: &DatabaseConnection,
    covers_dir: Option<&Path>,
    key: &str,
) -> Result<MergeReport, DomainError> {
    let plan = reference_plan(db).await?;
    let group = load_groups(db)
        .await?
        .into_iter()
        .find(|g| g.key == key)
        .ok_or(DomainError::NotFound)?;

    let mut report = MergeReport::default();
    merge_one(db, covers_dir, &plan, &group, &mut report).await?;
    Ok(report)
}

// -----------------------------------------------------------------------------
// Grouping
// -----------------------------------------------------------------------------

/// The columns `load_groups` reads: id, title, isbn, publication year,
/// creation instant, cover. Everything the key and the preview need, and
/// nothing else.
type GroupingRow = (
    String,
    String,
    Option<String>,
    Option<i32>,
    String,
    Option<String>,
);

/// Every group of two or more rows sharing a natural-identity key, each with
/// its canonical row already elected. Ordered by title so the preview reads in
/// a stable order on every device.
async fn load_groups(db: &DatabaseConnection) -> Result<Vec<DuplicateGroup>, DomainError> {
    // Six columns, not whole rows. Grouping needs four of them and the preview
    // two more; `summary`, `marc_record`, `source_data` and `subjects` are the
    // heavy ones and none of them is read here. The full rows are re-read inside
    // the merge transaction, for the handful of books that actually merge.
    let rows: Vec<GroupingRow> = book::Entity::find()
        .select_only()
        .column(book::Column::Id)
        .column(book::Column::Title)
        .column(book::Column::Isbn)
        .column(book::Column::PublicationYear)
        .column(book::Column::CreatedAt)
        .column(book::Column::CoverUrl)
        .into_tuple()
        .all(db)
        .await
        .map_err(db_err)?;
    let authors = primary_authors(db).await?;

    let mut by_key: BTreeMap<String, Vec<DuplicateBook>> = BTreeMap::new();
    for (id, title, isbn, publication_year, created_at, cover_url) in rows {
        let key = book_dedup_key(
            isbn.as_deref(),
            &title,
            authors.get(&id).map(String::as_str),
            publication_year,
        );
        let author = authors.get(&id).cloned();
        by_key.entry(key).or_default().push(DuplicateBook {
            id,
            title,
            isbn,
            author,
            created_at,
            cover_url,
        });
    }

    let mut groups = Vec::new();
    for (key, mut view) in by_key {
        if view.len() < 2 {
            continue;
        }
        // ADR-070 D3: oldest first, ties broken by id. A total order over data
        // that has already converged, so every device elects the same survivor.
        view.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });

        let kind = if key.starts_with(ISBN_KEY_PREFIX) {
            MatchKind::Isbn
        } else {
            MatchKind::TitleAuthorYear
        };
        let canonical = view.remove(0);
        groups.push(DuplicateGroup {
            key,
            kind,
            canonical,
            duplicates: view,
        });
    }

    groups.sort_by(|a, b| {
        a.canonical
            .title
            .to_lowercase()
            .cmp(&b.canonical.title.to_lowercase())
            .then_with(|| a.key.cmp(&b.key))
    });
    Ok(groups)
}

/// One author name per book, the alphabetically first when a book has several.
///
/// The dedup key needs a single author and `book_authors` carries no ordering,
/// so insertion order would differ between two devices holding the same book.
/// Picking the minimum name is the only choice that agrees everywhere.
async fn primary_authors(db: &DatabaseConnection) -> Result<HashMap<String, String>, DomainError> {
    crate::services::author_names::primary_authors(db).await
}

// -----------------------------------------------------------------------------
// Reference discovery (ADR-070 D5)
// -----------------------------------------------------------------------------

/// How one table referencing a book must be treated when its book disappears.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RefKind {
    /// The book column is the whole primary key: a per-book sidecar. Delete.
    Sidecar,
    /// The book column is part of a composite key: a junction. Rewire, but drop
    /// the rows whose counterpart the survivor already has.
    Junction { other_key_columns: Vec<String> },
    /// An ordinary reference. Rewire.
    Plain,
}

#[derive(Clone, Debug)]
struct BookReference {
    table: String,
    column: String,
    kind: RefKind,
}

/// Every column in the live schema that names a book, with the treatment its
/// shape calls for.
///
/// Discovered rather than listed because a written list is already wrong:
/// `hangman_scores.book_id` and `metadata_fill_journal.book_id` reference books
/// with no declared foreign key, so even the uuid migration's drift guard does
/// not see them. `metadata_fill_run.cursor_book_id` is deliberately NOT matched:
/// it is a scan cursor, and a cursor pointing at a vanished row just restarts.
async fn reference_plan(db: &DatabaseConnection) -> Result<Vec<BookReference>, DomainError> {
    let mut plan = Vec::new();
    for table in candidate_tables(db).await? {
        let (columns, key_columns) = table_shape(db, &table).await?;
        for column in ["book_id", "book_uuid"] {
            if !columns.iter().any(|c| c == column) {
                continue;
            }
            let column = column.to_owned();
            let kind = if key_columns == [column.clone()] {
                if !BOOK_KEYED_SIDECARS.contains(&table.as_str()) {
                    // ADR-070 D5: a table keyed solely by a book is a per-book
                    // sidecar, and whether its row should follow the survivor or
                    // die with its book is a decision, not a default. Stop before
                    // anything is written.
                    return Err(DomainError::Internal(format!(
                        "duplicate merge aborted: `{table}` is keyed on `{column}` alone and \
                         the engine has no rule for it (add it to BOOK_KEYED_SIDECARS, or \
                         give it an explicit treatment)"
                    )));
                }
                RefKind::Sidecar
            } else if key_columns.contains(&column) {
                RefKind::Junction {
                    other_key_columns: key_columns
                        .iter()
                        .filter(|c| **c != column)
                        .cloned()
                        .collect(),
                }
            } else {
                RefKind::Plain
            };
            plan.push(BookReference {
                table: table.clone(),
                column,
                kind,
            });
        }
    }
    Ok(plan)
}

/// The ordinary tables worth inspecting: everything but SQLite's own, the
/// `books` table being merged, and cr-sqlite's machinery.
///
/// cr-sqlite is excluded by name rather than inspected. Its clock companions
/// (`book_authors__crsql_clock` and friends) do carry a `book_id`, but they are
/// the merge log itself and rewriting them by hand would corrupt the very
/// bookkeeping that replicates this repair; `crsql_changes` is a virtual table
/// that has no business in a schema walk at all. This is also why the shape is
/// read table by table instead of joining `sqlite_master` to
/// `pragma_table_info`: a join leaves it to the planner whether a virtual table
/// is expanded before the filter drops it.
async fn candidate_tables(db: &DatabaseConnection) -> Result<Vec<String>, DomainError> {
    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' \
               AND name <> 'books' \
               AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
               AND name NOT LIKE 'crsql%' \
               AND name NOT LIKE '%\\_\\_crsql\\_%' ESCAPE '\\' \
             ORDER BY name"
                .to_owned(),
        ))
        .await
        .map_err(db_err)?;

    let mut tables = Vec::new();
    for row in rows {
        let name: String = row.try_get("", "name").map_err(db_err)?;
        // The name comes from `sqlite_master`, but it is interpolated into SQL
        // further down (SQLite cannot bind an identifier), so it is checked
        // rather than trusted.
        if !is_safe_identifier(&name) {
            return Err(DomainError::Internal(format!(
                "duplicate merge: refusing unsafe table name {name}"
            )));
        }
        tables.push(name);
    }
    Ok(tables)
}

/// A table's column names, and its primary-key columns in declaration order.
async fn table_shape(
    db: &DatabaseConnection,
    table: &str,
) -> Result<(Vec<String>, Vec<String>), DomainError> {
    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            format!("PRAGMA table_info(\"{table}\")"),
        ))
        .await
        .map_err(db_err)?;

    let mut columns = Vec::new();
    let mut keyed: Vec<(i64, String)> = Vec::new();
    for row in rows {
        let name: String = row.try_get("", "name").map_err(db_err)?;
        let pk: i64 = row.try_get("", "pk").map_err(db_err)?;
        if pk > 0 {
            keyed.push((pk, name.clone()));
        }
        columns.push(name);
    }
    keyed.sort_by_key(|(position, _)| *position);
    Ok((columns, keyed.into_iter().map(|(_, name)| name).collect()))
}

fn is_safe_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// -----------------------------------------------------------------------------
// Merging one group
// -----------------------------------------------------------------------------

async fn merge_one(
    db: &DatabaseConnection,
    covers_dir: Option<&Path>,
    plan: &[BookReference],
    group: &DuplicateGroup,
    report: &mut MergeReport,
) -> Result<(), DomainError> {
    let canonical_id = group.canonical.id.clone();
    let duplicate_ids: Vec<String> = group.duplicates.iter().map(|d| d.id.clone()).collect();
    if duplicate_ids.is_empty() {
        return Ok(());
    }

    // Decided before the transaction, applied after the commit. A rename that
    // lands while the transaction then rolls back would move a file out from
    // under a row that still points at it; `migrate_uuid_pk` orders its own
    // cover renames the same way, and for the same reason.
    let cover_rescue = plan_cover_rescue(covers_dir, &canonical_id, &duplicate_ids);

    let txn = db.begin().await.map_err(db_err)?;

    // A collection membership carries the reading order of a series volume and
    // the moment the book was shelved, both on the junction row itself. The
    // generic rewiring below drops a duplicate's row whenever the survivor is
    // already a member of that collection, so whatever the survivor is missing
    // has to be taken before that happens.
    preserve_collection_membership(&txn, &canonical_id, &duplicate_ids).await?;

    for reference in plan {
        rewire_reference(&txn, reference, &canonical_id, &duplicate_ids).await?;
    }

    let duplicates = book::Entity::find()
        .filter(book::Column::Id.is_in(duplicate_ids.clone()))
        .all(&txn)
        .await
        .map_err(db_err)?;
    let canonical = book::Entity::find_by_id(&canonical_id)
        .one(&txn)
        .await
        .map_err(db_err)?
        .ok_or(DomainError::NotFound)?;
    reconcile_fields(&txn, canonical, &duplicates, cover_rescue.is_some()).await?;

    book::Entity::delete_many()
        .filter(book::Column::Id.is_in(duplicate_ids.clone()))
        .exec(&txn)
        .await
        .map_err(db_err)?;

    let collapsed = collapse_copies(&txn, &canonical_id).await?;

    txn.commit().await.map_err(db_err)?;

    // Committed: the survivor's row is the one that claims `<survivor>.jpg`, so
    // the file moves now.
    let mut kept_bytes: Option<std::path::PathBuf> = None;
    let mut covers_recovered = 0u32;
    if let Some((from, to)) = &cover_rescue {
        match std::fs::rename(from, to) {
            Ok(()) => covers_recovered = 1,
            Err(e) => {
                // The row already says the survivor has this cover, so it will
                // render a placeholder until the next cover edit. The bytes are
                // the part that cannot be recreated, so they are spared from the
                // orphan sweep below rather than swept away with their book.
                tracing::warn!(
                    "duplicate merge: cover rescue {} -> {} failed: {e}",
                    from.display(),
                    to.display()
                );
                kept_bytes = Some(from.clone());
            }
        }
    }

    // The merged-away rows are gone, so their cover files are orphans.
    if let Some(dir) = covers_dir {
        for id in &duplicate_ids {
            if let Some(path) = own_local_cover_path(dir, id)
                && kept_bytes.as_deref() != Some(path.as_path())
            {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    // A merge IS a deletion, and every other path that deletes a book records
    // one (`book_service::delete_book`). The operation log is where a reader
    // looks when rows vanish, and these vanish on every device of the account,
    // so staying silent here would make a replicated disappearance
    // unexplainable on the device that did not run the repair. Best effort,
    // exactly as everywhere else: a log write never fails a merge.
    for id in &duplicate_ids {
        let _ = crate::sync::log_operation(
            db,
            "book",
            id,
            "DELETE",
            Some(serde_json::json!({ "merged_into": canonical_id })),
        )
        .await;
    }

    report.groups_merged += 1;
    report.books_removed += duplicate_ids.len() as u32;
    report.copies_collapsed += collapsed;
    report.covers_recovered += covers_recovered;
    Ok(())
}

/// Carry a duplicate's collection metadata onto the survivor's membership row
/// before the duplicate's row is dropped as a composite-key collision.
async fn preserve_collection_membership<C: ConnectionTrait>(
    txn: &C,
    canonical_id: &str,
    duplicate_ids: &[String],
) -> Result<(), DomainError> {
    let kept = collection_book::Entity::find()
        .filter(collection_book::Column::BookId.eq(canonical_id))
        .all(txn)
        .await
        .map_err(db_err)?;
    if kept.is_empty() {
        return Ok(());
    }
    let incoming = collection_book::Entity::find()
        .filter(collection_book::Column::BookId.is_in(duplicate_ids.to_vec()))
        .all(txn)
        .await
        .map_err(db_err)?;

    for row in kept {
        let twins: Vec<&collection_book::Model> = incoming
            .iter()
            .filter(|i| i.collection_id == row.collection_id)
            .collect();
        if twins.is_empty() {
            continue;
        }
        let volume = row
            .volume_number
            .or_else(|| twins.iter().find_map(|t| t.volume_number));
        // The book entered the collection the first time any of its rows did.
        let added_at = twins
            .iter()
            .map(|t| t.added_at.as_str())
            .chain(std::iter::once(row.added_at.as_str()))
            .min()
            .unwrap_or(row.added_at.as_str())
            .to_owned();
        if volume == row.volume_number && added_at == row.added_at {
            continue;
        }
        let mut active: collection_book::ActiveModel = row.into();
        active.volume_number = Set(volume);
        active.added_at = Set(added_at);
        active.update(txn).await.map_err(db_err)?;
    }
    Ok(())
}

/// Point one table's book references at the survivor.
async fn rewire_reference<C: ConnectionTrait>(
    txn: &C,
    reference: &BookReference,
    canonical_id: &str,
    duplicate_ids: &[String],
) -> Result<(), DomainError> {
    let table = &reference.table;
    let column = &reference.column;
    let list = placeholders(duplicate_ids.len());
    let ids: Vec<Value> = duplicate_ids
        .iter()
        .map(|id| Value::from(id.clone()))
        .collect();

    match &reference.kind {
        RefKind::Sidecar => {
            exec(
                txn,
                format!("DELETE FROM \"{table}\" WHERE \"{column}\" IN ({list})"),
                ids,
            )
            .await
        }
        RefKind::Junction { other_key_columns } => {
            // Drop first, move second. A junction row whose counterpart the
            // survivor already holds would collide on the composite key, and
            // `OR IGNORE` on a cr-sqlite CRR is not a road worth taking.
            let matches_counterpart = other_key_columns
                .iter()
                .map(|c| format!("kept.\"{c}\" = \"{table}\".\"{c}\""))
                .collect::<Vec<_>>()
                .join(" AND ");
            // Parameter order follows the SQL text: the IN list, then the
            // survivor's id inside the EXISTS.
            let mut delete_values = ids.clone();
            delete_values.push(Value::from(canonical_id.to_owned()));
            exec(
                txn,
                format!(
                    "DELETE FROM \"{table}\" WHERE \"{column}\" IN ({list}) \
                     AND EXISTS (SELECT 1 FROM \"{table}\" AS kept \
                                 WHERE kept.\"{column}\" = ? AND {matches_counterpart})"
                ),
                delete_values,
            )
            .await?;
            let mut update_values = vec![Value::from(canonical_id.to_owned())];
            update_values.extend(ids);
            exec(
                txn,
                format!("UPDATE \"{table}\" SET \"{column}\" = ? WHERE \"{column}\" IN ({list})"),
                update_values,
            )
            .await
        }
        RefKind::Plain => {
            let mut values = vec![Value::from(canonical_id.to_owned())];
            values.extend(ids);
            exec(
                txn,
                format!("UPDATE \"{table}\" SET \"{column}\" = ? WHERE \"{column}\" IN ({list})"),
                values,
            )
            .await
        }
    }
}

async fn exec<C: ConnectionTrait>(
    txn: &C,
    sql: String,
    values: Vec<Value>,
) -> Result<(), DomainError> {
    txn.execute(Statement::from_sql_and_values(
        txn.get_database_backend(),
        sql,
        values,
    ))
    .await
    .map(|_| ())
    .map_err(db_err)
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

// -----------------------------------------------------------------------------
// Field reconciliation (ADR-070 D4)
// -----------------------------------------------------------------------------

/// The survivor keeps every value it holds and inherits only what it lacks.
/// `owned` and `private` widen instead: possession recorded anywhere is
/// possession, privacy claimed anywhere is privacy.
async fn reconcile_fields<C: ConnectionTrait>(
    txn: &C,
    canonical: book::Model,
    duplicates: &[book::Model],
    cover_rescued: bool,
) -> Result<(), DomainError> {
    let mut changed = false;
    let mut active: book::ActiveModel = canonical.clone().into();

    macro_rules! fill_option {
        ($field:ident) => {
            if canonical.$field.is_none()
                && let Some(value) = duplicates.iter().find_map(|d| d.$field.clone())
            {
                active.$field = Set(Some(value));
                changed = true;
            }
        };
    }

    fill_option!(isbn);
    fill_option!(summary);
    fill_option!(publisher);
    fill_option!(publication_year);
    fill_option!(dewey_decimal);
    fill_option!(lcc);
    fill_option!(marc_record);
    fill_option!(cataloguing_notes);
    fill_option!(source_data);
    fill_option!(shelf_position);
    fill_option!(finished_reading_at);
    fill_option!(started_reading_at);
    fill_option!(user_rating);
    fill_option!(price);
    fill_option!(page_count);
    fill_option!(loan_duration_days);

    if canonical.title.trim().is_empty()
        && let Some(title) = duplicates
            .iter()
            .map(|d| d.title.clone())
            .find(|t| !t.trim().is_empty())
    {
        active.title = Set(title);
        changed = true;
    }

    if canonical.reading_status.trim().is_empty()
        && let Some(status) = duplicates
            .iter()
            .map(|d| d.reading_status.clone())
            .find(|s| !s.trim().is_empty())
    {
        active.reading_status = Set(status);
        changed = true;
    }

    // A shelf lives in `books.subjects`, so taking the survivor's list whole
    // would silently unshelve the book on one of the reader's own devices.
    if let Some(subjects) = union_json_array(
        canonical.subjects.as_deref(),
        duplicates.iter().filter_map(|d| d.subjects.as_deref()),
    ) {
        active.subjects = Set(Some(subjects));
        changed = true;
    }
    if let Some(formats) = union_json_array(
        canonical.digital_formats.as_deref(),
        duplicates
            .iter()
            .filter_map(|d| d.digital_formats.as_deref()),
    ) {
        active.digital_formats = Set(Some(formats));
        changed = true;
    }

    if !canonical.owned && duplicates.iter().any(|d| d.owned) {
        active.owned = Set(true);
        changed = true;
    }
    if !canonical.private && duplicates.iter().any(|d| d.private) {
        active.private = Set(true);
        changed = true;
    }

    // The rescued file will be named after the survivor, so the column has to
    // say so: the stored form is the bare basename (ADR-044 Addendum A.4).
    //
    // Without a rescue, only a DEVICE-INDEPENDENT value may be inherited. A
    // local one is `<that duplicate's id>.jpg`, a file keyed to the row about to
    // disappear: carried over it would name a cover the survivor can never
    // resolve, on this device and on every device the column replicates to.
    if cover_rescued {
        active.cover_url = Set(Some(local_cover_filename(&canonical.id)));
        changed = true;
    } else if canonical.cover_url.is_none()
        && let Some(url) = duplicates
            .iter()
            .filter_map(|d| d.cover_url.as_deref())
            .find(|url| !url.is_empty() && !is_local_cover(url))
    {
        active.cover_url = Set(Some(url.to_owned()));
        changed = true;
    }

    if changed {
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active.update(txn).await.map_err(db_err)?;
    }
    Ok(())
}

/// The union of several JSON string arrays, or `None` when it adds nothing to
/// `current`. Order is preserved: the survivor's entries first, then whatever
/// the duplicates bring, so a reader's own ordering survives.
fn union_json_array<'a>(
    current: Option<&str>,
    others: impl Iterator<Item = &'a str>,
) -> Option<String> {
    let parse = |raw: &str| serde_json::from_str::<Vec<String>>(raw).unwrap_or_default();
    let mut merged: Vec<String> = current.map(parse).unwrap_or_default();
    let mut seen: HashSet<String> = merged.iter().cloned().collect();

    let mut added = false;
    for raw in others {
        for entry in parse(raw) {
            if seen.insert(entry.clone()) {
                merged.push(entry);
                added = true;
            }
        }
    }
    added.then(|| serde_json::to_string(&merged).unwrap_or_default())
}

// -----------------------------------------------------------------------------
// Covers (ADR-070 D8)
// -----------------------------------------------------------------------------

/// Which cover file should be re-keyed onto the survivor, `(from, to)`, when the
/// survivor has none of its own and a duplicate does.
///
/// Decides only. The caller applies it AFTER the transaction commits, so a
/// rolled-back merge never leaves a file moved out from under the row that still
/// points at it.
fn plan_cover_rescue(
    covers_dir: Option<&Path>,
    canonical_id: &str,
    duplicate_ids: &[String],
) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let dir = covers_dir?;
    let target = own_local_cover_path(dir, canonical_id)?;
    if target.exists() {
        return None;
    }
    duplicate_ids
        .iter()
        .filter_map(|id| own_local_cover_path(dir, id))
        .find(|source| source.exists())
        .map(|source| (source, target))
}

// -----------------------------------------------------------------------------
// Copies (ADR-070 D6)
// -----------------------------------------------------------------------------

/// Fold copies of `book_id` that nothing distinguishes into the oldest of each
/// set. A copy carrying a loan or a sale is never touched, and neither is one
/// whose status or acquisition data sets it apart: it may be a real second
/// exemplar, and the engine cannot tell.
async fn collapse_copies<C: ConnectionTrait>(txn: &C, book_id: &str) -> Result<u32, DomainError> {
    let copies = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .all(txn)
        .await
        .map_err(db_err)?;
    if copies.len() < 2 {
        return Ok(0);
    }

    let copy_ids: Vec<String> = copies.iter().map(|c| c.id.clone()).collect();
    let mut attached: HashSet<String> = loan::Entity::find()
        .filter(loan::Column::CopyId.is_in(copy_ids.clone()))
        .all(txn)
        .await
        .map_err(db_err)?
        .into_iter()
        .map(|l| l.copy_id)
        .collect();
    attached.extend(
        sale::Entity::find()
            .filter(sale::Column::CopyId.is_in(copy_ids))
            .all(txn)
            .await
            .map_err(db_err)?
            .into_iter()
            .map(|s| s.copy_id),
    );

    let mut by_signature: BTreeMap<String, Vec<copy::Model>> = BTreeMap::new();
    for c in copies {
        if attached.contains(&c.id) {
            continue;
        }
        by_signature.entry(copy_signature(&c)).or_default().push(c);
    }

    let mut doomed: Vec<String> = Vec::new();
    for (_, mut set) in by_signature {
        if set.len() < 2 {
            continue;
        }
        set.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        doomed.extend(set.into_iter().skip(1).map(|c| c.id));
    }
    if doomed.is_empty() {
        return Ok(0);
    }

    let removed = doomed.len() as u32;
    copy::Entity::delete_many()
        .filter(copy::Column::Id.is_in(doomed))
        .exec(txn)
        .await
        .map_err(db_err)?;
    Ok(removed)
}

/// Everything that could tell two exemplars of the same book apart. Deliberately
/// exhaustive: a field left out here is a copy silently deleted.
fn copy_signature(c: &copy::Model) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        c.status,
        c.library_id,
        c.acquisition_date.as_deref().unwrap_or(""),
        c.notes.as_deref().unwrap_or(""),
        c.price.map(f64::to_bits).unwrap_or(0),
        c.is_temporary,
        c.sold_at.as_deref().unwrap_or(""),
        c.lender_display_name.as_deref().unwrap_or(""),
        c.lender_peer_id.unwrap_or(0),
        c.borrow_due_date.as_deref().unwrap_or(""),
        c.borrow_source.as_deref().unwrap_or(""),
        c.lender_library_uuid.as_deref().unwrap_or(""),
        c.lender_request_id.as_deref().unwrap_or(""),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{author, book_authors, collection, tag};
    use sea_orm::{ActiveModelTrait, DatabaseBackend};

    async fn setup_db() -> DatabaseConnection {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        // Copies are seeded with `library_id = 0`, as `book_service::tests` and
        // `collection_service::tests` already do.
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA foreign_keys = OFF".to_owned(),
        ))
        .await
        .unwrap();
        db
    }

    /// A book with an explicit uuid and creation instant, so the tests control
    /// which row is the oldest instead of racing the clock.
    async fn insert_book(
        db: &DatabaseConnection,
        id: &str,
        title: &str,
        isbn: Option<&str>,
        created_at: &str,
    ) {
        book::ActiveModel {
            id: Set(id.to_owned()),
            title: Set(title.to_owned()),
            isbn: Set(isbn.map(str::to_owned)),
            created_at: Set(created_at.to_owned()),
            updated_at: Set(created_at.to_owned()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    /// A book with a publication year and no ISBN, so it keys on the
    /// title/author/year branch of `book_dedup_key`.
    async fn insert_dated_book(
        db: &DatabaseConnection,
        id: &str,
        title: &str,
        publication_year: Option<i32>,
        created_at: &str,
    ) {
        book::ActiveModel {
            id: Set(id.to_owned()),
            title: Set(title.to_owned()),
            publication_year: Set(publication_year),
            created_at: Set(created_at.to_owned()),
            updated_at: Set(created_at.to_owned()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn insert_author(db: &DatabaseConnection, id: &str, name: &str) {
        let now = "2026-01-01T00:00:00Z".to_owned();
        author::ActiveModel {
            id: Set(id.to_owned()),
            name: Set(name.to_owned()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn link_author(db: &DatabaseConnection, book_id: &str, author_id: &str) {
        book_authors::ActiveModel {
            book_id: Set(book_id.to_owned()),
            author_id: Set(author_id.to_owned()),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn insert_collection(db: &DatabaseConnection, id: &str, name: &str) {
        let now = "2026-01-01T00:00:00Z".to_owned();
        collection::ActiveModel {
            id: Set(id.to_owned()),
            name: Set(name.to_owned()),
            description: Set(None),
            source: Set("manual".to_owned()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn attach_to_collection(
        db: &DatabaseConnection,
        collection_id: &str,
        book_id: &str,
        added_at: &str,
        volume_number: Option<i32>,
    ) {
        collection_book::ActiveModel {
            collection_id: Set(collection_id.to_owned()),
            book_id: Set(book_id.to_owned()),
            added_at: Set(added_at.to_owned()),
            volume_number: Set(volume_number),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn insert_copy(db: &DatabaseConnection, id: &str, book_id: &str, status: &str) {
        let now = "2026-01-01T00:00:00Z".to_owned();
        copy::ActiveModel {
            id: Set(id.to_owned()),
            book_id: Set(book_id.to_owned()),
            library_id: Set(0),
            status: Set(status.to_owned()),
            is_temporary: Set(false),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn book_ids(db: &DatabaseConnection) -> Vec<String> {
        let mut ids: Vec<String> = book::Entity::find()
            .all(db)
            .await
            .unwrap()
            .into_iter()
            .map(|b| b.id)
            .collect();
        ids.sort();
        ids
    }

    async fn exec_sql(db: &DatabaseConnection, sql: &str) {
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            sql.to_owned(),
        ))
        .await
        .unwrap();
    }

    // -- grouping ------------------------------------------------------------

    #[tokio::test]
    async fn isbn_groups_merge_alone_and_title_groups_are_only_proposed() {
        let db = setup_db().await;
        insert_book(
            &db,
            "b1",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "b2",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        insert_book(&db, "n1", "Sans code", None, "2026-01-01T00:00:00Z").await;
        insert_book(&db, "n2", "Sans code", None, "2026-02-01T00:00:00Z").await;

        let scan = scan_duplicates(&db).await.unwrap();
        assert_eq!(scan.automatic.len(), 1);
        assert_eq!(scan.automatic[0].kind, MatchKind::Isbn);
        assert_eq!(scan.books_removed_by_automatic, 1);
        assert_eq!(scan.proposed.len(), 1);
        assert_eq!(scan.proposed[0].kind, MatchKind::TitleAuthorYear);

        merge_automatic(&db, None).await.unwrap();
        // The ISBN pair collapsed; the title-only pair is untouched.
        assert_eq!(book_ids(&db).await, vec!["b1", "n1", "n2"]);
    }

    #[tokio::test]
    async fn isbn10_and_isbn13_of_the_same_book_are_one_group() {
        let db = setup_db().await;
        insert_book(
            &db,
            "b1",
            "Dune",
            Some("2264024844"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "b2",
            "Dune",
            Some("978-2-264-02484-8"),
            "2026-02-01T00:00:00Z",
        )
        .await;

        let scan = scan_duplicates(&db).await.unwrap();
        assert_eq!(scan.automatic.len(), 1);
        assert_eq!(scan.automatic[0].canonical.id, "b1");
    }

    #[tokio::test]
    async fn a_proposed_group_merges_only_when_its_key_is_confirmed() {
        let db = setup_db().await;
        insert_book(&db, "n1", "Sans code", None, "2026-01-01T00:00:00Z").await;
        insert_book(&db, "n2", "Sans code", None, "2026-02-01T00:00:00Z").await;

        let scan = scan_duplicates(&db).await.unwrap();
        let key = scan.proposed[0].key.clone();
        let report = merge_group(&db, None, &key).await.unwrap();

        assert_eq!(report.groups_merged, 1);
        assert_eq!(book_ids(&db).await, vec!["n1"]);
        assert!(matches!(
            merge_group(&db, None, "isbn:nope").await,
            Err(DomainError::NotFound)
        ));
    }

    // -- survivor and fields -------------------------------------------------

    #[tokio::test]
    async fn the_oldest_survives_and_inherits_only_what_it_lacks() {
        let db = setup_db().await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-05-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;

        // The survivor already has a publisher; the duplicate carries a summary
        // and a rating it does not have.
        let mut kept: book::ActiveModel = book::Entity::find_by_id("old")
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .into();
        kept.publisher = Set(Some("Kept".to_owned()));
        kept.update(&db).await.unwrap();

        let mut other: book::ActiveModel = book::Entity::find_by_id("young")
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .into();
        other.publisher = Set(Some("Discarded".to_owned()));
        other.summary = Set(Some("Inherited".to_owned()));
        other.user_rating = Set(Some(8));
        other.update(&db).await.unwrap();

        merge_automatic(&db, None).await.unwrap();

        let survivor = book::Entity::find_by_id("old")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(survivor.publisher.as_deref(), Some("Kept"));
        assert_eq!(survivor.summary.as_deref(), Some("Inherited"));
        assert_eq!(survivor.user_rating, Some(8));
        assert_eq!(book_ids(&db).await, vec!["old"]);
    }

    #[tokio::test]
    async fn owned_and_private_widen_instead_of_being_overwritten() {
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;

        let mut other: book::ActiveModel = book::Entity::find_by_id("young")
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .into();
        other.owned = Set(true);
        other.private = Set(true);
        other.update(&db).await.unwrap();

        merge_automatic(&db, None).await.unwrap();

        let survivor = book::Entity::find_by_id("old")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(survivor.owned, "possession recorded anywhere must survive");
        assert!(survivor.private, "privacy claimed anywhere must survive");
    }

    #[tokio::test]
    async fn shelves_are_unioned_so_a_merge_never_unshelves_a_book() {
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;

        for (id, subjects) in [("old", r#"["Salon"]"#), ("young", r#"["Bureau","Salon"]"#)] {
            let mut active: book::ActiveModel = book::Entity::find_by_id(id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .into();
            active.subjects = Set(Some(subjects.to_owned()));
            active.update(&db).await.unwrap();
        }

        merge_automatic(&db, None).await.unwrap();

        let survivor = book::Entity::find_by_id("old")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let shelves: Vec<String> =
            serde_json::from_str(survivor.subjects.as_deref().unwrap()).unwrap();
        assert_eq!(shelves, vec!["Salon".to_owned(), "Bureau".to_owned()]);
    }

    // -- references ----------------------------------------------------------

    #[tokio::test]
    async fn a_shared_collection_holds_the_book_once_after_the_merge() {
        // The reported symptom: one collection, the same book listed twice.
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        insert_collection(&db, "c1", "Perso").await;
        attach_to_collection(&db, "c1", "old", "2026-03-01T00:00:00Z", None).await;
        attach_to_collection(&db, "c1", "young", "2026-02-15T00:00:00Z", Some(3)).await;

        merge_automatic(&db, None).await.unwrap();

        let members = collection_book::Entity::find()
            .filter(collection_book::Column::CollectionId.eq("c1"))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].book_id, "old");
        // The volume number and the earlier shelving date were carried over
        // rather than dropped with the duplicate's row.
        assert_eq!(members[0].volume_number, Some(3));
        assert_eq!(members[0].added_at, "2026-02-15T00:00:00Z");
    }

    #[tokio::test]
    async fn a_membership_the_survivor_did_not_have_is_moved_over() {
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        insert_collection(&db, "c1", "Perso").await;
        attach_to_collection(&db, "c1", "young", "2026-02-15T00:00:00Z", None).await;

        merge_automatic(&db, None).await.unwrap();

        let members = collection_book::Entity::find().all(&db).await.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].book_id, "old");
    }

    #[tokio::test]
    async fn shelf_tags_survive_the_merge_without_colliding() {
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        for (id, name) in [("t1", "science-fiction"), ("t2", "à relire")] {
            tag::ActiveModel {
                id: Set(id.to_owned()),
                name: Set(name.to_owned()),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
        }
        // Both rows carry `t1`; only the duplicate carries `t2`.
        exec_sql(
            &db,
            "INSERT INTO book_tags (book_id, tag_id) VALUES ('old','t1')",
        )
        .await;
        exec_sql(
            &db,
            "INSERT INTO book_tags (book_id, tag_id) VALUES ('young','t1')",
        )
        .await;
        exec_sql(
            &db,
            "INSERT INTO book_tags (book_id, tag_id) VALUES ('young','t2')",
        )
        .await;

        merge_automatic(&db, None).await.unwrap();

        let rows = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT book_id, tag_id FROM book_tags ORDER BY tag_id".to_owned(),
            ))
            .await
            .unwrap();
        let pairs: Vec<(String, String)> = rows
            .iter()
            .map(|r| {
                (
                    r.try_get("", "book_id").unwrap(),
                    r.try_get("", "tag_id").unwrap(),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("old".to_owned(), "t1".to_owned()),
                ("old".to_owned(), "t2".to_owned())
            ]
        );
    }

    #[tokio::test]
    async fn a_reference_with_no_declared_foreign_key_is_still_rewired() {
        // `hangman_scores` and `metadata_fill_journal` are shaped like this, and
        // the uuid migration's drift guard cannot see either.
        let db = setup_db().await;
        exec_sql(
            &db,
            "CREATE TABLE zzz_fkless (id INTEGER PRIMARY KEY, book_id TEXT NOT NULL)",
        )
        .await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        exec_sql(&db, "INSERT INTO zzz_fkless (book_id) VALUES ('young')").await;

        merge_automatic(&db, None).await.unwrap();

        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT book_id FROM zzz_fkless".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<String>("", "book_id").unwrap(), "old");
    }

    #[tokio::test]
    async fn a_device_local_sidecar_row_is_deleted_rather_than_moved() {
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        exec_sql(
            &db,
            "INSERT INTO cover_sync_state (book_uuid, file_mtime) VALUES ('young', 42)",
        )
        .await;

        merge_automatic(&db, None).await.unwrap();

        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS n FROM cover_sync_state".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "n").unwrap(), 0);
    }

    #[tokio::test]
    async fn an_unknown_book_keyed_table_aborts_before_anything_is_written() {
        let db = setup_db().await;
        exec_sql(
            &db,
            "CREATE TABLE zzz_sidecar (book_uuid TEXT PRIMARY KEY, note TEXT)",
        )
        .await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;

        assert!(matches!(
            scan_duplicates(&db).await,
            Err(DomainError::Internal(_))
        ));
        assert!(matches!(
            merge_automatic(&db, None).await,
            Err(DomainError::Internal(_))
        ));
        assert_eq!(book_ids(&db).await, vec!["old", "young"]);
    }

    // -- copies --------------------------------------------------------------

    #[tokio::test]
    async fn indiscernible_copies_collapse_into_one() {
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        insert_copy(&db, "c-old", "old", "available").await;
        insert_copy(&db, "c-young", "young", "available").await;

        let report = merge_automatic(&db, None).await.unwrap();

        assert_eq!(report.copies_collapsed, 1);
        let copies = copy::Entity::find().all(&db).await.unwrap();
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].id, "c-old");
        assert_eq!(copies[0].book_id, "old");
    }

    #[tokio::test]
    async fn a_copy_that_differs_is_kept_as_a_second_exemplar() {
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        insert_copy(&db, "c-old", "old", "available").await;
        insert_copy(&db, "c-young", "young", "loaned").await;

        let report = merge_automatic(&db, None).await.unwrap();

        assert_eq!(report.copies_collapsed, 0);
        assert_eq!(copy::Entity::find().all(&db).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_copy_carrying_a_loan_is_never_collapsed() {
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        insert_copy(&db, "c-old", "old", "available").await;
        insert_copy(&db, "c-young", "young", "available").await;
        exec_sql(
            &db,
            "INSERT INTO loans (uuid, copy_id, contact_id, library_id, loan_date, due_date, \
             status, created_at, updated_at) VALUES ('l1','c-young','k1',0,'2026-03-01', \
             '2026-04-01','active','2026-03-01','2026-03-01')",
        )
        .await;

        let report = merge_automatic(&db, None).await.unwrap();

        assert_eq!(report.copies_collapsed, 0);
        assert_eq!(copy::Entity::find().all(&db).await.unwrap().len(), 2);
    }

    // -- covers and idempotence ---------------------------------------------

    #[tokio::test]
    async fn the_survivor_inherits_the_only_cover_file_on_the_device() {
        let db = setup_db().await;
        let dir = std::env::temp_dir().join(format!(
            "bg-merge-{}",
            crate::utils::uuid_gen::new_uuid_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        std::fs::write(dir.join("young.jpg"), b"bytes").unwrap();

        let report = merge_automatic(&db, Some(dir.as_path())).await.unwrap();

        assert_eq!(report.covers_recovered, 1);
        assert!(dir.join("old.jpg").exists());
        assert!(!dir.join("young.jpg").exists());
        let survivor = book::Entity::find_by_id("old")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(survivor.cover_url.as_deref(), Some("old.jpg"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The rescue DECIDES, it does not act: the rename belongs after the commit,
    /// so a transaction that rolls back can never have moved a file out from
    /// under the row that still points at it. Directly on the planner, because
    /// no test can make the merge transaction fail on demand.
    #[test]
    fn planning_a_cover_rescue_moves_nothing() {
        let dir = std::env::temp_dir().join(format!(
            "bg-merge-plan-{}",
            crate::utils::uuid_gen::new_uuid_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("young.jpg"), b"bytes").unwrap();

        let plan = plan_cover_rescue(Some(dir.as_path()), "old", &["young".to_owned()]);

        assert_eq!(
            plan,
            Some((dir.join("young.jpg"), dir.join("old.jpg"))),
            "the planner must name the rename it wants"
        );
        assert!(
            dir.join("young.jpg").exists(),
            "planning must not move the file"
        );
        assert!(!dir.join("old.jpg").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A duplicate's LOCAL cover value names `<that book's id>.jpg`, a file keyed
    /// to the row being deleted. Inheriting it would plant a reference the
    /// survivor can never resolve, on every device the column reaches.
    #[tokio::test]
    async fn a_local_cover_value_is_never_inherited_from_a_duplicate() {
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        let mut other: book::ActiveModel = book::Entity::find_by_id("young")
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .into();
        other.cover_url = Set(Some("young.jpg".to_owned()));
        other.update(&db).await.unwrap();

        // No covers directory, so no file can be rescued either.
        merge_automatic(&db, None).await.unwrap();

        let survivor = book::Entity::find_by_id("old")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(survivor.cover_url, None);
    }

    /// A remote cover, on the other hand, describes the same image wherever it
    /// is read, so the survivor is right to take it.
    #[tokio::test]
    async fn a_remote_cover_url_is_inherited() {
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        let mut other: book::ActiveModel = book::Entity::find_by_id("young")
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .into();
        other.cover_url = Set(Some("https://covers.example/dune.jpg".to_owned()));
        other.update(&db).await.unwrap();

        merge_automatic(&db, None).await.unwrap();

        let survivor = book::Entity::find_by_id("old")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            survivor.cover_url.as_deref(),
            Some("https://covers.example/dune.jpg")
        );
    }

    /// A merge refused by the drift guard must not have touched anything, files
    /// included.
    #[tokio::test]
    async fn a_refused_merge_leaves_the_cover_files_untouched() {
        let db = setup_db().await;
        let dir = std::env::temp_dir().join(format!(
            "bg-merge-abort-{}",
            crate::utils::uuid_gen::new_uuid_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;
        std::fs::write(dir.join("young.jpg"), b"bytes").unwrap();
        // A book-keyed table the engine has no rule for: the merge aborts in the
        // reference plan, before any write and before any rename.
        exec_sql(
            &db,
            "CREATE TABLE zzz_sidecar (book_uuid TEXT PRIMARY KEY, note TEXT)",
        )
        .await;

        assert!(merge_automatic(&db, Some(dir.as_path())).await.is_err());

        assert!(
            dir.join("young.jpg").exists(),
            "the file must not have moved"
        );
        assert!(!dir.join("old.jpg").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn merging_twice_changes_nothing_the_second_time() {
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "young",
            "Dune",
            Some("9782264024848"),
            "2026-02-01T00:00:00Z",
        )
        .await;

        assert_eq!(merge_automatic(&db, None).await.unwrap().groups_merged, 1);
        let second = merge_automatic(&db, None).await.unwrap();
        assert_eq!(second, MergeReport::default());
        assert_eq!(book_ids(&db).await, vec!["old"]);
    }

    #[tokio::test]
    async fn a_library_without_duplicates_yields_an_empty_scan() {
        let db = setup_db().await;
        insert_book(
            &db,
            "b1",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "b2",
            "Neuromancien",
            Some("9782253072980"),
            "2026-01-01T00:00:00Z",
        )
        .await;

        let scan = scan_duplicates(&db).await.unwrap();
        assert_eq!(scan, DuplicateScan::default());
    }

    /// The count the account banner reads and the full preview must never
    /// disagree. The fixture carries every shape where they COULD: an ISBN-10
    /// beside its ISBN-13 (canonicalization), a book whose several authors are
    /// attached in an order that contradicts the alphabetical rule, and two
    /// near-misses that must stay apart (another author, a missing year).
    #[tokio::test]
    async fn the_count_matches_the_preview() {
        let db = setup_db().await;

        // One ISBN group of three: plain, formatted, and the ISBN-10 form.
        insert_book(
            &db,
            "a1",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "a2",
            "Dune",
            Some("978-2-264-02484-8"),
            "2026-01-02T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "a3",
            "Dune",
            Some("2264024844"),
            "2026-01-03T00:00:00Z",
        )
        .await;

        insert_author(&db, "au-gibson", "Gibson").await;
        insert_author(&db, "au-zelazny", "Zelazny").await;
        insert_author(&db, "au-sterling", "Sterling").await;

        // One proposed group of two. `b1` gets Zelazny FIRST, so insertion order
        // contradicts the alphabetical rule and only the rule can match it to `b2`.
        insert_dated_book(
            &db,
            "b1",
            "Neuromancien",
            Some(1984),
            "2026-01-01T00:00:00Z",
        )
        .await;
        link_author(&db, "b1", "au-zelazny").await;
        link_author(&db, "b1", "au-gibson").await;
        insert_dated_book(
            &db,
            "b2",
            "Neuromancien",
            Some(1984),
            "2026-01-02T00:00:00Z",
        )
        .await;
        link_author(&db, "b2", "au-gibson").await;

        // Near-misses: same title and year but another author, and the same
        // title and author with no year. Neither may join anything.
        insert_dated_book(
            &db,
            "d1",
            "Neuromancien",
            Some(1984),
            "2026-01-04T00:00:00Z",
        )
        .await;
        link_author(&db, "d1", "au-sterling").await;
        insert_dated_book(&db, "d2", "Neuromancien", None, "2026-01-05T00:00:00Z").await;
        link_author(&db, "d2", "au-gibson").await;
        insert_dated_book(&db, "d3", "Fondation", Some(1951), "2026-01-06T00:00:00Z").await;

        let scan = scan_duplicates(&db).await.unwrap();
        let from_preview: u32 = scan.books_removed_by_automatic
            + scan
                .proposed
                .iter()
                .map(|g| g.duplicates.len() as u32)
                .sum::<u32>();

        // Two surplus rows in the ISBN group, one in the proposed group.
        assert_eq!(scan.books_removed_by_automatic, 2);
        assert_eq!(scan.proposed.len(), 1);
        assert_eq!(from_preview, 3);
        assert_eq!(count_surplus(&db).await.unwrap(), from_preview);
    }

    #[tokio::test]
    async fn the_count_is_zero_on_a_library_without_duplicates() {
        let db = setup_db().await;
        insert_book(
            &db,
            "b1",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "b2",
            "Neuromancien",
            Some("9782253072980"),
            "2026-01-01T00:00:00Z",
        )
        .await;

        assert_eq!(count_surplus(&db).await.unwrap(), 0);
    }

    /// A merged-away book disappears from every device of the account, so the
    /// device that did not run the repair needs the same trace a manual
    /// deletion leaves, plus the survivor it was folded into.
    #[tokio::test]
    async fn every_removed_book_leaves_a_deletion_in_the_operation_log() {
        let db = setup_db().await;
        insert_book(
            &db,
            "old",
            "Dune",
            Some("9782264024848"),
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "dup",
            "Dune",
            Some("9782264024848"),
            "2026-01-02T00:00:00Z",
        )
        .await;

        merge_automatic(&db, None).await.unwrap();

        let logged = crate::models::operation_log::Entity::find()
            .filter(crate::models::operation_log::Column::EntityType.eq("book"))
            .filter(crate::models::operation_log::Column::Operation.eq("DELETE"))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].entity_id, "dup");
        assert!(
            logged[0]
                .payload
                .as_deref()
                .is_some_and(|p| p.contains("\"merged_into\":\"old\"")),
            "the log entry must name the survivor, got {:?}",
            logged[0].payload
        );
    }
}

/// The merge on a cr-sqlite enrolled device: the shape the reader who prompted
/// ADR-070 actually runs.
///
/// Isolated from the ordinary tests because `crsqlite_static::register()` is
/// process-wide. Run with:
/// `cargo test --features crsqlite-static merge_on_a_crr_schema`.
#[cfg(all(test, feature = "crsqlite-static"))]
mod crr_tests {
    use super::*;
    use crate::infrastructure::crsqlite_crr::{connect_pinned, finalize, setup_crrs};
    use crate::infrastructure::crsqlite_static;
    use crate::models::collection;
    use sea_orm::{ActiveModelTrait, DatabaseBackend};

    async fn insert_book(db: &DatabaseConnection, id: &str, created_at: &str) {
        book::ActiveModel {
            id: Set(id.to_owned()),
            title: Set("Dune".to_owned()),
            isbn: Set(Some("9782264024848".to_owned())),
            created_at: Set(created_at.to_owned()),
            updated_at: Set(created_at.to_owned()),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert book into CRR");
    }

    #[tokio::test]
    async fn merge_on_a_crr_schema_converges_and_is_captured_by_cr_sqlite() {
        crsqlite_static::register();
        let db = connect_pinned().await;
        crate::db::run_migrations(&db)
            .await
            .expect("run_migrations");
        setup_crrs(&db).await.expect("setup_crrs");

        insert_book(&db, "old", "2026-01-01T00:00:00Z").await;
        insert_book(&db, "young", "2026-02-01T00:00:00Z").await;
        collection::ActiveModel {
            id: Set("c1".to_owned()),
            name: Set("Perso".to_owned()),
            description: Set(None),
            source: Set("manual".to_owned()),
            created_at: Set("2026-01-01T00:00:00Z".to_owned()),
            updated_at: Set("2026-01-01T00:00:00Z".to_owned()),
        }
        .insert(&db)
        .await
        .expect("insert collection");
        for book_id in ["old", "young"] {
            collection_book::ActiveModel {
                collection_id: Set("c1".to_owned()),
                book_id: Set(book_id.to_owned()),
                added_at: Set("2026-03-01T00:00:00Z".to_owned()),
                volume_number: Set(None),
            }
            .insert(&db)
            .await
            .expect("attach book to collection");
        }

        // The clock companions cr-sqlite created carry a `book_id` of their own.
        // Discovery must skip them rather than treat the merge log as a
        // reference to rewrite, and the guard must not fire on them either.
        let report = merge_automatic(&db, None).await.expect("merge on a CRR");
        assert_eq!(report.groups_merged, 1);
        assert_eq!(report.books_removed, 1);

        let members = collection_book::Entity::find()
            .all(&db)
            .await
            .expect("read memberships");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].book_id, "old");

        // The deletion has to be in the merge log, or the account's other
        // devices would keep their copy of the duplicate forever.
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT count(*) AS n FROM crsql_changes WHERE \"table\" = 'books'".to_owned(),
            ))
            .await
            .expect("query crsql_changes")
            .expect("one row");
        let n: i64 = row.try_get("", "n").expect("decode count");
        assert!(n > 0, "the merge must be captured in crsql_changes");

        finalize(&db).await.expect("crsql_finalize");
    }
}
