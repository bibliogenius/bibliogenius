use sea_orm::entity::prelude::*;
use sea_orm::{ModelTrait, NotSet, Set};
use serde::{Deserialize, Serialize};

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
    /// When the local cache first observed this book (peer libraries only).
    /// Set by the cache layer for "new" badge support; absent for the
    /// owner's own books and for live network responses with no cache row.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub first_seen_at: Option<String>,
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
            first_seen_at: None,
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
    pub fn rewrite_local_cover_urls(books: &mut [Book], hub_cover_prefix: Option<&str>) {
        for book in books {
            if let Some(ref url) = book.cover_url
                && !url.starts_with("http")
                && !url.starts_with("/api")
            {
                book.cover_url = book.id.map(|id| {
                    if let Some(prefix) = hub_cover_prefix {
                        format!("{}/{}", prefix, id)
                    } else {
                        format!("/api/books/{}/cover", id)
                    }
                });
            }
        }
    }

    /// Rewrites a single cover_url from a SeaORM entity model.
    /// Same logic as `rewrite_local_cover_urls` but for individual books.
    pub fn safe_cover_url(
        cover_url: Option<&str>,
        book_id: i32,
        hub_cover_prefix: Option<&str>,
    ) -> Option<String> {
        match cover_url {
            Some(url) if url.starts_with("http") || url.starts_with("/api") => {
                Some(url.to_string())
            }
            Some(_) => Some(if let Some(prefix) = hub_cover_prefix {
                format!("{}/{}", prefix, book_id)
            } else {
                format!("/api/books/{}/cover", book_id)
            }),
            None => None,
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
        }
    }
}
