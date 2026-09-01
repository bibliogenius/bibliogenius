//! Favorites typed collection (ADR-064).
//!
//! A favorite is membership in the single `collections.source = 'favorites'`
//! collection (ADR-052 typed-collection pattern; zero schema change). The
//! stored name is the technical sentinel [`FAVORITES_SENTINEL_NAME`]: it is
//! never displayed, every UI derives the label from the type via i18n.
//!
//! This module owns:
//! - lazy creation (the universal net: first toggle creates the collection,
//!   deletion just unmarks everything and the next toggle recreates it);
//! - the seeding gate (Reader preset selection only, eligibility enforced
//!   here so every caller applies the same rule);
//! - the one-shot adoption of a pre-existing manual "favorites-like"
//!   collection on existing installs;
//! - the multi-device keep-oldest merge: `collections`/`collection_books`
//!   are cr-sqlite CRRs, so two enrolled devices can each create their own
//!   favorites collection before account sync converges. Everything here
//!   goes through ordinary row writes (never an ALTER of a CRR table), and
//!   the canonical pick (`created_at` asc, `id` asc) is a total order
//!   identical on every device, so independent merges converge.

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::domain::DomainError;
use crate::domain::collection_repository::{
    Collection, CollectionRepository, CreateCollectionInput,
};
use crate::infrastructure::repositories::collection_repository::SeaOrmCollectionRepository;
use crate::models::{book, collection_book};
use crate::services::recommendation_service::FAVORITE_SHELF_LABELS;

/// Machine source of the typed favorites collection. Matched everywhere the
/// feature is recognized; display names are never matched.
pub const FAVORITES_SOURCE: &str = "favorites";

/// Technical stored name of a lazily created or seeded favorites collection.
/// Never displayed: the UI renders the translated catalogue string whenever
/// `source == 'favorites'` (same lesson as the banned "My Library" seed).
pub const FAVORITES_SENTINEL_NAME: &str = "__favorites__";

fn repo(db: &DatabaseConnection) -> SeaOrmCollectionRepository {
    SeaOrmCollectionRepository::new(db.clone())
}

fn is_favorites_like_name(name: &str) -> bool {
    // The engine's own normalization, so "favorites-like" means exactly the
    // same thing in both features.
    let normed = crate::services::recommendation_service::norm(name);
    FAVORITE_SHELF_LABELS.contains(&normed.as_str())
}

/// Names that qualify a non-typed collection for adoption (and block the
/// seed): user favorites labels, plus the technical sentinel itself. A
/// collection literally named `__favorites__` is a typed collection whose
/// source was lost (an old build's series flip, a partial sync): adoption
/// is its recovery path back to `source = 'favorites'`.
fn is_adoptable_name(name: &str) -> bool {
    name == FAVORITES_SENTINEL_NAME || is_favorites_like_name(name)
}

/// Refuse identity-destroying source flips on the typed favorites
/// collection: `set_source` is how ADR-052 flips manual/series, but on the
/// favorites collection it silently untypes it (the raw sentinel name
/// resurfaces, marker and liked signal die). Called by the series-flip
/// handlers (FFI and HTTP) before touching the source.
pub async fn ensure_not_favorites(
    db: &DatabaseConnection,
    collection_id: &str,
) -> Result<(), DomainError> {
    if is_typed_favorites(db, collection_id).await? {
        return Err(DomainError::Validation(
            "the favorites collection cannot be flipped to or from a series".to_string(),
        ));
    }
    Ok(())
}

/// Refuse a rename on the typed favorites collection: its displayed label
/// comes from the i18n catalogue, never from `collections.name` (ADR-064),
/// so a stored name would be invisible on every screen and in all 11
/// languages. Called by the rename entry point; the UI hides the action.
pub async fn ensure_renamable(
    db: &DatabaseConnection,
    collection_id: &str,
) -> Result<(), DomainError> {
    if is_typed_favorites(db, collection_id).await? {
        return Err(DomainError::Validation(
            "the favorites collection cannot be renamed: its label comes from the translations"
                .to_string(),
        ));
    }
    Ok(())
}

