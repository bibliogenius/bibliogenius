// Reading recommendations (ADR-059): similar books + personal suggestions.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ── Reading recommendations ──────────────────────────────────────────

/// One reason a book was recommended, as a stable wire pair. `reason_type`
/// maps to an i18n key on the Flutter side ("same_author" ->
/// "reason_same_author"), `value` is the display payload ("Albert Camus").
#[flutter_rust_bridge::frb]
pub struct FrbRecommendationReason {
    pub reason_type: String,
    pub value: String,
}

/// One recommendation: the book, its score, and the human-readable reasons
/// (strongest first). Explainability is the trust contract of the feature:
/// the UI must always show at least the first reason.
#[flutter_rust_bridge::frb]
pub struct FrbRecommendation {
    pub book: FrbBook,
    pub score: f64,
    pub reasons: Vec<FrbRecommendationReason>,
}

/// Dashboard response: suggestions plus the taste-profile summary that
/// produced them. `scored_books_count` gates the section (hidden below 5).
#[flutter_rust_bridge::frb]
pub struct FrbRecommendationResponse {
    pub recommendations: Vec<FrbRecommendation>,
    pub top_subjects: Vec<String>,
    pub favorite_authors: Vec<String>,
    pub scored_books_count: u32,
}

impl From<crate::domain::recommendations::ScoredRecommendation> for FrbRecommendation {
    fn from(rec: crate::domain::recommendations::ScoredRecommendation) -> Self {
        Self {
            book: FrbBook::from(rec.book),
            score: rec.score,
            reasons: rec
                .reasons
                .iter()
                .map(|r| FrbRecommendationReason {
                    reason_type: r.type_key().to_string(),
                    value: r.value(),
                })
                .collect(),
        }
    }
}

/// "You might also like": books from the local library similar to the given
/// one. Computed on demand; the Flutter side caches (Rule F4).
#[flutter_rust_bridge::frb]
pub async fn get_book_recommendations(
    book_id: String,
    limit: Option<u32>,
) -> Result<Vec<FrbRecommendation>, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::recommendation_service::similar_to(db, &book_id, limit.map(|l| l as usize))
        .await
        .map(|v| v.into_iter().map(FrbRecommendation::from).collect())
        .map_err(|e| format!("{e:?}"))
}

/// "Suggestions for you": unread books scored against the taste profile
/// built from the user's read, rated and favorite books.
#[flutter_rust_bridge::frb]
pub async fn get_personal_recommendations(
    limit: Option<u32>,
) -> Result<FrbRecommendationResponse, String> {
    let db = db().ok_or("Database not initialized")?;
    let (profile, recs) = crate::services::recommendation_service::suggestions_for_user(
        db,
        limit.map(|l| l as usize),
    )
    .await
    .map_err(|e| format!("{e:?}"))?;
    Ok(FrbRecommendationResponse {
        recommendations: recs.into_iter().map(FrbRecommendation::from).collect(),
        top_subjects: profile.top_subjects.into_iter().map(|(s, _)| s).collect(),
        favorite_authors: profile
            .favorite_authors
            .into_iter()
            .map(|(a, _)| a)
            .collect(),
        scored_books_count: profile.scored_books_count,
    })
}
