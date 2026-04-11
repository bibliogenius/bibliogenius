//! SeaORM implementation of MemoryGameRepository

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, Set,
};

use super::domain::{
    DomainError, MemoryGameCard, MemoryGameRepository, MemoryGameScore, PeerMemoryScoreRow,
};
use super::models::memory_game_score::{ActiveModel as ScoreActiveModel, Entity as ScoreEntity};
use super::models::peer_memory_score::{
    ActiveModel as PeerScoreActiveModel, Column as PeerScoreColumn, Entity as PeerScoreEntity,
};
use crate::models::book::{Column as BookColumn, Entity as BookEntity};

/// SeaORM-based implementation of MemoryGameRepository
pub struct SeaOrmGameRepository {
    db: DatabaseConnection,
}

impl SeaOrmGameRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl MemoryGameRepository for SeaOrmGameRepository {
    async fn find_books_with_covers(&self) -> Result<Vec<MemoryGameCard>, DomainError> {
        let books = BookEntity::find()
            .filter(BookColumn::CoverUrl.is_not_null())
            .filter(BookColumn::CoverUrl.ne(""))
            .all(&self.db)
            .await?;

        Ok(books
            .into_iter()
            .filter(|b| {
                // Only keep books with real cover images (network URLs or local files)
                // Exclude empty strings, /api/ relative paths, and other non-image values
                b.cover_url
                    .as_ref()
                    .map(|url| url.starts_with("http") || url.starts_with("/"))
                    .unwrap_or(false)
            })
            .map(|b| MemoryGameCard {
                book_id: b.id,
                title: b.title,
                cover_url: b.cover_url.unwrap_or_default(),
            })
            .collect())
    }

    async fn save_score(&self, score: MemoryGameScore) -> Result<MemoryGameScore, DomainError> {
        let model = ScoreActiveModel {
            difficulty: Set(score.difficulty),
            pairs_count: Set(score.pairs_count),
            elapsed_seconds: Set(score.elapsed_seconds),
            errors: Set(score.errors),
            normalized_score: Set(score.normalized_score),
            played_at: Set(score.played_at),
            ..Default::default()
        };

        let result = model.insert(&self.db).await?;

        Ok(MemoryGameScore {
            id: Some(result.id),
            difficulty: result.difficulty,
            pairs_count: result.pairs_count,
            elapsed_seconds: result.elapsed_seconds,
            errors: result.errors,
            normalized_score: result.normalized_score,
            played_at: result.played_at,
        })
    }

    async fn get_top_scores(&self, limit: u32) -> Result<Vec<MemoryGameScore>, DomainError> {
        let scores = ScoreEntity::find()
            .order_by_desc(super::models::memory_game_score::Column::NormalizedScore)
            .limit(Some(limit as u64))
            .all(&self.db)
            .await?;

        Ok(scores
            .into_iter()
            .map(|s| MemoryGameScore {
                id: Some(s.id),
                difficulty: s.difficulty,
                pairs_count: s.pairs_count,
                elapsed_seconds: s.elapsed_seconds,
                errors: s.errors,
                normalized_score: s.normalized_score,
                played_at: s.played_at,
            })
            .collect())
    }

    async fn get_personal_best(&self) -> Result<Option<f64>, DomainError> {
        let score = ScoreEntity::find()
            .order_by_desc(super::models::memory_game_score::Column::NormalizedScore)
            .limit(Some(1))
            .one(&self.db)
            .await?;

        Ok(score.map(|s| s.normalized_score))
    }

    async fn get_best_score_entry(&self) -> Result<Option<MemoryGameScore>, DomainError> {
        let score = ScoreEntity::find()
            .order_by_desc(super::models::memory_game_score::Column::NormalizedScore)
            .limit(Some(1))
            .one(&self.db)
            .await?;

        Ok(score.map(|s| MemoryGameScore {
            id: Some(s.id),
            difficulty: s.difficulty,
            pairs_count: s.pairs_count,
            elapsed_seconds: s.elapsed_seconds,
            errors: s.errors,
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
            let score_improved = best_score > existing.best_score;
            let name_changed = existing.library_name != library_name;
            if score_improved || name_changed {
                let mut active: PeerScoreActiveModel = existing.into();
                active.library_name = Set(library_name.to_string());
                if score_improved {
                    active.best_score = Set(best_score);
                    active.difficulty = Set(difficulty.to_string());
                    active.played_at = Set(played_at.to_string());
                }
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

    async fn get_peer_scores(&self) -> Result<Vec<PeerMemoryScoreRow>, DomainError> {
        let scores = PeerScoreEntity::find()
            .order_by_desc(PeerScoreColumn::BestScore)
            .all(&self.db)
            .await?;

        Ok(scores
            .into_iter()
            .map(|s| PeerMemoryScoreRow {
                peer_id: s.peer_id,
                library_name: s.library_name,
                best_score: s.best_score,
                difficulty: s.difficulty,
                played_at: s.played_at,
            })
            .collect())
    }

    async fn delete_all_scores(&self) -> Result<(), DomainError> {
        ScoreEntity::delete_many().exec(&self.db).await?;
        Ok(())
    }
}
