use sea_orm::entity::prelude::*;
use sea_orm::{ModelTrait, NotSet, Set};
use serde::{Deserialize, Serialize};

/// Strips non-alphanumeric characters from a timestamp so it can be
/// embedded as a `?v=` query parameter without percent-encoding.
/// A SQLite timestamp `"2026-04-20 10:30:00"` becomes `"20260420103000"`,
/// which is deterministic, short enough for URL hygiene, and changes on
/// every book edit — exactly what cache-busting needs.
fn cover_version_tag(updated_at: &str) -> String {
    updated_at
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// Builds a hub-hosted cover URL, optionally suffixed with a `?v={tag}`
/// cache-buster derived from `updated_at`. The caller is responsible for
/// passing a canonical `hub_cover_prefix` (without trailing slash).
fn build_hub_cover_url(hub_cover_prefix: &str, book_id: i32, updated_at: Option<&str>) -> String {
    let base = format!("{hub_cover_prefix}/{book_id}");
    append_version(base, updated_at)
}

/// Builds a LAN peer cover URL (`/api/books/{id}/cover`), with the same
/// optional `?v={tag}` cache-buster logic as the hub variant. The peer's
/// own HTTP cover endpoint ignores query strings so this is safe.
fn build_lan_cover_url(book_id: i32, updated_at: Option<&str>) -> String {
    append_version(format!("/api/books/{book_id}/cover"), updated_at)
}

fn append_version(base: String, updated_at: Option<&str>) -> String {
    match updated_at {
        Some(s) if !s.is_empty() => {
            let tag = cover_version_tag(s);
            if tag.is_empty() {
                base
            } else {
                format!("{base}?v={tag}")
            }
        }
        _ => base,
    }
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "books")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub title: String,
    pub isbn: Option<String>,
    pub summary: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
    pub dewey_decimal: Option<String>,
    pub lcc: Option<String>,
    pub subjects: Option<String>, // JSON array
    pub marc_record: Option<String>,
    pub cataloguing_notes: Option<String>,
    pub source_data: Option<String>,
    pub shelf_position: Option<i32>,
    /// Personal reading progress status.
    /// Valid values:
    /// - `to_read`: Haven't started yet
    /// - `reading`: Currently reading
    /// - `read`: Finished reading
    /// - `wanting`: Wishlist (want to read someday)
    /// - `abandoned`: Stopped reading
    ///
    /// NOTE: Do NOT use `lent`/`borrowed` here - those belong to Copy.status
    #[sea_orm(default_value = "to_read")]
    pub reading_status: String,
    pub finished_reading_at: Option<String>,
    pub started_reading_at: Option<String>,
    pub cover_url: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub user_rating: Option<i32>, // 0-10 scale, NULL = not rated
    /// Whether I physically own this book.
    /// - `true`: I have this book → a Copy should exist
    /// - `false`: Wishlist/wanted → no Copy
    #[sea_orm(default_value = "true")]
    pub owned: bool,
    /// Price of the book (EUR). Used by bookseller profile.
    /// If set, this is the default price for all copies of this book.
    pub price: Option<f64>,
    pub digital_formats: Option<String>, // JSON array
    /// When true, this book is hidden from network peers.
    /// Only relevant for the "reader" profile (librarian/bookseller always share all books).
    #[sea_orm(default_value = "false")]
    pub private: bool,
    pub page_count: Option<i32>,
    pub loan_duration_days: Option<i32>,
    /// ISO 8601 timestamp of the last failed hub cover upload for this book.
    /// NULL when the most recent attempt succeeded or none ever ran. The
    /// owner's UI reads this to surface a warning badge while a retry is
    /// pending. Cleared on successful upload and on hub purge.
    pub hub_cover_upload_failed_at: Option<String>,
}

// ... (Relation enum and Related impls omit for brevity) ...
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::copy::Entity")]
    Copies,
}

impl Related<super::author::Entity> for Entity {
    fn to() -> RelationDef {
        super::book_authors::Relation::Author.def()
    }

    fn via() -> Option<RelationDef> {
        Some(super::book_authors::Relation::Book.def().rev())
    }
}

impl Related<super::tag::Entity> for Entity {
    fn to() -> RelationDef {
        super::book_tags::Relation::Tag.def()
    }

    fn via() -> Option<RelationDef> {
        Some(super::book_tags::Relation::Book.def().rev())
    }
}