/// Whether `collection_id` is THE typed favorites collection. An unknown id
/// is not: reporting it is the calling handler's job, not the guard's.
async fn is_typed_favorites(
    db: &DatabaseConnection,
    collection_id: &str,
) -> Result<bool, DomainError> {
    Ok(repo(db)
        .find_by_id(collection_id)
        .await?
        .is_some_and(|c| c.source == FAVORITES_SOURCE))
}

/// Resolve the canonical favorites collection, merging duplicates first.
///
/// Among all `source = 'favorites'` rows the OLDEST (`created_at` asc, `id`
/// asc) is canonical; every other one has its members moved over (idempotent
/// insert) and is then deleted through the standard collection deletion
/// (junction rows first, books untouched). Returns `None` when no favorites
/// collection exists: creation is the caller's decision.
pub async fn resolve_favorites_collection(
    db: &DatabaseConnection,
) -> Result<Option<Collection>, DomainError> {
    let repo = repo(db);
    let mut favorites = repo.find_by_source(FAVORITES_SOURCE).await?;
    if favorites.is_empty() {
        return Ok(None);
    }
    let canonical = favorites.remove(0);

    for duplicate in favorites {
        let member_ids: Vec<String> = collection_book::Entity::find()
            .filter(collection_book::Column::CollectionId.eq(&duplicate.id))
            .all(db)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?
            .into_iter()
            .map(|cb| cb.book_id)
            .collect();
        for book_id in member_ids {
            repo.add_book(&canonical.id, &book_id).await?;
        }
        crate::services::collection_service::delete_collection(db, &duplicate.id, false)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?;
    }

    // Re-read so the returned counts include merged members.
    repo.find_by_id(&canonical.id).await
}

/// The canonical favorites collection, created lazily when absent.
pub async fn get_or_create_favorites_collection(
    db: &DatabaseConnection,
) -> Result<Collection, DomainError> {
    if let Some(existing) = resolve_favorites_collection(db).await? {
        return Ok(existing);
    }
    repo(db)
        .create(CreateCollectionInput {
            name: FAVORITES_SENTINEL_NAME.to_string(),
            description: None,
            source: Some(FAVORITES_SOURCE.to_string()),
        })
        .await
}

/// Toggle a book's favorite state. Returns the NEW state (`true` = now a
/// favorite). Creates the collection lazily on the first marking.
pub async fn toggle_favorite_book(
    db: &DatabaseConnection,
    book_id: &str,
) -> Result<bool, DomainError> {
    if book::Entity::find_by_id(book_id)
        .one(db)
        .await
        .map_err(|e| DomainError::Database(e.to_string()))?
        .is_none()
    {
        return Err(DomainError::NotFound);
    }

    let favorites = get_or_create_favorites_collection(db).await?;
    let repo = repo(db);
    let is_member = collection_book::Entity::find()
        .filter(collection_book::Column::CollectionId.eq(&favorites.id))
        .filter(collection_book::Column::BookId.eq(book_id))
        .one(db)
        .await
        .map_err(|e| DomainError::Database(e.to_string()))?
        .is_some();

    if is_member {
        repo.remove_book(&favorites.id, book_id).await?;
        Ok(false)
    } else {
        repo.add_book(&favorites.id, book_id).await?;
        Ok(true)
    }
}

/// All favorite book ids, in one pass. Empty when no favorites collection
/// exists (the caller must NOT create one just to read an empty set).
pub async fn get_favorite_book_ids(db: &DatabaseConnection) -> Result<Vec<String>, DomainError> {
    let Some(favorites) = resolve_favorites_collection(db).await? else {
        return Ok(Vec::new());
    };
    Ok(collection_book::Entity::find()
        .filter(collection_book::Column::CollectionId.eq(&favorites.id))
        .all(db)
        .await
        .map_err(|e| DomainError::Database(e.to_string()))?
        .into_iter()
        .map(|cb| cb.book_id)
        .collect())
}

