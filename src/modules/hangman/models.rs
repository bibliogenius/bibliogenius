//! SeaORM entities for hangman tables

pub mod hangman_score {
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "hangman_scores")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub book_id: i32,
        pub difficulty: String,
        #[sea_orm(column_type = "Double")]
        pub elapsed_seconds: f64,
        pub errors: i32,
        pub hints_used: i32,
        pub won: i32,
        #[sea_orm(column_type = "Double")]
        pub normalized_score: f64,
        pub played_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod peer_hangman_score {
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "peer_hangman_scores")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub peer_id: i32,
        pub library_name: String,
        #[sea_orm(column_type = "Double")]
        pub best_score: f64,
        pub difficulty: String,
        pub played_at: String,
        pub synced_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "crate::models::peer::Entity",
            from = "Column::PeerId",
            to = "crate::models::peer::Column::Id",
            on_update = "Cascade",
            on_delete = "Cascade"
        )]
        Peer,
    }

    impl Related<crate::models::peer::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Peer.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}