impl ActiveModelBehavior for ActiveModel {}

// DTO for API responses
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Book {
    pub id: Option<i32>,
    pub title: String,
    pub isbn: Option<String>,
    pub summary: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dewey_decimal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lcc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subjects: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marc_record: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cataloguing_notes: Option<String>,
    pub source_data: Option<String>,
    pub shelf_position: Option<i32>,
    pub reading_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_reading_at: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_reading_at: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authors: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub large_cover_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_rating: Option<i32>, // 0-10 scale
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owned: Option<bool>, // Whether I own this book (default true)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<f64>, // Price in EUR (for bookseller profile)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>, // Language code (e.g., "fr", "en", "fre", "eng")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digital_formats: Option<Vec<String>>, // ["ebook", "audiobook"]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_copies: Option<i32>, // Number of copies with status "available"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>, // When true, hidden from network peers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loan_duration_days: Option<i32>,
    /// When this book was added to its owner's library (ISO 8601, maps to
    /// `books.created_at`). Broadcast to peers so every viewer sees the
    /// same "new" badge regardless of when they first discovered the book.
    /// Not redacted by `redact_for_peer`: it is editorial metadata, not
    /// personal annotation.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub added_at: Option<String>,
    /// Last modification timestamp of the book row (maps to `books.updated_at`).
    /// Used to build a cache-busting `?v=` suffix on cover URLs so peers
    /// refetch the image after the owner re-uploads it, without having to
    /// wait out the 7-day image cache TTL.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub updated_at: Option<String>,
    /// ISO 8601 timestamp of the last failed hub cover upload. Exposed to
    /// the owner's UI so a warning badge can be shown while a retry pends.
    /// Redacted from peer-facing responses (see `redact_for_peer`) so
    /// visitors never see another library's internal sync state.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub hub_cover_upload_failed_at: Option<String>,
}