/// Seed the empty favorites collection at Reader-profile selection (ADR-064).
///
/// Returns `true` when the collection was created. The gate is enforced here
/// so every caller applies the same rule; the seed is a no-op unless ALL
/// hold:
/// 1. no `source = 'favorites'` collection exists (duplicates are merged,
///    not re-seeded);
/// 2. no collection of any source carries a favorites-like name (the user
///    already built their own; the adoption flow owns that case);
/// 3. no shelf label (`books.subjects` entry) is favorites-like (legacy
///    shelf variant of the same situation).
///
/// App startup, app update and DB migration MUST NOT call this: the only
/// trigger is the explicit Reader preset selection, so an existing install
/// can never see the collection appear spontaneously.
pub async fn seed_favorites_collection(db: &DatabaseConnection) -> Result<bool, DomainError> {
    if resolve_favorites_collection(db).await?.is_some() {
        return Ok(false);
    }

    let repository = repo(db);
    let all = repository.find_all().await?;
    if all.iter().any(|c| is_adoptable_name(&c.name)) {
        return Ok(false);
    }

    if any_favorites_like_shelf(db).await? {
        return Ok(false);
    }

    repository
        .create(CreateCollectionInput {
            name: FAVORITES_SENTINEL_NAME.to_string(),
            description: None,
            source: Some(FAVORITES_SOURCE.to_string()),
        })
        .await?;
    Ok(true)
}

