//! SeaORM implementation of SlidingPuzzleRepository

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, Set,
};

use super::domain::{
    DomainError, PeerPuzzleScoreRow, PuzzleBook, PuzzleScore, SlidingPuzzleRepository,
};
use super::models::peer_puzzle_score::{
    ActiveModel as PeerScoreActiveModel, Column as PeerScoreColumn, Entity as PeerScoreEntity,
};
use super::models::sliding_puzzle_score::{
    ActiveModel as ScoreActiveModel, Column as ScoreColumn, Entity as ScoreEntity,
};
use crate::models::book::{Column as BookColumn, Entity as BookEntity};

/// SeaORM-based implementation of SlidingPuzzleRepository
pub struct SeaOrmPuzzleRepository {
    db: DatabaseConnection,
}

impl SeaOrmPuzzleRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl SlidingPuzzleRepository for SeaOrmPuzzleRepository {
    async fn find_books_with_covers(&self) -> Result<Vec<PuzzleBook>, DomainError> {
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
            .map(|b| PuzzleBook {
                book_id: b.id,
                title: b.title,
                cover_url: b.cover_url.unwrap_or_default(),
            })
            .collect())
    }

    async fn save_score(&self, score: PuzzleScore) -> Result<PuzzleScore, DomainError> {
        let model = ScoreActiveModel {
            difficulty: Set(score.difficulty),
            grid_size: Set(score.grid_size),
            elapsed_seconds: Set(score.elapsed_seconds),
            move_count: Set(score.move_count),
            par_moves: Set(score.par_moves),
            normalized_score: Set(score.normalized_score),
            played_at: Set(score.played_at),
            ..Default::default()
        };

        let result = model.insert(&self.db).await?;

        Ok(PuzzleScore {
            id: Some(result.id),
            difficulty: result.difficulty,
            grid_size: result.grid_size,
            elapsed_seconds: result.elapsed_seconds,
            move_count: result.move_count,
            par_moves: result.par_moves,
            normalized_score: result.normalized_score,
            played_at: result.played_at,
        })
    }

    async fn get_top_scores(&self, limit: u32) -> Result<Vec<PuzzleScore>, DomainError> {
        let scores = ScoreEntity::find()
            .order_by_desc(ScoreColumn::NormalizedScore)
            .limit(Some(limit as u64))
            .all(&self.db)
            .await?;

        Ok(scores
            .into_iter()
            .map(|s| PuzzleScore {
                id: Some(s.id),
                difficulty: s.difficulty,
                grid_size: s.grid_size,
                elapsed_seconds: s.elapsed_seconds,
                move_count: s.move_count,
                par_moves: s.par_moves,
                normalized_score: s.normalized_score,
                played_at: s.played_at,
            })
            .collect())
    }

    async fn get_personal_best(&self) -> Result<Option<f64>, DomainError> {
        let score = ScoreEntity::find()
            .order_by_desc(ScoreColumn::NormalizedScore)
            .limit(Some(1))
            .one(&self.db)
            .await?;

        Ok(score.map(|s| s.normalized_score))
    }

    async fn get_best_score_entry(&self) -> Result<Option<PuzzleScore>, DomainError> {
        let score = ScoreEntity::find()
            .order_by_desc(ScoreColumn::NormalizedScore)
            .limit(Some(1))
            .one(&self.db)
            .await?;

        Ok(score.map(|s| PuzzleScore {
            id: Some(s.id),
            difficulty: s.difficulty,
            grid_size: s.grid_size,
            elapsed_seconds: s.elapsed_seconds,
            move_count: s.move_count,
            par_moves: s.par_moves,
            normalized_score: s.normalized_score,
            played_at: s.played_at,
        }))
    }

    async fn get_best_score_entries_per_difficulty(&self) -> Result<Vec<PuzzleScore>, DomainError> {
        use sea_orm::{ConnectionTrait, Statement};

        let stmt = Statement::from_string(
            self.db.get_database_backend(),
            r#"
            SELECT id, difficulty, grid_size, elapsed_seconds, move_count,
                   par_moves, normalized_score, played_at
            FROM sliding_puzzle_scores
            WHERE id IN (
                SELECT id FROM sliding_puzzle_scores sp1
                WHERE normalized_score = (
                    SELECT MAX(normalized_score) FROM sliding_puzzle_scores sp2
                    WHERE sp2.difficulty = sp1.difficulty
                )
                GROUP BY difficulty
            )
            "#
            .to_owned(),
        );

        let rows = self.db.query_all(stmt).await?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(PuzzleScore {
                id: Some(row.try_get("", "id")?),
                difficulty: row.try_get("", "difficulty")?,
                grid_size: row.try_get("", "grid_size")?,
                elapsed_seconds: row.try_get("", "elapsed_seconds")?,
                move_count: row.try_get("", "move_count")?,
                par_moves: row.try_get("", "par_moves")?,
                normalized_score: row.try_get("", "normalized_score")?,
                played_at: row.try_get("", "played_at")?,
            });
        }
        Ok(entries)
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

        // Key by (peer_id, difficulty) — see hangman::upsert_peer_score.
        let existing = PeerScoreEntity::find()
            .filter(PeerScoreColumn::PeerId.eq(peer_id))
            .filter(PeerScoreColumn::Difficulty.eq(difficulty))
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

    async fn get_peer_scores(&self) -> Result<Vec<PeerPuzzleScoreRow>, DomainError> {
        let scores = PeerScoreEntity::find()
            .order_by_desc(PeerScoreColumn::BestScore)
            .all(&self.db)
            .await?;

        Ok(scores
            .into_iter()
            .map(|s| PeerPuzzleScoreRow {
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