impl From<Model> for Book {
    fn from(model: Model) -> Self {
        let subjects: Option<Vec<String>> = model
            .subjects
            .map(|s| serde_json::from_str(&s).unwrap_or_default());

        // Extract language from source_data if available
        let language: Option<String> = model.source_data.as_ref().and_then(|sd| {
            serde_json::from_str::<serde_json::Value>(sd)
                .ok()
                .and_then(|json| {
                    json.get("languages")
                        .and_then(|l| l.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
        });

        let digital_formats: Option<Vec<String>> = model
            .digital_formats
            .map(|s| serde_json::from_str(&s).unwrap_or_default());

        Self {
            id: Some(model.id),
            title: model.title,
            isbn: model.isbn,
            summary: model.summary,
            publisher: model.publisher,
            publication_year: model.publication_year,
            dewey_decimal: model.dewey_decimal,
            lcc: model.lcc,
            subjects,
            marc_record: model.marc_record,
            cataloguing_notes: model.cataloguing_notes,
            source_data: model.source_data,
            shelf_position: model.shelf_position,
            reading_status: Some(model.reading_status),
            finished_reading_at: Some(model.finished_reading_at),
            started_reading_at: Some(model.started_reading_at),
            source: Some("Local".to_string()),
            author: None,               // Populated by API handlers
            authors: None,              // Populated by API handlers
            cover_url: model.cover_url, // Use stored cover_url
            large_cover_url: None,
            user_rating: model.user_rating,
            owned: Some(model.owned),
            price: model.price,
            language,
            digital_formats,
            available_copies: None, // Populated separately
            private: Some(model.private),
            page_count: model.page_count,
            loan_duration_days: model.loan_duration_days,
            added_at: Some(model.created_at),
            updated_at: Some(model.updated_at),
            hub_cover_upload_failed_at: model.hub_cover_upload_failed_at,
        }
    }
}

impl Book {
    /// Subquery returning book IDs whose related author name matches `query` (LIKE %query%).
    /// Use with `Expr::col(book::Column::Id).in_subquery(...)` to filter books by author.
    pub fn author_search_subquery(query: &str) -> sea_orm::sea_query::SelectStatement {
        use sea_orm::sea_query::{Alias, Expr, Query};

        Query::select()
            .column(Alias::new("book_id"))
            .from(Alias::new("book_authors"))
            .inner_join(
                Alias::new("authors"),
                Expr::col((Alias::new("book_authors"), Alias::new("author_id")))
                    .equals((Alias::new("authors"), Alias::new("id"))),
            )
            .and_where(
                Expr::col((Alias::new("authors"), Alias::new("name"))).like(format!("%{}%", query)),
            )
            .to_owned()
    }

    /// Rewrites local file-system cover paths so peers can fetch covers.
    ///
    /// When `hub_cover_prefix` is provided (e.g. `https://hub.../api/directory/{nodeId}/covers`),
    /// local paths become absolute hub URLs that work for both LAN and relay peers.
    /// Without it, falls back to relative `/api/books/{id}/cover` (LAN only).
    /// HTTP URLs and paths already starting with `/api` are left untouched.
    /// Every rewritten URL carries a `?v={tag}` suffix derived from the
    /// book's `updated_at` so peers refetch automatically after a re-upload.
    ///
    /// This is the LAN variant: callers that serve the payload over local
    /// HTTP (peers on the same network) can rely on the relative fallback
    /// because the peer resolves it against the known base URL. For
    /// payloads that may travel via hub relay to a peer with no direct
    /// connectivity, use `rewrite_cover_urls_for_relay` instead.
    pub fn rewrite_local_cover_urls(books: &mut [Book], hub_cover_prefix: Option<&str>) {
        for book in books {
            if let Some(ref url) = book.cover_url
                && !url.starts_with("http")
                && !url.starts_with("/api")
                && let Some(id) = book.id
            {
                let version = book.updated_at.as_deref();
                book.cover_url = Some(match hub_cover_prefix {
                    Some(prefix) => build_hub_cover_url(prefix, id, version),
                    None => build_lan_cover_url(id, version),
                });
            }
        }
    }

    /// Rewrites local cover paths strictly: errors if any book carries a
    /// local filesystem path while no hub prefix is configured.
    ///
    /// Used for relay-bound payloads where the relative `/api/books/{id}/cover`
    /// fallback is unreachable (the peer has no direct HTTP route to us).
    /// HTTP URLs and `/api` paths pass through untouched. Books with local
    /// paths but no id are left as-is (they are already non-servable; the
    /// caller can strip them separately if desired).
    pub fn rewrite_cover_urls_strict(
        books: &mut [Book],
        hub_cover_prefix: Option<&str>,
    ) -> Result<(), CoverRewriteError> {
        let offenders: Vec<i32> = books
            .iter()
            .filter(|b| {
                b.cover_url
                    .as_deref()
                    .is_some_and(|u| !u.starts_with("http") && !u.starts_with("/api"))
            })
            .filter_map(|b| b.id)
            .collect();

        if offenders.is_empty() {
            return Ok(());
        }

        let prefix = match hub_cover_prefix {
            Some(p) => p,
            None => {
                return Err(CoverRewriteError {
                    book_ids: offenders,
                });
            }
        };

        for book in books.iter_mut() {
            if let Some(ref url) = book.cover_url
                && !url.starts_with("http")
                && !url.starts_with("/api")
                && let Some(id) = book.id
            {
                let version = book.updated_at.as_deref();
                book.cover_url = Some(build_hub_cover_url(prefix, id, version));
            }
        }
        Ok(())
    }

    /// Relay-safe wrapper around `rewrite_cover_urls_strict`. On failure,
    /// logs ERROR and strips the offending `cover_url` entries to `None`
    /// so no unreachable path leaks into the envelope sent over the hub.
    ///
    /// Callers that produce E2EE / relay payloads should prefer this over
    /// `rewrite_local_cover_urls` so the peer never receives
    /// `/api/books/{id}/cover` paths it cannot resolve.
    pub fn rewrite_cover_urls_for_relay(books: &mut [Book], hub_cover_prefix: Option<&str>) {
        if let Err(e) = Self::rewrite_cover_urls_strict(books, hub_cover_prefix) {
            tracing::error!(
                "relay cover rewrite failed, stripping {} offender(s) to None: {e}",
                e.book_ids.len()
            );
            for book in books.iter_mut() {
                if let Some(ref u) = book.cover_url
                    && !u.starts_with("http")
                    && !u.starts_with("/api")
                {
                    book.cover_url = None;
                }
            }
        }
    }

    /// Strip fields that must not leak to unauthenticated peer callers.
    ///
    /// The HTTP catalog endpoints (`/api/books`, `/api/books/:id`) are
    /// reachable without a JWT so peers can browse a library before
    /// pairing (ADR-026 mDNS fallback). That flow must expose bibliographic
    /// metadata (title, author, ISBN, summary, cover…) but NEVER the
    /// owner's personal annotations: reading status, rating, notes,
    /// purchase info, physical organisation.
    ///
    /// Callers in the handler layer invoke this on every Book DTO before
    /// serialisation when no valid Claims was extracted. Fields set to
    /// `None` here are dropped from the JSON output thanks to
    /// `#[serde(skip_serializing_if = "Option::is_none")]`.
    pub fn redact_for_peer(&mut self) {
        self.cataloguing_notes = None;
        self.source_data = None;
        self.shelf_position = None;
        self.reading_status = None;
        self.finished_reading_at = None;
        self.started_reading_at = None;
        self.user_rating = None;
        self.price = None;
        self.private = None;
        // Internal sync state: peers have no business knowing our retry backlog.
        self.hub_cover_upload_failed_at = None;
    }

    /// Appends the canonical `?v={tag}` cache-buster to an already-built
    /// cover URL based on `updated_at`. Exposed for callers that received
    /// a hub URL from the directory service and need to match the
    /// versioning scheme of the rewrite functions without rebuilding from
    /// parts. No-op when `updated_at` is None or empty.
    pub fn append_cover_version_tag(url: String, updated_at: Option<&str>) -> String {
        append_version(url, updated_at)
    }

    /// Strict relay variant for a single book: errors when the source is
    /// a local path and no hub prefix is configured. `updated_at` (if any)
    /// adds a cache-busting `?v={tag}` suffix so peers refetch after a
    /// re-upload without waiting for their image cache to expire.
    pub fn safe_cover_url_strict(
        cover_url: Option<&str>,
        book_id: i32,
        updated_at: Option<&str>,
        hub_cover_prefix: Option<&str>,
    ) -> Result<Option<String>, CoverRewriteError> {
        match cover_url {
            None => Ok(None),
            Some(url) if url.starts_with("http") || url.starts_with("/api") => {
                Ok(Some(url.to_string()))
            }
            Some(_) => {
                let prefix = hub_cover_prefix.ok_or(CoverRewriteError {
                    book_ids: vec![book_id],
                })?;
                Ok(Some(build_hub_cover_url(prefix, book_id, updated_at)))
            }
        }
    }

    /// Relay-safe wrapper around `safe_cover_url_strict`. Logs ERROR and
    /// returns `None` if the source is an unservable local path and no
    /// hub prefix is available, so E2EE payloads never carry unreachable
    /// paths.
    pub fn safe_cover_url_for_relay(
        cover_url: Option<&str>,
        book_id: i32,
        updated_at: Option<&str>,
        hub_cover_prefix: Option<&str>,
    ) -> Option<String> {
        match Self::safe_cover_url_strict(cover_url, book_id, updated_at, hub_cover_prefix) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("relay safe_cover_url failed for book {book_id}: {e}");
                None
            }
        }
    }

    /// Computes a strong, order-independent hash of the local catalog state.
    ///
    /// Used as a "no-op canary" for sync flows: a peer that has the same
    /// catalog hash as ours can skip re-fetching the full book list. The
    /// hash is over `(book_id, updated_at)` pairs sorted by id, which
    /// covers any meaningful change (insert, delete, edit) as long as
    /// `updated_at` is bumped on edit (an invariant enforced by the API
    /// layer for create/update/delete handlers).
    ///
    /// Returns a 64-char lowercase hex SHA-256 string. On DB error the
    /// returned hash is the empty-catalog hash, which is safe: the peer
    /// will never match it for a non-empty catalog and we'll just fall
    /// back to a full sync.
    pub async fn compute_catalog_hash(db: &sea_orm::DatabaseConnection) -> String {
        use sea_orm::EntityTrait;
        use sha2::{Digest, Sha256};

        let books = super::book::Entity::find()
            .all(db)
            .await
            .unwrap_or_default();
        let mut pairs: Vec<(i32, String)> =
            books.into_iter().map(|b| (b.id, b.updated_at)).collect();
        pairs.sort_by_key(|(id, _)| *id);

        let mut hasher = Sha256::new();
        for (id, updated_at) in &pairs {
            hasher.update(format!("{id}:{updated_at}"));
        }
        hex::encode(hasher.finalize())
    }

    /// Builds the hub cover URL prefix (`{hub_url}/api/directory/{node_id}/covers`)
    /// from the current hub configuration.  Returns `None` when the hub is not
    /// configured or the node is not registered.
    pub async fn hub_cover_prefix(db: &sea_orm::DatabaseConnection) -> Option<String> {
        let hub_url = std::env::var("HUB_URL").ok()?;
        let hub_url = hub_url.trim_end_matches('/');
        let cfg = crate::services::hub_directory_service::HubDirectoryService::get_config(db)
            .await
            .ok()
            .flatten()?;
        Some(format!("{}/api/directory/{}/covers", hub_url, cfg.node_id))
    }

    /// Convert book models to DTOs with author names and available copy counts populated.
    pub async fn populate_authors(
        db: &sea_orm::DatabaseConnection,
        models: Vec<Model>,
    ) -> Vec<Book> {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        use std::collections::HashMap;

        // Batch-fetch copy info for all book IDs
        let book_ids: Vec<i32> = models.iter().map(|m| m.id).collect();
        let mut available_map: HashMap<i32, i32> = HashMap::new();
        let mut lent_set: std::collections::HashSet<i32> = std::collections::HashSet::new();
        let mut borrowed_set: std::collections::HashSet<i32> = std::collections::HashSet::new();
        if !book_ids.is_empty()
            && let Ok(copies) = super::copy::Entity::find()
                .filter(super::copy::Column::BookId.is_in(book_ids.clone()))
                .all(db)
                .await
        {
            for c in &copies {
                if c.status == "available" {
                    *available_map.entry(c.book_id).or_insert(0) += 1;
                }
                if c.status == "loaned" {
                    lent_set.insert(c.book_id);
                }
                if c.status == "borrowed" && c.is_temporary {
                    borrowed_set.insert(c.book_id);
                }
            }
        }

        let mut dtos = Vec::with_capacity(models.len());
        for model in models {
            let book_id = model.id;
            let mut dto = Book::from(model.clone());
            if let Ok(authors) = model.find_related(super::author::Entity).all(db).await
                && !authors.is_empty()
            {
                let names: Vec<String> = authors.into_iter().map(|a| a.name).collect();
                dto.author = Some(names.join(", "));
                dto.authors = Some(names);
            }
            dto.available_copies = Some(*available_map.get(&book_id).unwrap_or(&0));
            // Override reading_status based on copy status
            if borrowed_set.contains(&book_id) {
                dto.reading_status = Some("borrowed".to_string());
            } else if lent_set.contains(&book_id) {
                dto.reading_status = Some("lent".to_string());
            }
            dtos.push(dto);
        }
        dtos
    }
}

impl From<Book> for ActiveModel {
    fn from(book: Book) -> Self {
        Self {
            id: book.id.map_or(NotSet, Set),
            title: Set(book.title),
            isbn: Set(book.isbn),
            summary: Set(book.summary),
            publisher: Set(book.publisher),
            publication_year: Set(book.publication_year),
            dewey_decimal: Set(book.dewey_decimal),
            lcc: Set(book.lcc),
            subjects: Set(book
                .subjects
                .map(|s| serde_json::to_string(&s).unwrap_or_default())),
            marc_record: Set(book.marc_record),
            cataloguing_notes: Set(book.cataloguing_notes),
            source_data: Set(book.source_data),
            shelf_position: Set(book.shelf_position),
            cover_url: Set(book.cover_url),
            reading_status: book.reading_status.map_or(NotSet, Set),
            finished_reading_at: book.finished_reading_at.map_or(NotSet, Set),
            started_reading_at: book.started_reading_at.map_or(NotSet, Set),
            created_at: NotSet,
            updated_at: NotSet,
            user_rating: book.user_rating.map_or(NotSet, |r| Set(Some(r))),
            owned: book.owned.map_or(NotSet, Set),
            price: book.price.map_or(NotSet, |p| Set(Some(p))),
            digital_formats: book
                .digital_formats
                .map(|s| serde_json::to_string(&s).unwrap_or_default())
                .map_or(NotSet, |s| Set(Some(s))),
            private: book.private.map_or(NotSet, Set),
            page_count: book.page_count.map_or(NotSet, |p| Set(Some(p))),
            loan_duration_days: book.loan_duration_days.map_or(NotSet, |d| Set(Some(d))),
            // Owned solely by the hub-sync loop; leave NotSet on DTO round
            // trips so regular CRUD never clobbers a pending-failure flag.
            hub_cover_upload_failed_at: NotSet,
        }
    }
}

/// Signals that a cover URL rewrite intended for a relay-bound payload
/// could not produce a remotely reachable URL: one or more books carry a
/// local filesystem path while no hub prefix is configured. The caller
/// must decide whether to strip the offending fields or abort the payload.
#[derive(Debug, Clone)]
pub struct CoverRewriteError {
    pub book_ids: Vec<i32>,
}

impl std::fmt::Display for CoverRewriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cover rewrite requires a hub prefix but none is configured (book_ids: {:?})",
            self.book_ids
        )
    }
}

