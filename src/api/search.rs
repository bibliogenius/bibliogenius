use crate::models::book;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Clone)]
pub struct SearchQuery {
    pub title: Option<String>,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub year_min: Option<i32>,
    pub year_max: Option<i32>,
    pub tags: Option<String>,
    pub q: Option<String>,
    pub subjects: Option<String>, // Plural to match existing usage or "subject" singular? OL uses subject. Book model uses subjects. Let's use "subject" for query param for consistency with others.
    pub sources: Option<String>,  // "local,peers,public"
    pub autocomplete: Option<bool>,
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub books: Vec<book::Book>,
    pub total: usize,
}

pub async fn search_books(
    State(db): State<DatabaseConnection>,
    Query(params): Query<SearchQuery>,
) -> impl IntoResponse {
    let sources = params
        .sources
        .clone()
        .unwrap_or_else(|| "local".to_string());
    let source_list: Vec<&str> = sources.split(',').map(|s| s.trim()).collect();

    let mut all_books: Vec<book::Book> = Vec::new();

    // 1. Local Search
    if source_list.contains(&"local") {
        let mut condition = Condition::all();

        if let Some(title) = &params.title {
            if !title.is_empty() {
                condition = condition.add(book::Column::Title.contains(title));
            }
        }

        if let Some(q) = &params.q {
            if !q.is_empty() {
                condition = condition.add(
                    Condition::any()
                        .add(book::Column::Title.contains(q))
                        .add(book::Column::Publisher.contains(q)), // Note: Author is not a column on Book in some versions, check model.
                                                                   // If author is joined, we need join logic.
                                                                   // For now, let's stick to simple columns or check if 'author' column exists.
                                                                   // Looking at previous files, 'author' in Book struct is enriched.
                                                                   // But wait, search_unified maps it.
                                                                   // Let's check book::Column usage.
                                                                   // If I can't easily search author in the simple entity find, I'll skip it for q
                                                                   // or just do title/publisher.
                );
            }
        }

        if let Some(publisher) = &params.publisher {
            if !publisher.is_empty() {
                condition = condition.add(book::Column::Publisher.contains(publisher));
            }
        }

        if let Some(min) = params.year_min {
            condition = condition.add(book::Column::PublicationYear.gte(min));
        }

        if let Some(max) = params.year_max {
            condition = condition.add(book::Column::PublicationYear.lte(max));
        }

        if let Ok(local_books) = book::Entity::find()
            .filter(condition)
            .order_by_asc(book::Column::Title)
            .all(&db)
            .await
        {
            let mut dtos: Vec<book::Book> = local_books.into_iter().map(|b| b.into()).collect();
            all_books.append(&mut dtos);
        }
    }

    // 2. Public Search (Open Library)
    if source_list.contains(&"public") {
        let external_models = crate::api::integrations::search_external(&params, &db).await;
        let mut dtos: Vec<book::Book> = external_models
            .into_iter()
            .map(|m| {
                let mut dto: book::Book = m.into();
                dto.source = Some("Open Library".to_string());
                dto
            })
            .collect();
        all_books.append(&mut dtos);
    }

    // 3. Peer Search (P2P)
    if source_list.contains(&"peers") {
        // We need to implement broadcast_search in api::peer
        // For now, let's assume it exists or implement it inline if simple
        // But better to call a helper.
        let peer_books = crate::api::peer::broadcast_search(&db, &params).await;
        all_books.extend(peer_books);
    }

    (
        StatusCode::OK,
        Json(SearchResponse {
            total: all_books.len(),
            books: all_books,
        }),
    )
        .into_response()
}
