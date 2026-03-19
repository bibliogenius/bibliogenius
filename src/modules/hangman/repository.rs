//! SeaORM implementation of HangmanRepository

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, Set,
};

use super::domain::{
    DomainError, HangmanBook, HangmanRepository, HangmanScore, PeerHangmanScoreRow,
};
use super::models::hangman_score::{ActiveModel as ScoreActiveModel, Entity as ScoreEntity};
use super::models::peer_hangman_score::{
    ActiveModel as PeerScoreActiveModel, Column as PeerScoreColumn, Entity as PeerScoreEntity,
};
use crate::models::author::Entity as AuthorEntity;
use crate::models::book::Entity as BookEntity;

/// SeaORM-based implementation of HangmanRepository
pub struct SeaOrmHangmanRepository {
    db: DatabaseConnection,
}

impl SeaOrmHangmanRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl HangmanRepository for SeaOrmHangmanRepository {
    async fn find_eligible_books(&self) -> Result<Vec<HangmanBook>, DomainError> {
        use sea_orm::RelationTrait;

        // Fetch books with their authors via join
        let books = BookEntity::find().all(&self.db).await?;

        let mut result = Vec::with_capacity(books.len());
        for book in books {
            // Fetch authors for this book
            let authors = AuthorEntity::find()
                .join(
                    sea_orm::JoinType::InnerJoin,
                    crate::models::book_authors::Relation::Author.def().rev(),
                )
                .filter(crate::models::book_authors::Column::BookId.eq(book.id))
                .all(&self.db)
                .await
                .unwrap_or_default();

            let author = authors.first().map(|a| a.name.clone()).unwrap_or_default();

            result.push(HangmanBook {
                book_id: book.id,
                title: book.title,
                author,
                cover_url: book.cover_url,
            });
        }

        Ok(result)
    }

    async fn get_recent_book_ids(&self, limit: u32) -> Result<Vec<i32>, DomainError> {
        let scores = ScoreEntity::find()
            .order_by_desc(super::models::hangman_score::Column::Id)
            .limit(Some(limit as u64))
            .all(&self.db)
            .await?;

        Ok(scores.into_iter().map(|s| s.book_id).collect())
    }

    async fn save_score(&self, score: HangmanScore) -> Result<HangmanScore, DomainError> {
        let model = ScoreActiveModel {
            book_id: Set(score.book_id),
            difficulty: Set(score.difficulty),
            elapsed_seconds: Set(score.elapsed_seconds),
            errors: Set(score.errors),
            hints_used: Set(score.hints_used),
            won: Set(if score.won { 1 } else { 0 }),
            normalized_score: Set(score.normalized_score),
            played_at: Set(score.played_at),
            ..Default::default()
        };

        let result = model.insert(&self.db).await?;

        Ok(HangmanScore {
            id: Some(result.id),
            book_id: result.book_id,
            difficulty: result.difficulty,
            elapsed_seconds: result.elapsed_seconds,
            errors: result.errors,
            hints_used: result.hints_used,
            won: result.won != 0,
            normalized_score: result.normalized_score,
            played_at: result.played_at,
        })
    }

    async fn get_top_scores(&self, limit: u32) -> Result<Vec<HangmanScore>, DomainError> {
        let scores = ScoreEntity::find()
            .order_by_desc(super::models::hangman_score::Column::NormalizedScore)
            .limit(Some(limit as u64))
            .all(&self.db)
            .await?;

        Ok(scores
            .into_iter()
            .map(|s| HangmanScore {
                id: Some(s.id),
                book_id: s.book_id,
                difficulty: s.difficulty,
                elapsed_seconds: s.elapsed_seconds,
                errors: s.errors,
                hints_used: s.hints_used,
                won: s.won != 0,
                normalized_score: s.normalized_score,
                played_at: s.played_at,
            })
            .collect())
    }

    async fn get_personal_best(&self) -> Result<Option<f64>, DomainError> {
        let score = ScoreEntity::find()
            .order_by_desc(super::models::hangman_score::Column::NormalizedScore)
            .limit(Some(1))
            .one(&self.db)
            .await?;

        Ok(score.map(|s| s.normalized_score))
    }

    async fn get_best_score_entry(&self) -> Result<Option<HangmanScore>, DomainError> {
        let score = ScoreEntity::find()
            .order_by_desc(super::models::hangman_score::Column::NormalizedScore)
            .limit(Some(1))
            .one(&self.db)
            .await?;

        Ok(score.map(|s| HangmanScore {
            id: Some(s.id),
            book_id: s.book_id,
            difficulty: s.difficulty,
            elapsed_seconds: s.elapsed_seconds,
            errors: s.errors,
            hints_used: s.hints_used,
            won: s.won != 0,
            normalized_score: s.normalized_score,
            played_at: s.played_at,
        }))
    }

    async fn delete_peer_scores(&self, peer_id: i32) -> Result<(), DomainError> {
        PeerScoreEntity::delete_many()
            .filter(PeerScoreColumn::PeerId.eq(peer_id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    async fn upsert_peer_score(
        &self,
        peer_id: i32,
        library_name: &str,
        best_score: f64,
        difficulty: &str,
        played_at: &str,
    ) -> Result<(), DomainError> {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

        let existing = PeerScoreEntity::find()
            .filter(PeerScoreColumn::PeerId.eq(peer_id))
            .one(&self.db)
            .await?;

        if let Some(existing) = existing {
            if best_score > existing.best_score {
                let mut active: PeerScoreActiveModel = existing.into();
                active.library_name = Set(library_name.to_string());
                active.best_score = Set(best_score);
                active.difficulty = Set(difficulty.to_string());
                active.played_at = Set(played_at.to_string());
                active.synced_at = Set(now);
                active.update(&self.db).await?;
            }
        } else {
            let model = PeerScoreActiveModel {
                peer_id: Set(peer_id),
                library_name: Set(library_name.to_string()),
                best_score: Set(best_score),
                difficulty: Set(difficulty.to_string()),
                played_at: Set(played_at.to_string()),
                synced_at: Set(now),
                ..Default::default()
            };
            model.insert(&self.db).await?;
        }

        Ok(())
    }

    async fn get_peer_scores(&self) -> Result<Vec<PeerHangmanScoreRow>, DomainError> {
        let scores = PeerScoreEntity::find()
            .order_by_desc(PeerScoreColumn::BestScore)
            .all(&self.db)
            .await?;

        Ok(scores
            .into_iter()
            .map(|s| PeerHangmanScoreRow {
                peer_id: s.peer_id,
                library_name: s.library_name,
                best_score: s.best_score,
                difficulty: s.difficulty,
                played_at: s.played_at,
            })
            .collect())
    }
}