/// Whether any book carries a favorites-like shelf label in its subjects.
async fn any_favorites_like_shelf(db: &DatabaseConnection) -> Result<bool, DomainError> {
    let books = book::Entity::find()
        .all(db)
        .await
        .map_err(|e| DomainError::Database(e.to_string()))?;
    for b in books {
        if let Some(subjects_json) = b.subjects
            && let Ok(subjects) = serde_json::from_str::<Vec<String>>(&subjects_json)
            && subjects.iter().any(|s| is_favorites_like_name(s))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The collection to propose for one-shot adoption, if any: the OLDEST
/// favorites-like collection of any non-typed source, and only while no
/// typed favorites collection exists. The remembered refusal is
/// device-local and lives on the Flutter side.
///
/// Source is deliberately NOT restricted to 'manual': a real library
/// carried a "favoris" collection flipped to `source = 'series'` by a
/// stray series toggle, and excluding it produced exactly the duplicate
/// this flow exists to prevent. A favorites-like NAME is the signal; the
/// adoption dialog makes the type change explicit (a series adopted as
/// favorites stops rendering its frieze).
pub async fn get_favorites_adoption_candidate(
    db: &DatabaseConnection,
) -> Result<Option<Collection>, DomainError> {
    if resolve_favorites_collection(db).await?.is_some() {
        return Ok(None);
    }
    let mut candidates: Vec<Collection> = repo(db)
        .find_all()
        .await?
        .into_iter()
        .filter(|c| c.source != FAVORITES_SOURCE && is_adoptable_name(&c.name))
        .collect();
    // Oldest first (created_at asc, id asc), the same total order as the
    // multi-device merge rule.
    candidates.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(candidates.into_iter().next())
}

/// Adopt a collection as THE favorites collection: flip its source, keep
/// its name and members. Validation keeps the operation honest (the HTTP
/// mirror exposes it beyond the guided flow): the target must exist and
/// carry a favorites-like name, and no typed favorites collection may
/// already exist.
pub async fn adopt_favorites_collection(
    db: &DatabaseConnection,
    collection_id: &str,
) -> Result<(), DomainError> {
    if resolve_favorites_collection(db).await?.is_some() {
        return Err(DomainError::Validation(
            "a favorites collection already exists".to_string(),
        ));
    }
    let repository = repo(db);
    let Some(target) = repository.find_by_id(collection_id).await? else {
        return Err(DomainError::NotFound);
    };
    if !is_adoptable_name(&target.name) {
        return Err(DomainError::Validation(
            "only a favorites-like collection can be adopted".to_string(),
        ));
    }
    repository.set_source(collection_id, FAVORITES_SOURCE).await
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Set, Statement};

    use super::*;

    async fn setup() -> DatabaseConnection {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        // Seeded books use `library_id = 0`; relax FK checks like the other
        // repository/service tests do.
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "PRAGMA foreign_keys = OFF".to_owned(),
        ))
        .await
        .unwrap();
        db
    }

    async fn insert_book(db: &DatabaseConnection, title: &str, subjects: Option<&str>) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        let id = crate::utils::uuid_gen::new_uuid_v7();
        book::Entity::insert(book::ActiveModel {
            id: Set(id.clone()),
            title: Set(title.to_owned()),
            reading_status: Set("to_read".to_owned()),
            owned: Set(true),
            subjects: Set(subjects.map(str::to_owned)),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap();
        id
    }

    /// Insert a collection row with an explicit `created_at` so merge-order
    /// tests control which one is oldest.
    async fn insert_collection(
        db: &DatabaseConnection,
        id: &str,
        name: &str,
        source: &str,
        created_at: &str,
    ) {
        use crate::models::collection;
        collection::Entity::insert(collection::ActiveModel {
            id: Set(id.to_owned()),
            name: Set(name.to_owned()),
            description: Set(None),
            source: Set(source.to_owned()),
            created_at: Set(created_at.to_owned()),
            updated_at: Set(created_at.to_owned()),
        })
        .exec(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn toggle_creates_lazily_with_sentinel_and_flips_membership() {
        let db = setup().await;
        let book_id = insert_book(&db, "Book", None).await;

        assert!(get_favorite_book_ids(&db).await.unwrap().is_empty());
        assert!(resolve_favorites_collection(&db).await.unwrap().is_none());

        assert!(toggle_favorite_book(&db, &book_id).await.unwrap());
        let favorites = resolve_favorites_collection(&db).await.unwrap().unwrap();
        assert_eq!(favorites.source, FAVORITES_SOURCE);
        assert_eq!(favorites.name, FAVORITES_SENTINEL_NAME);
        assert_eq!(
            get_favorite_book_ids(&db).await.unwrap(),
            vec![book_id.clone()]
        );

        assert!(!toggle_favorite_book(&db, &book_id).await.unwrap());
        assert!(get_favorite_book_ids(&db).await.unwrap().is_empty());
        // The collection survives an empty membership; only deletion removes it.
        assert!(resolve_favorites_collection(&db).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn toggle_unknown_book_is_not_found() {
        let db = setup().await;
        assert!(matches!(
            toggle_favorite_book(&db, "missing").await,
            Err(DomainError::NotFound)
        ));
    }

    #[tokio::test]
    async fn resolve_merges_duplicates_keeping_oldest_and_members() {
        let db = setup().await;
        let a = insert_book(&db, "A", None).await;
        let b = insert_book(&db, "B", None).await;
        let shared = insert_book(&db, "Shared", None).await;

        insert_collection(
            &db,
            "newer",
            FAVORITES_SENTINEL_NAME,
            FAVORITES_SOURCE,
            "2026-08-02T00:00:00Z",
        )
        .await;
        insert_collection(
            &db,
            "older",
            FAVORITES_SENTINEL_NAME,
            FAVORITES_SOURCE,
            "2026-08-01T00:00:00Z",
        )
        .await;
        let repository = repo(&db);
        repository.add_book("older", &a).await.unwrap();
        repository.add_book("older", &shared).await.unwrap();
        repository.add_book("newer", &b).await.unwrap();
        repository.add_book("newer", &shared).await.unwrap();

        let canonical = resolve_favorites_collection(&db).await.unwrap().unwrap();
        assert_eq!(canonical.id, "older", "the oldest row wins");

        let remaining = repository.find_by_source(FAVORITES_SOURCE).await.unwrap();
        assert_eq!(remaining.len(), 1, "the duplicate is deleted");

        let mut ids = get_favorite_book_ids(&db).await.unwrap();
        ids.sort();
        let mut expected = vec![a, b, shared];
        expected.sort();
        assert_eq!(ids, expected, "members merged, shared member deduplicated");
    }

    #[tokio::test]
    async fn resolve_tie_breaks_on_id_for_equal_created_at() {
        let db = setup().await;
        insert_collection(
            &db,
            "b-id",
            FAVORITES_SENTINEL_NAME,
            FAVORITES_SOURCE,
            "2026-08-01T00:00:00Z",
        )
        .await;
        insert_collection(
            &db,
            "a-id",
            FAVORITES_SENTINEL_NAME,
            FAVORITES_SOURCE,
            "2026-08-01T00:00:00Z",
        )
        .await;
        let canonical = resolve_favorites_collection(&db).await.unwrap().unwrap();
        assert_eq!(canonical.id, "a-id");
    }

    #[tokio::test]
    async fn seed_creates_when_eligible() {
        let db = setup().await;
        assert!(seed_favorites_collection(&db).await.unwrap());
        let favorites = resolve_favorites_collection(&db).await.unwrap().unwrap();
        assert_eq!(favorites.name, FAVORITES_SENTINEL_NAME);
        assert_eq!(favorites.source, FAVORITES_SOURCE);
        // Idempotent: a second seed is a no-op.
        assert!(!seed_favorites_collection(&db).await.unwrap());
    }

    #[tokio::test]
    async fn seed_declines_when_favorites_like_collection_exists() {
        let db = setup().await;
        insert_collection(&db, "mine", "Favoris", "manual", "2026-08-01T00:00:00Z").await;
        assert!(!seed_favorites_collection(&db).await.unwrap());
        assert!(resolve_favorites_collection(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn seed_declines_when_favorites_like_shelf_exists() {
        let db = setup().await;
        insert_book(&db, "Book", Some(r#"["SF"," Favorites "]"#)).await;
        assert!(!seed_favorites_collection(&db).await.unwrap());
        assert!(resolve_favorites_collection(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn adoption_candidate_is_oldest_favorites_like_collection() {
        let db = setup().await;
        insert_collection(&db, "reading", "Lectures", "manual", "2026-07-01T00:00:00Z").await;
        insert_collection(
            &db,
            "younger",
            "favorites",
            "manual",
            "2026-08-02T00:00:00Z",
        )
        .await;
        insert_collection(&db, "elder", "Favoris", "manual", "2026-08-01T00:00:00Z").await;

        let candidate = get_favorites_adoption_candidate(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(candidate.id, "elder");
    }

    #[tokio::test]
    async fn adoption_candidate_includes_series_flagged_favorites() {
        // Field regression: a real "favoris" collection carried
        // `source = 'series'` (stray series toggle) and the manual-only
        // filter skipped it, producing the exact duplicate the adoption
        // flow exists to prevent. Favorites-like NAME is the signal, the
        // source only excludes the already-typed collection.
        let db = setup().await;
        insert_collection(&db, "mine", "favoris", "series", "2026-07-02T00:00:00Z").await;
        insert_collection(&db, "younger", "Favoris", "manual", "2026-08-01T00:00:00Z").await;

        let candidate = get_favorites_adoption_candidate(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(candidate.id, "mine", "the series-flagged one is oldest");

        adopt_favorites_collection(&db, "mine").await.unwrap();
        let favorites = resolve_favorites_collection(&db).await.unwrap().unwrap();
        assert_eq!(favorites.id, "mine");
        assert_eq!(
            favorites.source, FAVORITES_SOURCE,
            "series-ness is stripped"
        );
    }

    #[tokio::test]
    async fn series_flip_is_refused_on_the_favorites_collection() {
        let db = setup().await;
        insert_collection(
            &db,
            "typed",
            FAVORITES_SENTINEL_NAME,
            FAVORITES_SOURCE,
            "2026-08-01T00:00:00Z",
        )
        .await;
        insert_collection(
            &db,
            "cycle",
            "Harry Potter",
            "series",
            "2026-08-02T00:00:00Z",
        )
        .await;

        assert!(matches!(
            ensure_not_favorites(&db, "typed").await,
            Err(DomainError::Validation(_))
        ));
        // Every other collection keeps its series toggle.
        assert!(ensure_not_favorites(&db, "cycle").await.is_ok());
        // An unknown id is not this guard's concern (NotFound is the
        // flip handler's job).
        assert!(ensure_not_favorites(&db, "missing").await.is_ok());
    }

    #[tokio::test]
    async fn a_flipped_sentinel_collection_recovers_through_adoption() {
        // Field regression: an old build's series toggle flipped the typed
        // collection to `source = 'series'`, leaving a collection literally
        // named `__favorites__` that nothing proposed for adoption; the
        // next toggle then lazily created a duplicate.
        let db = setup().await;
        let a = insert_book(&db, "A", None).await;
        insert_collection(
            &db,
            "lost",
            FAVORITES_SENTINEL_NAME,
            "series",
            "2026-08-22T00:00:00Z",
        )
        .await;
        repo(&db).add_book("lost", &a).await.unwrap();

        // The stray sentinel blocks re-seeding (no duplicate)...
        assert!(!seed_favorites_collection(&db).await.unwrap());
        // ...and is proposed for adoption, which restores the type.
        let candidate = get_favorites_adoption_candidate(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(candidate.id, "lost");
        adopt_favorites_collection(&db, "lost").await.unwrap();
        let favorites = resolve_favorites_collection(&db).await.unwrap().unwrap();
        assert_eq!(favorites.id, "lost");
        assert_eq!(get_favorite_book_ids(&db).await.unwrap(), vec![a]);
    }

    #[tokio::test]
    async fn adoption_refuses_a_non_favorites_like_target() {
        let db = setup().await;
        insert_collection(
            &db,
            "cycle",
            "Harry Potter",
            "series",
            "2026-07-01T00:00:00Z",
        )
        .await;
        assert!(matches!(
            adopt_favorites_collection(&db, "cycle").await,
            Err(DomainError::Validation(_))
        ));
        assert!(matches!(
            adopt_favorites_collection(&db, "missing").await,
            Err(DomainError::NotFound)
        ));
    }

    #[tokio::test]
    async fn adoption_flips_source_and_keeps_members_then_blocks_reseeding() {
        let db = setup().await;
        let a = insert_book(&db, "A", None).await;
        insert_collection(&db, "mine", "Favoris", "manual", "2026-08-01T00:00:00Z").await;
        repo(&db).add_book("mine", &a).await.unwrap();

        adopt_favorites_collection(&db, "mine").await.unwrap();

        let favorites = resolve_favorites_collection(&db).await.unwrap().unwrap();
        assert_eq!(favorites.id, "mine");
        assert_eq!(favorites.name, "Favoris", "the user's name is kept");
        assert_eq!(get_favorite_book_ids(&db).await.unwrap(), vec![a]);

        // Once adopted there is no candidate left and no seed either.
        assert!(
            get_favorites_adoption_candidate(&db)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!seed_favorites_collection(&db).await.unwrap());
    }

    #[tokio::test]
    async fn adoption_refused_when_typed_collection_exists() {
        let db = setup().await;
        insert_collection(
            &db,
            "typed",
            FAVORITES_SENTINEL_NAME,
            FAVORITES_SOURCE,
            "2026-08-01T00:00:00Z",
        )
        .await;
        insert_collection(&db, "mine", "Favoris", "manual", "2026-08-02T00:00:00Z").await;
        assert!(matches!(
            adopt_favorites_collection(&db, "mine").await,
            Err(DomainError::Validation(_))
        ));
    }

    #[tokio::test]
    async fn candidate_is_none_when_typed_collection_exists() {
        let db = setup().await;
        insert_collection(
            &db,
            "typed",
            FAVORITES_SENTINEL_NAME,
            FAVORITES_SOURCE,
            "2026-08-01T00:00:00Z",
        )
        .await;
        insert_collection(&db, "mine", "Favoris", "manual", "2026-08-02T00:00:00Z").await;
        assert!(
            get_favorites_adoption_candidate(&db)
                .await
                .unwrap()
                .is_none()
        );
    }
}
