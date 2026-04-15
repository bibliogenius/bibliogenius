use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "peer_books")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub peer_id: i32,
    pub remote_book_id: i32,
    pub title: String,
    pub isbn: Option<String>,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub summary: Option<String>,
    pub synced_at: String,
    pub node_id: Option<String>,
    pub first_seen_at: Option<String>,
    pub notified_at: Option<String>,
    /// `books.created_at` propagated from the owner peer (ADR pending).
    /// Replaces `first_seen_at` for the "new" badge: source of truth is
    /// the owner's library, not the local cache observation time.
    pub added_at: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::peer::Entity",
        from = "Column::PeerId",
        to = "super::peer::Column::Id",
        on_update = "Cascade",
        on_delete = "Cascade"
    )]
    Peer,
}

impl Related<super::peer::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Peer.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// Convert a cached peer book row into the Book DTO returned by the API.
/// `id` becomes `remote_book_id` so cached and live responses share the same
/// id space (the peer's book id), and `added_at` is propagated for the
/// "new" badge.
impl From<Model> for super::Book {
    fn from(pb: Model) -> Self {
        super::Book {
            id: Some(pb.remote_book_id),
            title: pb.title,
            isbn: pb.isbn,
            summary: pb.summary,
            publisher: None,
            publication_year: None,
            dewey_decimal: None,
            lcc: None,
            subjects: None,
            marc_record: None,
            cataloguing_notes: None,
            source_data: None,
            shelf_position: None,
            reading_status: None,
            finished_reading_at: None,
            started_reading_at: None,
            source: None,
            author: pb.author,
            authors: None,
            cover_url: pb.cover_url,
            large_cover_url: None,
            user_rating: None,
            owned: None,
            price: None,
            language: None,
            digital_formats: None,
            available_copies: None,
            private: None,
            page_count: None,
            loan_duration_days: None,
            added_at: pb.added_at,
        }
    }
}
