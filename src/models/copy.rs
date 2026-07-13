use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "copies")]
pub struct Model {
    /// Stable cross-device primary key (UUID v7); stored in the `uuid` column
    /// (ADR-044 Addendum A). Minted by `before_save` when not provided.
    #[sea_orm(primary_key, auto_increment = false, column_name = "uuid")]
    pub id: String,
    pub book_id: String,
    pub library_id: i32,
    pub acquisition_date: Option<String>,
    pub notes: Option<String>,
    /// Availability status of this physical copy.
    /// Valid values:
    /// - `available`: On shelf, can be loaned
    /// - `loaned`: Currently lent to someone (has active Loan)
    /// - `borrowed`: Borrowed from another library (P2P)
    /// - `lost`: Copy is lost
    /// - `wanted`: Wishlist - don't own yet
    /// - `sold`: Already sold (bookseller module)
    pub status: String,
    pub is_temporary: bool,
    pub created_at: String,
    pub updated_at: String,
    /// Detailed sale date if status is 'sold'
    pub sold_at: Option<String>,
    /// Price of this specific copy (EUR). Used by bookseller profile.
    /// If set, this overrides the book's default price.
    /// If NULL, the price from the parent book is used.
    pub price: Option<f64>,
    /// Display name of the lender (peer library name or contact name).
    /// Populated when `status = 'borrowed'`. NULL for non-borrowed copies
    /// and for legacy borrowed copies created before ADR-034 (see backfill
    /// in `infrastructure/db.rs`).
    pub lender_display_name: Option<String>,
    /// FK to `peers.id` when the copy was borrowed from a P2P peer.
    /// NULL for contact loans or legacy rows.
    pub lender_peer_id: Option<i32>,
    /// ISO 8601 due date for a borrowed copy (string to match the rest of
    /// the date columns in this schema). NULL for non-borrowed copies.
    pub borrow_due_date: Option<String>,
    /// Source of the borrow: `"peer"` (P2P loan) or `"contact"` (local
    /// contact loan). Stored as TEXT; see `BorrowSource` for the typed
    /// representation used across the code. NULL for non-borrowed copies.
    pub borrow_source: Option<String>,
    /// The lender's stable library identifier (`peers.library_uuid`), carried on
    /// the copy so a SECOND synced device — which replicates `copies` but not
    /// `peers` — can still resolve the lender when the book is returned (ADR-049,
    /// migration 089). NULL for owned/contact copies, and for peer loans whose
    /// lender never announced a uuid.
    pub lender_library_uuid: Option<String>,
    /// The loan's id at the lender (`p2p_outgoing_request.lender_request_id`),
    /// copied here at borrow time so the return notification survives on a device
    /// that never held the outgoing request (ADR-049). NULL for non-peer copies.
    pub lender_request_id: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::book::Entity",
        from = "Column::BookId",
        to = "super::book::Column::Id"
    )]
    Book,
    #[sea_orm(
        belongs_to = "super::library::Entity",
        from = "Column::LibraryId",
        to = "super::library::Column::Id"
    )]
    Library,
}

impl Related<super::book::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Book.def()
    }
}

impl Related<super::library::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Library.def()
    }
}

#[async_trait::async_trait]
impl ActiveModelBehavior for ActiveModel {
    async fn before_save<C>(mut self, _db: &C, insert: bool) -> Result<Self, DbErr>
    where
        C: ConnectionTrait,
    {
        if insert && self.id.is_not_set() {
            self.id = Set(crate::utils::uuid_gen::new_uuid_v7());
        }
        Ok(self)
    }
}
