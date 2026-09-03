//! Linked author names per book, shared by the duplicate merge (ADR-070) and by
//! the reimport completion (ADR-071).
//!
//! Both features correlate books through `book_dedup_key`, whose author
//! component is a single name. They need the same two queries and the same
//! grouping, so it lives here rather than twice.

use std::collections::HashMap;

use sea_orm::{DatabaseConnection, EntityTrait};

use crate::domain::DomainError;
use crate::models::{author, book_authors};

fn db_err<E: std::fmt::Display>(e: E) -> DomainError {
    DomainError::Database(e.to_string())
}

/// Every linked author name, per book uuid, sorted alphabetically.
///
/// Sorted so callers get a deterministic order: the merge takes the first name
/// as the primary author, and the reimport index joins them in this order to
/// rebuild the form an export writes.
pub async fn author_names_by_book(
    db: &DatabaseConnection,
) -> Result<HashMap<String, Vec<String>>, DomainError> {
    let names: HashMap<String, String> = author::Entity::find()
        .all(db)
        .await
        .map_err(db_err)?
        .into_iter()
        .map(|a| (a.id, a.name))
        .collect();

    let mut by_book: HashMap<String, Vec<String>> = HashMap::new();
    for link in book_authors::Entity::find().all(db).await.map_err(db_err)? {
        let Some(name) = names.get(&link.author_id) else {
            continue;
        };
        by_book.entry(link.book_id).or_default().push(name.clone());
    }
    for names in by_book.values_mut() {
        names.sort();
    }
    Ok(by_book)
}

/// The author each book contributes to its dedup key: the alphabetically
/// smallest linked name. Books with no author are absent from the map.
pub async fn primary_authors(
    db: &DatabaseConnection,
) -> Result<HashMap<String, String>, DomainError> {
    Ok(author_names_by_book(db)
        .await?
        .into_iter()
        .filter_map(|(book_id, mut names)| (!names.is_empty()).then(|| (book_id, names.remove(0))))
        .collect())
}
