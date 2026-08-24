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

// ── External discovery lookup inputs (ADR-060) ───────────────────────

/// One "complete the series" lookup for the hub discovery resolver: the
/// anchors identify the series, the member identity lets the client match
/// returned volumes against owned ones (source ordinals are truth; local
/// `volume_number` is never consulted).
#[flutter_rust_bridge::frb]
pub struct FrbDiscoverySeriesLookup {
    /// Local collection id: client-side throttle/cache key, never sent.
    pub collection_id: String,
    /// User-authored series name, opaque tiebreaker for the hub.
    pub name: String,
    /// Up to 3 checksum-valid member ISBNs (canonical ISBN-13).
    pub anchor_isbns: Vec<String>,
    /// All member ISBNs in both ISBN-10/13 forms.
    pub member_isbns: Vec<String>,
    /// Normalized "title|author" keys of the members, one per author.
    pub member_title_author_keys: Vec<String>,
}

/// One "complete the author" lookup: the profile author name plus up to 3
/// anchor ISBNs of liked books by that author, for hub-side verification.
#[flutter_rust_bridge::frb]
pub struct FrbDiscoveryAuthorLookup {
    pub name: String,
    pub anchor_isbns: Vec<String>,
}

/// Everything the Dart discovery service needs: the lookups derived from
/// the library and the identity index that filters the answers (a volume
/// or work matching by ISBN or title+author is never suggested). Empty
/// below the ADR-059 profile threshold: no lookups without local signal.
#[flutter_rust_bridge::frb]
pub struct FrbDiscoveryLookupInputs {
    pub series: Vec<FrbDiscoverySeriesLookup>,
    pub authors: Vec<FrbDiscoveryAuthorLookup>,
    /// Library ISBNs (all statuses including `wanting`), both 10/13 forms.
    pub library_isbns: Vec<String>,
    /// Normalized "title|author" keys for every library book.
    pub library_title_author_keys: Vec<String>,
    /// The two index halves restricted to LIKED books (ADR-066), a strict
    /// subset of the two above. Additive fields (Rule R5); older Flutter
    /// builds simply ignore them.
    pub liked_isbns: Vec<String>,
    pub liked_title_author_keys: Vec<String>,
}

impl From<crate::domain::recommendations::DiscoveryLookupInputs> for FrbDiscoveryLookupInputs {
    fn from(inputs: crate::domain::recommendations::DiscoveryLookupInputs) -> Self {
        Self {
            series: inputs
                .series
                .into_iter()
                .map(|s| FrbDiscoverySeriesLookup {
                    collection_id: s.collection_id,
                    name: s.name,
                    anchor_isbns: s.anchor_isbns,
                    member_isbns: s.member_isbns,
                    member_title_author_keys: s.member_title_author_keys,
                })
                .collect(),
            authors: inputs
                .authors
                .into_iter()
                .map(|a| FrbDiscoveryAuthorLookup {
                    name: a.name,
                    anchor_isbns: a.anchor_isbns,
                })
                .collect(),
            library_isbns: inputs.library_isbns,
            library_title_author_keys: inputs.library_title_author_keys,
            liked_isbns: inputs.liked_isbns,
            liked_title_author_keys: inputs.liked_title_author_keys,
        }
    }
}

/// Inputs of the external discovery lookups (ADR-060): what to ask the hub
/// resolver and how to filter its answers. The Flutter side owns the HTTP
/// call, the 24h throttle and the cache (Rule F4); this function only
/// derives the inputs from one local library pass.
#[flutter_rust_bridge::frb]
pub async fn get_discovery_lookup_inputs() -> Result<FrbDiscoveryLookupInputs, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::discovery_lookup_service::discovery_lookup_inputs(db)
        .await
        .map(FrbDiscoveryLookupInputs::from)
        .map_err(|e| format!("{e:?}"))
}
