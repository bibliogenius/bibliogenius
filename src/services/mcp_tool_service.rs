//! The MCP tool contract v1: six read-only tools over the owner's library.
//!
//! The schemas, the invariants, and the reasoning behind them are specified in
//! `bibliogenius-docs/docs/technical/mcp-tool-contract-v1.md` (decision record:
//! ADR-048). That document is the single reference. Change it first.
//!
//! Two properties of this module are load-bearing:
//!
//! 1. **It is transport-independent.** The tools take a request and return a
//!    value; they do not know whether they were reached over the stdio helper,
//!    the loopback `/api/mcp/rpc` endpoint, or a future E2EE channel. This is
//!    why they live here rather than in `api/mcp.rs`.
//! 2. **Everything here is the OWNER view.** Private books, wishlist entries,
//!    ratings, prices, reading progress, and loans with borrower names all flow
//!    out of these functions. `Book::redact_for_peer` exists to keep exactly
//!    this data away from peers. Never reuse these tools, or their output
//!    schemas, to serve a peer-facing surface: the two views differ in what they
//!    are permitted to disclose, not merely in which columns they select.

use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect,
};
use serde_json::{Value, json};

use crate::domain::{BookFilter, BookRepository, CollectionRepository};
use crate::infrastructure::repositories::{SeaOrmBookRepository, SeaOrmCollectionRepository};
use crate::models::Book;
use crate::models::book::{self, READING_STATUSES};
use crate::services::loan_service::{self, LoanFilter};

/// Upper bound on any `limit` argument. Prevents an assistant from pulling the
/// whole library into its context in one call.
const MAX_LIMIT: u64 = 200;
const DEFAULT_SEARCH_LIMIT: u64 = 20;
const DEFAULT_LIST_LIMIT: u64 = 50;

/// Why a tool call could not produce a payload.
///
/// The distinction drives the wire representation: a bad argument is a fact the
/// assistant can see and correct, so it becomes an MCP tool error inside a
/// successful JSON-RPC response. An unknown tool is a protocol fault.
#[derive(Debug)]
pub(crate) enum ToolError {
    InvalidArguments(String),
    UnknownTool(String),
    Internal(String),
}

/// The `tools/list` result: the vocabulary exposed to AI assistants.
///
/// The `reading_status` filter enum is derived from [`READING_STATUSES`] rather
/// than restated, so a sixth stored value reaches this schema without an edit
/// here. A test asserting a hardcoded list against a hardcoded list would pass
/// on the very day it should fail.
///
/// Crate-visible on purpose: see [`call_tool`].
pub(crate) fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "search_books",
                "description": "Full-text search across the owner's library: title, ISBN, subjects, and author names. Returns raw JSON.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Matches title, ISBN, subjects, or author name" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_SEARCH_LIMIT }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "get_statistics",
                "description": "Counts across the library: total, owned versus wishlist, books per reading status, active and returned loans.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "get_book",
                "description": "Fetch one full book record by stable uuid or by ISBN. Supply exactly one of the two.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "uuid": { "type": "string", "description": "Stable book identifier, as returned by search_books or list_books" },
                        "isbn": { "type": "string", "description": "Returns the oldest match when several books share this ISBN" }
                    }
                }
            },
            {
                "name": "list_books",
                "description": "Enumerate the library with optional filters, paginated. All filters combine with AND.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "reading_status": {
                            "type": "string",
                            "enum": READING_STATUSES,
                            "description": "Matches the stored status. `lent` and `borrowed` are not filterable here; use list_loans."
                        },
                        "owned": { "type": "boolean", "description": "true = owned, false = wishlist, omitted = both" },
                        "tag": { "type": "string", "description": "A shelf or subject" },
                        "collection": { "type": "string", "description": "Collection uuid, or collection name (case-insensitive)" },
                        "page": { "type": "integer", "minimum": 0, "default": 0 },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIST_LIMIT }
                    }
                }
            },
            {
                "name": "list_loans",
                "description": "Books currently lent out, or the history of returned loans. Includes borrower names. Paginated.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "scope": { "type": "string", "enum": ["active", "history", "all"], "default": "active" },
                        "page": { "type": "integer", "minimum": 0, "default": 0 },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIST_LIMIT }
                    }
                }
            },
            {
                "name": "wishlist_check",
                "description": "Whether an ISBN is already owned, merely wished for, or absent. Answers 'should I buy this?'.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "isbn": { "type": "string" } },
                    "required": ["isbn"]
                }
            }
        ]
    })
}

/// Wrap a tool payload in the MCP `tools/call` envelope.
///
/// Both representations are emitted: `structuredContent` for clients negotiating
/// protocol revision `2025-06-18` or later, and the JSON-stringified payload in
/// `content[0].text` for older ones (Claude Desktop commonly negotiates
/// `2024-11-05`). Older clients ignore the field they do not know.
pub(crate) fn envelope(payload: Value) -> Value {
    let text = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": payload,
    })
}

/// Wrap a failure as an MCP tool error: a *successful* JSON-RPC response whose
/// result carries `isError`. Protocol errors are invisible to the model in most
/// clients, so a recoverable mistake must arrive this way instead.
pub(crate) fn error_envelope(message: &str) -> Value {
    let text = serde_json::to_string(&json!({ "error": message })).unwrap_or_default();
    json!({
        "isError": true,
        "content": [{ "type": "text", "text": text }],
    })
}