impl std::error::Error for CoverRewriteError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_book(id: Option<i32>, cover: Option<&str>) -> Book {
        Book {
            id,
            title: "t".into(),
            cover_url: cover.map(str::to_string),
            ..Default::default()
        }
    }

    fn mk_book_with_updated(id: Option<i32>, cover: Option<&str>, updated_at: &str) -> Book {
        Book {
            id,
            title: "t".into(),
            cover_url: cover.map(str::to_string),
            updated_at: Some(updated_at.into()),
            ..Default::default()
        }
    }

    #[test]
    fn strict_rewrites_local_paths_with_hub_prefix() {
        let mut books = vec![
            mk_book(Some(1), Some("/var/mobile/cover_1.jpg")),
            mk_book(Some(2), Some("/Users/x/cover_2.png")),
        ];
        Book::rewrite_cover_urls_strict(&mut books, Some("https://hub/api/directory/node/covers"))
            .expect("hub prefix provided");

        assert_eq!(
            books[0].cover_url.as_deref(),
            Some("https://hub/api/directory/node/covers/1")
        );
        assert_eq!(
            books[1].cover_url.as_deref(),
            Some("https://hub/api/directory/node/covers/2")
        );
    }

    #[test]
    fn strict_errors_on_local_paths_without_hub_prefix() {
        let mut books = vec![
            mk_book(Some(7), Some("/var/local_path.jpg")),
            mk_book(Some(9), Some("https://cdn.example/ok.jpg")),
            mk_book(Some(11), Some("/tmp/another.png")),
        ];
        let err = Book::rewrite_cover_urls_strict(&mut books, None).unwrap_err();

        // Only books with local paths appear in the error list.
        assert_eq!(err.book_ids, vec![7, 11]);
        // Books themselves are left unchanged when the call fails so the
        // caller retains full knowledge of what was missing.
        assert_eq!(books[0].cover_url.as_deref(), Some("/var/local_path.jpg"));
        assert_eq!(
            books[1].cover_url.as_deref(),
            Some("https://cdn.example/ok.jpg")
        );
    }

    #[test]
    fn strict_passthrough_for_http_and_api_urls() {
        let mut books = vec![
            mk_book(Some(1), Some("https://covers.openlibrary.org/x.jpg")),
            mk_book(Some(2), Some("/api/books/2/cover")),
        ];
        Book::rewrite_cover_urls_strict(&mut books, None).expect("no local paths => Ok");

        assert_eq!(
            books[0].cover_url.as_deref(),
            Some("https://covers.openlibrary.org/x.jpg")
        );
        assert_eq!(books[1].cover_url.as_deref(), Some("/api/books/2/cover"));
    }

    #[test]
    fn strict_ignores_books_without_id() {
        // A local-path book without an id can't be rewritten regardless; it
        // should not appear in the error set, and the call should succeed
        // when no other offender exists.
        let mut books = vec![mk_book(None, Some("/tmp/orphan.jpg"))];
        Book::rewrite_cover_urls_strict(&mut books, None).expect("no id => not an offender");
    }

    #[test]
    fn relay_wrapper_strips_offenders_to_none_on_failure() {
        let mut books = vec![
            mk_book(Some(1), Some("/var/local.jpg")),
            mk_book(Some(2), Some("https://cdn/ok.jpg")),
        ];
        Book::rewrite_cover_urls_for_relay(&mut books, None);

        assert_eq!(books[0].cover_url, None, "local path must be stripped");
        assert_eq!(
            books[1].cover_url.as_deref(),
            Some("https://cdn/ok.jpg"),
            "HTTP URL must be preserved"
        );
    }

    #[test]
    fn safe_strict_http_passthrough() {
        let out = Book::safe_cover_url_strict(Some("https://cdn/ok.jpg"), 1, None, None).unwrap();
        assert_eq!(out.as_deref(), Some("https://cdn/ok.jpg"));
    }

    #[test]
    fn safe_strict_local_without_prefix_errors() {
        let err =
            Book::safe_cover_url_strict(Some("/var/mobile/c.jpg"), 42, None, None).unwrap_err();
        assert_eq!(err.book_ids, vec![42]);
    }

    #[test]
    fn safe_strict_local_with_prefix_builds_hub_url() {
        let out = Book::safe_cover_url_strict(
            Some("/var/mobile/c.jpg"),
            42,
            None,
            Some("https://hub/api/directory/n/covers"),
        )
        .unwrap();
        assert_eq!(
            out.as_deref(),
            Some("https://hub/api/directory/n/covers/42")
        );
    }

    #[test]
    fn safe_strict_with_updated_at_appends_version() {
        let out = Book::safe_cover_url_strict(
            Some("/var/mobile/c.jpg"),
            42,
            Some("2026-04-20 10:30:00"),
            Some("https://hub/api/directory/n/covers"),
        )
        .unwrap();
        assert_eq!(
            out.as_deref(),
            Some("https://hub/api/directory/n/covers/42?v=20260420103000")
        );
    }

    #[test]
    fn safe_strict_none_in_none_out() {
        let out = Book::safe_cover_url_strict(None, 1, None, None).unwrap();
        assert_eq!(out, None);
    }

    #[test]
    fn safe_relay_wrapper_returns_none_on_failure() {
        let out = Book::safe_cover_url_for_relay(Some("/var/mobile/c.jpg"), 42, None, None);
        assert_eq!(out, None);
    }

    #[test]
    fn cover_version_tag_strips_non_alnum() {
        assert_eq!(cover_version_tag("2026-04-20 10:30:00"), "20260420103000");
        assert_eq!(
            cover_version_tag("2026-04-20T10:30:00Z"),
            "20260420T103000Z"
        );
        assert_eq!(cover_version_tag(""), "");
    }

    #[test]
    fn rewrite_versions_hub_urls_from_updated_at() {
        let mut books = vec![mk_book_with_updated(
            Some(7),
            Some("/var/mobile/c.jpg"),
            "2026-04-20 10:30:00",
        )];
        Book::rewrite_cover_urls_strict(&mut books, Some("https://hub/api/directory/n/covers"))
            .unwrap();

        assert_eq!(
            books[0].cover_url.as_deref(),
            Some("https://hub/api/directory/n/covers/7?v=20260420103000")
        );
    }

    #[test]
    fn rewrite_versions_lan_urls_from_updated_at() {
        let mut books = vec![mk_book_with_updated(
            Some(7),
            Some("/var/mobile/c.jpg"),
            "2026-04-20 10:30:00",
        )];
        Book::rewrite_local_cover_urls(&mut books, None);

        assert_eq!(
            books[0].cover_url.as_deref(),
            Some("/api/books/7/cover?v=20260420103000")
        );
    }

    #[test]
    fn rewrite_skips_version_when_updated_at_missing() {
        // Book with id but no updated_at (shouldn't happen in practice, but
        // the rewrite must stay safe — no dangling `?v=` suffix).
        let mut books = vec![mk_book(Some(7), Some("/var/mobile/c.jpg"))];
        Book::rewrite_cover_urls_strict(&mut books, Some("https://hub/covers")).unwrap();

        assert_eq!(books[0].cover_url.as_deref(), Some("https://hub/covers/7"));
    }

    /// Guardrail: every handler in `api/e2ee.rs` produces a JSON payload
    /// that may travel via hub relay to a peer with no direct HTTP route
    /// back to us. The LAN cover variants (`rewrite_local_cover_urls`,
    /// `safe_cover_url`) can emit `/api/books/{id}/cover` paths that are
    /// unreachable in that context. This test fails at compile time for
    /// any future caller that forgets to use the `_for_relay` variant.
    #[test]
    fn e2ee_handlers_never_use_lan_cover_rewriters() {
        let src = include_str!("../api/e2ee.rs");
        assert!(
            !src.contains("rewrite_local_cover_urls"),
            "api/e2ee.rs must use Book::rewrite_cover_urls_for_relay, not the LAN variant"
        );
        assert!(
            !src.contains("safe_cover_url("),
            "api/e2ee.rs must use Book::safe_cover_url_for_relay, not the LAN variant"
        );
    }
}
