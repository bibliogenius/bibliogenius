use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter};
use serde::Serialize;

use crate::models::{loan, operation_log};

#[derive(Serialize)]
pub struct UserStatus {
    pub level: String,
    pub loans_count: u64,
    pub edits_count: u64,
    pub next_level_progress: f32,
    pub badge_url: String, // Placeholder for frontend asset
}

pub async fn get_user_status(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    // 1. Calculate Loans (Lender Track)
    // Count loans where the library belongs to the user (owner_id).
    // Since we don't have auth context yet (single user mode mostly), we assume "My Library" (id=1) or all loans where library owner is "me".
    // For this phase, we'll count ALL loans in the system as "My Loans" since it's a single-user node.
    // In a multi-user node, we'd filter by `library.owner_id`.
    let loans_count = loan::Entity::find().count(&db).await.unwrap_or(0);

    // 2. Calculate Edits (Archivist Track)
    // Count operation_log entries for 'book'
    let edits_count = operation_log::Entity::find()
        .filter(operation_log::Column::EntityType.eq("book"))
        .count(&db)
        .await
        .unwrap_or(0);

    // 3. Determine Level
    // Level 1: Member (Default)
    // Level 2: BiblioGenius (5 Loans OR 10 Edits)
    // Level 3: Pro (50 Loans OR 100 Edits)

    let (level, progress, badge) = if loans_count >= 50 || edits_count >= 100 {
        ("Pro", 1.0, "assets/badges/pro.png")
    } else if loans_count >= 5 || edits_count >= 10 {
        // Calculate progress to Pro
        let loan_prog = loans_count as f32 / 50.0;
        let edit_prog = edits_count as f32 / 100.0;
        let p = loan_prog.max(edit_prog);
        ("BiblioGenius", p, "assets/badges/bibliogenius.png")
    } else {
        // Calculate progress to BiblioGenius
        let loan_prog = loans_count as f32 / 5.0;
        let edit_prog = edits_count as f32 / 10.0;
        let p = loan_prog.max(edit_prog);
        ("Member", p, "assets/badges/member.png")
    };

    let status = UserStatus {
        level: level.to_string(),
        loans_count,
        edits_count,
        next_level_progress: progress,
        badge_url: badge.to_string(),
    };

    (StatusCode::OK, Json(status)).into_response()
}