/// Dispatch one `tools/call` to its implementation.
///
/// Crate-visible, not `pub`: these tools serve the OWNER view, and the only
/// caller allowed to reach them is a transport that has authenticated the owner
/// (today `api::mcp`, behind `McpAuth`). Keeping them out of the crate's public
/// API turns the module-level warning above into a compile-time constraint, so a
/// future peer-facing handler cannot quietly serve private data (ADR-048).
pub(crate) async fn call_tool(
    db: &DatabaseConnection,
    name: &str,
    args: &Value,
) -> Result<Value, ToolError> {
    match name {
        "search_books" => search_books(db, args).await,
        "get_statistics" => get_statistics(db).await,
        "get_book" => get_book(db, args).await,
        "list_books" => list_books(db, args).await,
        "list_loans" => list_loans(db, args).await,
        "wishlist_check" => wishlist_check(db, args).await,
        other => Err(ToolError::UnknownTool(other.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Projections
// ---------------------------------------------------------------------------

/// The narrow projection used in lists. Deliberately excludes summaries and
/// MARC records: forty books must not ship forty of each.
fn book_summary(book: &Book) -> Value {
    json!({
        "uuid": book.id,
        "title": book.title,
        "authors": book.authors.clone().unwrap_or_default(),
        "isbn": book.isbn,
        "publication_year": book.publication_year,
        // Effective status, an open string: `populate_authors` overlays `lent`
        // and `borrowed` from copy state. See contract section 3.5.
        "reading_status": book.reading_status,
        "owned": book.owned.unwrap_or(true),
    })
}

/// The full owner record. `collections` closes the discovery loop: an assistant
/// learns collection uuids here and spends them on `list_books`.
fn book_detail(book: &Book, collections: Value) -> Value {
    json!({
        "uuid": book.id,
        "title": book.title,
        "authors": book.authors.clone().unwrap_or_default(),
        "isbn": book.isbn,
        "publication_year": book.publication_year,
        "reading_status": book.reading_status,
        "owned": book.owned.unwrap_or(true),
        "summary": book.summary,
        "publisher": book.publisher,
        "page_count": book.page_count,
        "language": book.language,
        "subjects": book.subjects.clone().unwrap_or_default(),
        "collections": collections,
        "user_rating": book.user_rating,
        "price": book.price,
        "private": book.private.unwrap_or(false),
        "digital_formats": book.digital_formats.clone().unwrap_or_default(),
        "shelf_position": book.shelf_position,
        "available_copies": book.available_copies.unwrap_or(0),
        "started_reading_at": book.started_reading_at.clone().flatten(),
        "finished_reading_at": book.finished_reading_at.clone().flatten(),
        "cover_url": book.cover_url,
        "added_at": book.added_at,
        "updated_at": book.updated_at,
    })
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn required_str(args: &Value, key: &str) -> Result<String, ToolError> {
    optional_str(args, key)
        .ok_or_else(|| ToolError::InvalidArguments(format!("{} is required", key)))
}

/// A present-but-wrong-typed argument is refused rather than ignored.
///
/// Silently dropping `{"owned": "false"}` (a string, not a boolean) would widen
/// `list_books` from the wishlist to the whole library and answer confidently
/// with the wrong rows. An `InvalidArguments` is a fact the assistant can see
/// and correct; a silent default is not.
fn optional_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(ToolError::InvalidArguments(format!(
            "{} must be a boolean",
            key
        ))),
    }
}

/// Same contract as [`optional_bool`] for non-negative integers.
fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_u64().map(Some).ok_or_else(|| {
            ToolError::InvalidArguments(format!("{} must be a non-negative integer", key))
        }),
        Some(_) => Err(ToolError::InvalidArguments(format!(
            "{} must be a non-negative integer",
            key
        ))),
    }
}

/// Clamp a caller-supplied limit into `1..=MAX_LIMIT`, falling back to `default`.
///
/// Clamping the upper bound rather than rejecting: an assistant asking for 1000
/// books wants as many as it can have, and `total` tells it what it missed. A
/// `limit` of `0` is a different matter, since it asks for nothing and almost
/// certainly means the caller misunderstood the schema.
fn clamped_limit(args: &Value, default: u64) -> Result<u64, ToolError> {
    match optional_u64(args, "limit")? {
        None => Ok(default),
        Some(0) => Err(ToolError::InvalidArguments(
            "limit must be at least 1".to_string(),
        )),
        Some(n) => Ok(n.min(MAX_LIMIT)),
    }
}

fn book_repo(db: &DatabaseConnection) -> SeaOrmBookRepository {
    SeaOrmBookRepository::new(db.clone())
}

fn internal(e: impl std::fmt::Display) -> ToolError {
    ToolError::Internal(e.to_string())
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

async fn search_books(db: &DatabaseConnection, args: &Value) -> Result<Value, ToolError> {
    let query = required_str(args, "query")?;
    let limit = clamped_limit(args, DEFAULT_SEARCH_LIMIT)?;

    let result = book_repo(db)
        .find_all(BookFilter {
            query: Some(query.clone()),
            limit: Some(limit),
            page: Some(0),
            ..Default::default()
        })
        .await
        .map_err(internal)?;

    Ok(json!({
        "query": query,
        // The count before truncation, so the assistant can say "40 matches,
        // here are the first 20" instead of asserting there are 20.
        "total": result.total,
        "books": result.books.iter().map(book_summary).collect::<Vec<_>>(),
    }))
}

async fn get_statistics(db: &DatabaseConnection) -> Result<Value, ToolError> {
    let total_books = book::Entity::find().count(db).await.map_err(internal)?;
    let owned = book::Entity::find()
        .filter(book::Column::Owned.eq(true))
        .count(db)
        .await
        .map_err(internal)?;

    // One GROUP BY rather than one COUNT per known status. The service-layer
    // write gate is bypassed by direct repository writes and by cr-sqlite
    // account-sync replication, so a status outside READING_STATUSES can already
    // sit in the column. Counting them individually would drop those rows on the
    // floor and quietly break `sum(by_reading_status) == total_books`, which is
    // the invariant the regression test asserts (contract section 5.1).
    let grouped: Vec<(String, i64)> = book::Entity::find()
        .select_only()
        .column(book::Column::ReadingStatus)
        .column_as(book::Column::Id.count(), "count")
        .group_by(book::Column::ReadingStatus)
        .into_tuple()
        .all(db)
        .await
        .map_err(internal)?;

    // Seed the known five at zero so the assistant never has to tell "absent"
    // from "none", then overlay whatever the column actually holds.
    let mut by_reading_status = serde_json::Map::new();
    for status in READING_STATUSES {
        by_reading_status.insert(status.to_string(), json!(0));
    }
    for (status, count) in grouped {
        by_reading_status.insert(status, json!(count));
    }

    Ok(json!({
        "total_books": total_books,
        "owned": owned,
        "wishlist": total_books.saturating_sub(owned),
        "by_reading_status": Value::Object(by_reading_status),
        "loans": {
            "active": loan_service::count_active_loans(db).await.map_err(|e| internal(format!("{:?}", e)))?,
            "returned": loan_service::count_returned_loans(db).await.map_err(|e| internal(format!("{:?}", e)))?,
        }
    }))
}

async fn get_book(db: &DatabaseConnection, args: &Value) -> Result<Value, ToolError> {
    let uuid = optional_str(args, "uuid");
    let isbn = optional_str(args, "isbn");

    let (book, matched_by) = match (uuid, isbn) {
        (Some(_), Some(_)) => {
            return Err(ToolError::InvalidArguments(
                "supply either uuid or isbn, not both".to_string(),
            ));
        }
        (None, None) => {
            return Err(ToolError::InvalidArguments(
                "supply one of uuid or isbn".to_string(),
            ));
        }
        (Some(uuid), None) => (
            book_repo(db).find_by_id(&uuid).await.map_err(internal)?,
            "uuid",
        ),
        (None, Some(isbn)) => (
            book_repo(db).find_by_isbn(&isbn).await.map_err(internal)?,
            "isbn",
        ),
    };

    // "I do not have that book" is an answer, not a failure.
    let Some(book) = book else {
        return Ok(json!({ "found": false, "matched_by": matched_by, "book": null }));
    };

    let book_uuid = book.id.clone().unwrap_or_default();
    let collections = SeaOrmCollectionRepository::new(db.clone())
        .get_book_collections(&book_uuid)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|c| json!({ "uuid": c.id, "name": c.name }))
        .collect::<Vec<_>>();

    Ok(json!({
        "found": true,
        "matched_by": matched_by,
        "book": book_detail(&book, json!(collections)),
    }))
}

async fn list_books(db: &DatabaseConnection, args: &Value) -> Result<Value, ToolError> {
    let reading_status = optional_str(args, "reading_status");
    if let Some(status) = &reading_status
        && !READING_STATUSES.contains(&status.as_str())
    {
        return Err(ToolError::InvalidArguments(format!(
            "unknown reading_status '{}'; expected one of {}",
            status,
            READING_STATUSES.join(", ")
        )));
    }

    let page = optional_u64(args, "page")?.unwrap_or(0);
    let limit = clamped_limit(args, DEFAULT_LIST_LIMIT)?;

    let result = book_repo(db)
        .find_all(BookFilter {
            status: reading_status,
            tag: optional_str(args, "tag"),
            collection: optional_str(args, "collection"),
            owned: optional_bool(args, "owned")?,
            page: Some(page),
            limit: Some(limit),
            ..Default::default()
        })
        .await
        .map_err(internal)?;

    Ok(json!({
        "total": result.total,
        "page": page,
        "limit": limit,
        "books": result.books.iter().map(book_summary).collect::<Vec<_>>(),
    }))
}

async fn list_loans(db: &DatabaseConnection, args: &Value) -> Result<Value, ToolError> {
    let scope = optional_str(args, "scope").unwrap_or_else(|| "active".to_string());

    // The argument vocabulary is the user's, not the column's: nobody asks for
    // their "returned" loans.
    let status = match scope.as_str() {
        "active" => Some("active".to_string()),
        "history" => Some("returned".to_string()),
        "all" => None,
        other => {
            return Err(ToolError::InvalidArguments(format!(
                "unknown scope '{}'; expected active, history, or all",
                other
            )));
        }
    };

    let page = optional_u64(args, "page")?.unwrap_or(0);
    let limit = clamped_limit(args, DEFAULT_LIST_LIMIT)?;

    // The loan history grows without bound, so this list is paginated like every
    // other one. `total` comes from a COUNT rather than from the truncated page,
    // otherwise the assistant would report the page size as the history size.
    let total = match scope.as_str() {
        "active" => loan_service::count_active_loans(db).await,
        "history" => loan_service::count_returned_loans(db).await,
        _ => loan_service::count_loans(db).await,
    }
    .map_err(|e| internal(format!("{:?}", e)))?;

    let loans = loan_service::list_loans(
        db,
        LoanFilter {
            status,
            limit: Some(limit),
            offset: Some(page * limit),
            ..Default::default()
        },
    )
    .await
    .map_err(|e| internal(format!("{:?}", e)))?;

    let rows: Vec<Value> = loans
        .iter()
        .map(|l| {
            json!({
                "uuid": l.uuid,
                "book_uuid": l.book_id,
                "book_title": l.book_title,
                "isbn": l.isbn,
                "contact_name": l.contact_name,
                "loan_date": l.loan_date,
                "due_date": l.due_date,
                "return_date": l.return_date,
                "status": l.status,
                "notes": l.notes,
            })
        })
        .collect();

    Ok(json!({
        "scope": scope,
        "total": total,
        "page": page,
        "limit": limit,
        "loans": rows,
    }))
}

async fn wishlist_check(db: &DatabaseConnection, args: &Value) -> Result<Value, ToolError> {
    let isbn = required_str(args, "isbn")?;

    // `in_library` wins when an owned copy and a wishlist entry share an ISBN:
    // the useful answer to "should I buy this" is "you already have it".
    let owned = find_oldest_by_isbn(db, &isbn, Some(true)).await?;
    let (status, book) = match owned {
        Some(b) => ("in_library", Some(b)),
        None => match find_oldest_by_isbn(db, &isbn, None).await? {
            Some(b) => ("in_wishlist", Some(b)),
            None => ("absent", None),
        },
    };

    Ok(json!({
        "isbn": isbn,
        "status": status,
        "book_uuid": book.as_ref().map(|b| b.id.clone()),
        "title": book.as_ref().map(|b| b.title.clone()),
    }))
}

/// Oldest book carrying `isbn`, optionally constrained by ownership.
///
/// Ordering by `created_at` is what makes the answer stable: `books.isbn` has no
/// UNIQUE constraint and nothing deduplicates on insert.
async fn find_oldest_by_isbn(
    db: &DatabaseConnection,
    isbn: &str,
    owned: Option<bool>,
) -> Result<Option<book::Model>, ToolError> {
    let mut query = book::Entity::find().filter(book::Column::Isbn.eq(isbn));
    if let Some(owned) = owned {
        query = query.filter(book::Column::Owned.eq(owned));
    }
    query
        .order_by_asc(book::Column::CreatedAt)
        .one(db)
        .await
        .map_err(internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveModelTrait, Set};

    async fn db() -> DatabaseConnection {
        crate::infrastructure::db::init_db("sqlite::memory:")
            .await
            .expect("in-memory database")
    }

    /// Insert straight through the entity, deliberately bypassing
    /// `book_service::create_book`. That is what cr-sqlite account-sync
    /// replication does, and it is the only way to plant a status the
    /// service-layer gate would have refused.
    #[allow(clippy::too_many_arguments)]
    async fn insert_book(
        db: &DatabaseConnection,
        title: &str,
        isbn: Option<&str>,
        owned: bool,
        status: &str,
        private: bool,
        created_at: &str,
    ) -> String {
        book::ActiveModel {
            title: Set(title.to_string()),
            isbn: Set(isbn.map(str::to_string)),
            reading_status: Set(status.to_string()),
            owned: Set(owned),
            private: Set(private),
            created_at: Set(created_at.to_string()),
            updated_at: Set(created_at.to_string()),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("book inserted")
        .id
    }

    async fn a_book(db: &DatabaseConnection, title: &str) -> String {
        insert_book(
            db,
            title,
            None,
            true,
            "to_read",
            false,
            "2026-01-01T00:00:00Z",
        )
        .await
    }

    fn titles(payload: &Value) -> Vec<String> {
        payload["books"]
            .as_array()
            .expect("books array")
            .iter()
            .map(|b| b["title"].as_str().expect("title").to_string())
            .collect()
    }

    // -- tools/list ---------------------------------------------------------

    #[test]
    fn the_contract_exposes_exactly_the_six_read_only_tools() {
        let tools = tools_list();
        let names: Vec<&str> = tools["tools"]
            .as_array()
            .expect("array")
            .iter()
            .map(|t| t["name"].as_str().expect("name"))
            .collect();

        assert_eq!(
            names,
            vec![
                "search_books",
                "get_statistics",
                "get_book",
                "list_books",
                "list_loans",
                "wishlist_check"
            ]
        );
        // v1 is read-only. A write tool must arrive as a separately gated set,
        // never appended here (ADR-048).
        assert!(!names.iter().any(|n| n.starts_with("add_")
            || n.starts_with("update_")
            || n.starts_with("delete_")));
    }

    #[test]
    fn the_reading_status_filter_enum_is_derived_from_the_stored_vocabulary() {
        // Not a restatement of the five values: it asserts the schema tracks
        // READING_STATUSES. Hardcoding the list here would pass on the very day
        // a sixth stored value made the filter incomplete.
        let tools = tools_list();
        let list_books = tools["tools"]
            .as_array()
            .expect("array")
            .iter()
            .find(|t| t["name"] == "list_books")
            .expect("list_books");

        let enum_values: Vec<&str> =
            list_books["inputSchema"]["properties"]["reading_status"]["enum"]
                .as_array()
                .expect("enum")
                .iter()
                .map(|v| v.as_str().expect("string"))
                .collect();

        assert_eq!(enum_values, READING_STATUSES.to_vec());
    }

    // -- envelope -----------------------------------------------------------

    #[test]
    fn the_envelope_carries_the_same_payload_to_old_and_new_clients() {
        let payload = json!({ "total": 3, "books": [] });
        let env = envelope(payload.clone());

        // Newer clients read structuredContent...
        assert_eq!(env["structuredContent"], payload);
        // ...older ones parse content[0].text, and must see the same object.
        let text = env["content"][0]["text"].as_str().expect("text");
        let reparsed: Value = serde_json::from_str(text).expect("valid JSON");
        assert_eq!(reparsed, payload);
        assert!(env.get("isError").is_none());
    }

    // -- search_books -------------------------------------------------------

    #[tokio::test]
    async fn search_books_returns_structured_rows_rather_than_prose() {
        let db = db().await;
        a_book(&db, "Martin Eden").await;

        let payload = call_tool(&db, "search_books", &json!({ "query": "Martin" }))
            .await
            .expect("payload");

        assert_eq!(payload["total"], 1);
        let book = &payload["books"][0];
        assert_eq!(book["title"], "Martin Eden");
        // The pre-v1 tools answered "Found 1 books matching...". Nothing here may
        // be a sentence: the assistant writes those.
        assert!(book["uuid"].is_string());
        assert!(book["authors"].is_array());
        assert!(book["owned"].is_boolean());
    }

    #[tokio::test]
    async fn search_books_reports_the_true_total_when_truncating() {
        let db = db().await;
        for i in 0..3 {
            a_book(&db, &format!("Dune {}", i)).await;
        }

        let payload = call_tool(&db, "search_books", &json!({ "query": "Dune", "limit": 2 }))
            .await
            .expect("payload");

        // Without an honest total the assistant would assert the library holds
        // two Dunes.
        assert_eq!(payload["total"], 3);
        assert_eq!(payload["books"].as_array().expect("array").len(), 2);
    }

    #[tokio::test]
    async fn search_books_matches_an_author_name() {
        use crate::models::{author, book_authors};

        let db = db().await;
        let book_id = a_book(&db, "Untitled").await;
        let author_id = author::ActiveModel {
            name: Set("Jack London".to_string()),
            created_at: Set("2026-01-01T00:00:00Z".to_string()),
            updated_at: Set("2026-01-01T00:00:00Z".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("author inserted")
        .id;

        book_authors::ActiveModel {
            book_id: Set(book_id),
            author_id: Set(author_id),
        }
        .insert(&db)
        .await
        .expect("authorship inserted");

        // Regression: the author subquery joined `authors.id`, a column that
        // stopped existing when `authors` was rebuilt on a uuid primary key.
        // Every query-by-author raised "no such column: authors.id".
        let payload = call_tool(&db, "search_books", &json!({ "query": "London" }))
            .await
            .expect("author search must not raise a database error");

        assert_eq!(payload["total"], 1);
        assert_eq!(payload["books"][0]["title"], "Untitled");
        assert_eq!(payload["books"][0]["authors"][0], "Jack London");
    }

    #[tokio::test]
    async fn search_books_finds_nothing_without_erroring() {
        let db = db().await;
        let payload = call_tool(&db, "search_books", &json!({ "query": "absent" }))
            .await
            .expect("an empty result is not an error");
        assert_eq!(payload["total"], 0);
        assert!(payload["books"].as_array().expect("array").is_empty());
    }

    #[tokio::test]
    async fn search_books_without_a_query_is_a_recoverable_argument_error() {
        let db = db().await;
        let err = call_tool(&db, "search_books", &json!({})).await;
        assert!(matches!(err, Err(ToolError::InvalidArguments(_))));
    }

    // -- get_statistics -----------------------------------------------------

    #[tokio::test]
    async fn get_statistics_on_an_empty_library_reports_every_known_status_as_zero() {
        let db = db().await;
        let payload = call_tool(&db, "get_statistics", &json!({}))
            .await
            .expect("payload");

        assert_eq!(payload["total_books"], 0);
        for status in READING_STATUSES {
            assert_eq!(
                payload["by_reading_status"][status], 0,
                "the assistant must never distinguish 'absent' from 'none'"
            );
        }
    }

    #[tokio::test]
    async fn get_statistics_separates_owned_from_wishlist() {
        let db = db().await;
        insert_book(
            &db,
            "Owned",
            None,
            true,
            "read",
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "Wanted",
            None,
            false,
            "wanting",
            false,
            "2026-01-02T00:00:00Z",
        )
        .await;

        let payload = call_tool(&db, "get_statistics", &json!({}))
            .await
            .expect("payload");

        assert_eq!(payload["total_books"], 2);
        assert_eq!(payload["owned"], 1);
        assert_eq!(payload["wishlist"], 1);
        assert_eq!(payload["by_reading_status"]["read"], 1);
        assert_eq!(payload["by_reading_status"]["wanting"], 1);
        assert_eq!(payload["loans"]["active"], 0);
    }

    /// The regression guard for stored-vocabulary drift (contract 5.1).
    ///
    /// It names no status, so it keeps firing after the vocabulary changes.
    /// Counting with one COUNT per known status would drop the unknown row and
    /// break the sum silently.
    #[tokio::test]
    async fn every_stored_status_is_counted_even_one_the_write_gate_would_refuse() {
        let db = db().await;
        insert_book(
            &db,
            "Normal",
            None,
            true,
            "reading",
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;
        // A value no `validate_reading_status` would accept, as an older or
        // newer device could replicate through cr-sqlite.
        insert_book(
            &db,
            "Replicated",
            None,
            true,
            "paused",
            false,
            "2026-01-02T00:00:00Z",
        )
        .await;

        let payload = call_tool(&db, "get_statistics", &json!({}))
            .await
            .expect("payload");

        let buckets = payload["by_reading_status"].as_object().expect("map");
        let summed: i64 = buckets.values().map(|v| v.as_i64().expect("count")).sum();

        assert_eq!(
            summed, payload["total_books"],
            "sum(by_reading_status) must equal total_books for any stored value"
        );
        assert_eq!(
            buckets["paused"], 1,
            "the unknown status surfaces as its own key"
        );
    }

    // -- get_book -----------------------------------------------------------

    #[tokio::test]
    async fn get_book_by_uuid_exposes_the_owner_view() {
        let db = db().await;
        let uuid = insert_book(
            &db,
            "Secret",
            Some("9780000000001"),
            true,
            "read",
            true, // private: hidden from peers, never from the owner
            "2026-01-01T00:00:00Z",
        )
        .await;

        let payload = call_tool(&db, "get_book", &json!({ "uuid": uuid }))
            .await
            .expect("payload");

        assert_eq!(payload["found"], true);
        assert_eq!(payload["matched_by"], "uuid");
        let book = &payload["book"];
        assert_eq!(book["title"], "Secret");
        // These are exactly the fields `redact_for_peer` strips. Their presence
        // is the contract's owner-view guarantee, not an oversight.
        assert_eq!(book["private"], true);
        assert!(book.get("user_rating").is_some());
        assert!(book.get("price").is_some());
        assert!(book.get("shelf_position").is_some());
        assert!(book["collections"].is_array());
    }

    #[tokio::test]
    async fn get_book_by_isbn_returns_the_oldest_of_several_duplicates() {
        let db = db().await;
        // `books.isbn` has no UNIQUE constraint and nothing dedupes on insert.
        insert_book(
            &db,
            "Second copy",
            Some("9780000000002"),
            true,
            "to_read",
            false,
            "2026-02-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "First copy",
            Some("9780000000002"),
            true,
            "to_read",
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;

        let payload = call_tool(&db, "get_book", &json!({ "isbn": "9780000000002" }))
            .await
            .expect("payload");

        assert_eq!(payload["matched_by"], "isbn");
        assert_eq!(
            payload["book"]["title"], "First copy",
            "the oldest match, so repeated lookups agree with each other"
        );
    }

    #[tokio::test]
    async fn get_book_that_is_absent_is_an_answer_not_an_error() {
        let db = db().await;
        let payload = call_tool(&db, "get_book", &json!({ "isbn": "9789999999999" }))
            .await
            .expect("'no' is an answer to 'do I have this book'");

        assert_eq!(payload["found"], false);
        assert_eq!(payload["book"], Value::Null);
    }

    #[tokio::test]
    async fn get_book_demands_exactly_one_selector() {
        let db = db().await;
        assert!(matches!(
            call_tool(&db, "get_book", &json!({})).await,
            Err(ToolError::InvalidArguments(_))
        ));
        assert!(matches!(
            call_tool(&db, "get_book", &json!({ "uuid": "a", "isbn": "b" })).await,
            Err(ToolError::InvalidArguments(_))
        ));
    }

    // -- list_books ---------------------------------------------------------

    #[tokio::test]
    async fn list_books_owned_false_yields_the_wishlist() {
        let db = db().await;
        insert_book(
            &db,
            "Held",
            None,
            true,
            "read",
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "Wanted",
            None,
            false,
            "wanting",
            false,
            "2026-01-02T00:00:00Z",
        )
        .await;

        let payload = call_tool(&db, "list_books", &json!({ "owned": false }))
            .await
            .expect("payload");

        assert_eq!(titles(&payload), vec!["Wanted"]);
    }

    #[tokio::test]
    async fn list_books_shows_the_owner_their_own_private_books() {
        let db = db().await;
        insert_book(
            &db,
            "Private",
            None,
            true,
            "read",
            true,
            "2026-01-01T00:00:00Z",
        )
        .await;

        let payload = call_tool(&db, "list_books", &json!({ "owned": true }))
            .await
            .expect("payload");

        // The trap this guards: `BookFilter::owned_only` also forces
        // `private = false`, because it builds the peer catalogue. Reusing it
        // here would hide the owner's private books from the owner.
        assert_eq!(titles(&payload), vec!["Private"]);
    }

    #[tokio::test]
    async fn list_books_paginates_and_reports_the_total_before_truncation() {
        let db = db().await;
        for i in 0..5 {
            a_book(&db, &format!("Book {}", i)).await;
        }

        let payload = call_tool(&db, "list_books", &json!({ "page": 1, "limit": 2 }))
            .await
            .expect("payload");

        assert_eq!(payload["total"], 5);
        assert_eq!(payload["page"], 1);
        assert_eq!(payload["limit"], 2);
        assert_eq!(payload["books"].as_array().expect("array").len(), 2);
    }

    #[tokio::test]
    async fn list_books_rejects_a_status_outside_the_stored_vocabulary() {
        let db = db().await;
        // `lent` is an *effective* value, never a stored one: it is not filterable.
        assert!(matches!(
            call_tool(&db, "list_books", &json!({ "reading_status": "lent" })).await,
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[tokio::test]
    async fn list_books_resolves_a_collection_by_uuid_or_by_name() {
        use crate::models::{collection, collection_book};

        let db = db().await;
        let book_id = a_book(&db, "In collection").await;
        a_book(&db, "Outside").await;

        let collection_id = uuid::Uuid::new_v4().to_string();
        collection::ActiveModel {
            id: Set(collection_id.clone()),
            name: Set("La Pléiade".to_string()),
            description: Set(None),
            source: Set("manual".to_string()),
            created_at: Set("2026-01-01T00:00:00Z".to_string()),
            updated_at: Set("2026-01-01T00:00:00Z".to_string()),
        }
        .insert(&db)
        .await
        .expect("collection inserted");

        collection_book::ActiveModel {
            collection_id: Set(collection_id.clone()),
            book_id: Set(book_id),
            added_at: Set("2026-01-01T00:00:00Z".to_string()),
        }
        .insert(&db)
        .await
        .expect("membership inserted");

        let by_uuid = call_tool(&db, "list_books", &json!({ "collection": collection_id }))
            .await
            .expect("payload");
        assert_eq!(titles(&by_uuid), vec!["In collection"]);

        // Names reach the assistant from the user, and case is not the user's problem.
        let by_name = call_tool(&db, "list_books", &json!({ "collection": "la pléiade" }))
            .await
            .expect("payload");
        assert_eq!(titles(&by_name), vec!["In collection"]);
    }

    #[tokio::test]
    async fn list_books_in_an_unknown_collection_is_empty_not_an_error() {
        let db = db().await;
        a_book(&db, "Somewhere else").await;

        let payload = call_tool(&db, "list_books", &json!({ "collection": "Nonexistent" }))
            .await
            .expect("an unknown collection holds nothing");

        assert_eq!(payload["total"], 0);
        assert!(payload["books"].as_array().expect("array").is_empty());
    }

    // -- list_loans ---------------------------------------------------------

    /// Book with an available copy, plus a contact to lend it to.
    async fn a_lendable_book(db: &DatabaseConnection, title: &str) -> (String, String) {
        use crate::models::{contact, copy};

        let book_id = a_book(db, title).await;
        let copy_id = copy::ActiveModel {
            book_id: Set(book_id),
            library_id: Set(1),
            status: Set("available".to_string()),
            is_temporary: Set(false),
            created_at: Set("2026-01-01T00:00:00Z".to_string()),
            updated_at: Set("2026-01-01T00:00:00Z".to_string()),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("copy inserted")
        .id;

        let contact_id = contact::ActiveModel {
            r#type: Set("friend".to_string()),
            name: Set("Osvaldo".to_string()),
            created_at: Set("2026-01-01T00:00:00Z".to_string()),
            updated_at: Set("2026-01-01T00:00:00Z".to_string()),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("contact inserted")
        .id;

        (copy_id, contact_id)
    }

    #[tokio::test]
    async fn list_loans_with_nothing_lent_is_empty_not_an_error() {
        let db = db().await;
        let payload = call_tool(&db, "list_loans", &json!({}))
            .await
            .expect("payload");
        assert_eq!(payload["scope"], "active");
        assert_eq!(payload["total"], 0);
    }

    #[tokio::test]
    async fn list_loans_separates_active_from_history_and_names_the_borrower() {
        let db = db().await;
        let (copy_id, contact_id) = a_lendable_book(&db, "Lent out").await;

        loan_service::create_loan(
            &db,
            crate::models::loan::LoanDto {
                id: None,
                copy_id,
                contact_id,
                library_id: 1,
                loan_date: "2026-01-01".to_string(),
                due_date: "2026-02-01".to_string(),
                return_date: None,
                status: None,
                notes: None,
            },
        )
        .await
        .expect("loan created");

        let active = call_tool(&db, "list_loans", &json!({ "scope": "active" }))
            .await
            .expect("payload");
        assert_eq!(active["total"], 1);
        assert_eq!(active["loans"][0]["book_title"], "Lent out");
        // Borrower names are owner data, and peers never see them.
        assert_eq!(active["loans"][0]["contact_name"], "Osvaldo");
        assert_eq!(active["loans"][0]["status"], "active");

        let history = call_tool(&db, "list_loans", &json!({ "scope": "history" }))
            .await
            .expect("payload");
        assert_eq!(history["total"], 0, "nothing has been returned yet");
    }

    #[tokio::test]
    async fn list_loans_rejects_an_unknown_scope() {
        let db = db().await;
        assert!(matches!(
            call_tool(&db, "list_loans", &json!({ "scope": "returned" })).await,
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[tokio::test]
    async fn list_loans_is_paginated_and_reports_the_untruncated_total() {
        let db = db().await;
        for i in 0..3 {
            let (copy_id, contact_id) = a_lendable_book(&db, &format!("Lent {}", i)).await;
            loan_service::create_loan(
                &db,
                crate::models::loan::LoanDto {
                    id: None,
                    copy_id,
                    contact_id,
                    library_id: 1,
                    loan_date: "2026-01-01".to_string(),
                    due_date: "2026-02-01".to_string(),
                    return_date: None,
                    status: None,
                    notes: None,
                },
            )
            .await
            .expect("loan created");
        }

        let payload = call_tool(&db, "list_loans", &json!({ "limit": 2 }))
            .await
            .expect("payload");

        // The loan history grows without bound; an unpaginated tool would pour it
        // all into the assistant's context.
        assert_eq!(payload["loans"].as_array().expect("array").len(), 2);
        assert_eq!(payload["limit"], 2);
        assert_eq!(
            payload["total"], 3,
            "total counts the loans, not the returned page"
        );

        let second = call_tool(&db, "list_loans", &json!({ "limit": 2, "page": 1 }))
            .await
            .expect("payload");
        assert_eq!(second["loans"].as_array().expect("array").len(), 1);
    }

    // -- argument typing ----------------------------------------------------

    #[tokio::test]
    async fn a_wrong_typed_argument_is_refused_rather_than_ignored() {
        let db = db().await;
        insert_book(
            &db,
            "Held",
            None,
            true,
            "read",
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "Wanted",
            None,
            false,
            "wanting",
            false,
            "2026-01-02T00:00:00Z",
        )
        .await;

        // Silently dropping this would answer with the whole library while the
        // caller believes it asked for the wishlist alone.
        assert!(matches!(
            call_tool(&db, "list_books", &json!({ "owned": "false" })).await,
            Err(ToolError::InvalidArguments(_))
        ));
        assert!(matches!(
            call_tool(&db, "list_books", &json!({ "page": "1" })).await,
            Err(ToolError::InvalidArguments(_))
        ));
        assert!(matches!(
            call_tool(&db, "list_books", &json!({ "limit": -3 })).await,
            Err(ToolError::InvalidArguments(_))
        ));
        // Asking for nothing is a misread of the schema, not a request.
        assert!(matches!(
            call_tool(&db, "list_books", &json!({ "limit": 0 })).await,
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[tokio::test]
    async fn an_absent_or_null_argument_falls_back_to_the_default() {
        let db = db().await;
        a_book(&db, "Anything").await;

        for args in [json!({}), json!({ "owned": null, "limit": null })] {
            let payload = call_tool(&db, "list_books", &args).await.expect("payload");
            assert_eq!(payload["total"], 1);
            assert_eq!(payload["limit"], DEFAULT_LIST_LIMIT);
        }
    }

    #[tokio::test]
    async fn an_oversized_limit_is_clamped_rather_than_refused() {
        let db = db().await;
        a_book(&db, "Anything").await;

        let payload = call_tool(&db, "list_books", &json!({ "limit": 100_000 }))
            .await
            .expect("an assistant asking for everything wants as much as it can have");
        assert_eq!(payload["limit"], MAX_LIMIT);
    }

    #[tokio::test]
    async fn list_books_filters_by_tag() {
        use sea_orm::ActiveModelTrait;

        let db = db().await;
        let id = a_book(&db, "Tagged").await;
        a_book(&db, "Untagged").await;
        let mut tagged: book::ActiveModel = book::Entity::find_by_id(id)
            .one(&db)
            .await
            .expect("query")
            .expect("book")
            .into();
        tagged.subjects = Set(Some(r#"["Science Fiction"]"#.to_string()));
        tagged.update(&db).await.expect("subjects set");

        let payload = call_tool(&db, "list_books", &json!({ "tag": "Science Fiction" }))
            .await
            .expect("payload");
        assert_eq!(titles(&payload), vec!["Tagged"]);
    }

    #[tokio::test]
    async fn get_statistics_counts_an_active_loan() {
        let db = db().await;
        let (copy_id, contact_id) = a_lendable_book(&db, "Lent out").await;
        loan_service::create_loan(
            &db,
            crate::models::loan::LoanDto {
                id: None,
                copy_id,
                contact_id,
                library_id: 1,
                loan_date: "2026-01-01".to_string(),
                due_date: "2026-02-01".to_string(),
                return_date: None,
                status: None,
                notes: None,
            },
        )
        .await
        .expect("loan created");

        let payload = call_tool(&db, "get_statistics", &json!({}))
            .await
            .expect("payload");

        assert_eq!(payload["loans"]["active"], 1);
        assert_eq!(payload["loans"]["returned"], 0);
        // A lent book still counts under its stored status (contract 3.5).
        assert_eq!(payload["by_reading_status"]["to_read"], 1);
    }

    // -- wishlist_check -----------------------------------------------------

    #[tokio::test]
    async fn wishlist_check_distinguishes_owned_wanted_and_absent() {
        let db = db().await;
        insert_book(
            &db,
            "Held",
            Some("9780000000010"),
            true,
            "read",
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "Wanted",
            Some("9780000000011"),
            false,
            "wanting",
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;

        let owned = call_tool(&db, "wishlist_check", &json!({ "isbn": "9780000000010" }))
            .await
            .expect("payload");
        assert_eq!(owned["status"], "in_library");
        assert_eq!(owned["title"], "Held");
        assert!(owned["book_uuid"].is_string());

        let wanted = call_tool(&db, "wishlist_check", &json!({ "isbn": "9780000000011" }))
            .await
            .expect("payload");
        assert_eq!(wanted["status"], "in_wishlist");

        let absent = call_tool(&db, "wishlist_check", &json!({ "isbn": "9789999999999" }))
            .await
            .expect("payload");
        assert_eq!(absent["status"], "absent");
        assert_eq!(absent["book_uuid"], Value::Null);
    }

    #[tokio::test]
    async fn wishlist_check_prefers_owned_when_an_isbn_is_both_held_and_wished_for() {
        let db = db().await;
        insert_book(
            &db,
            "Wished",
            Some("9780000000020"),
            false,
            "wanting",
            false,
            "2026-01-01T00:00:00Z",
        )
        .await;
        insert_book(
            &db,
            "Held",
            Some("9780000000020"),
            true,
            "read",
            false,
            "2026-02-01T00:00:00Z",
        )
        .await;

        let payload = call_tool(&db, "wishlist_check", &json!({ "isbn": "9780000000020" }))
            .await
            .expect("payload");

        // "Should I buy it?" is answered by the copy on the shelf, even though
        // the wishlist row is older.
        assert_eq!(payload["status"], "in_library");
        assert_eq!(payload["title"], "Held");
    }

    #[tokio::test]
    async fn an_unknown_tool_is_a_protocol_fault_not_a_tool_error() {
        let db = db().await;
        // The distinction drives the wire shape: this one becomes JSON-RPC -32601.
        assert!(matches!(
            call_tool(&db, "delete_everything", &json!({})).await,
            Err(ToolError::UnknownTool(_))
        ));
    }
}
