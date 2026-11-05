// Tag CRUD and subject renaming.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

/// Simplified tag structure for FFI
#[frb(dart_metadata=("freezed"))]
pub struct FrbTag {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    /// Books on this shelf that the DEFAULT library view shows.
    pub count: i64,
    /// Books on this shelf whatever their ownership.
    ///
    /// The two differ as soon as a shelf holds wished or given-away books,
    /// and the reader can now remember an ownership axis that shows them
    /// (ADR-063), so the client picks the number that matches the shelf its
    /// tap will open. Printing `count` under a "show everything" axis is how
    /// a shelf ends up announcing 0 and then listing ten books.
    pub total_count: i64,
    /// Up to three covers for the shelf's thumbnail, newest first.
    pub cover_paths: Vec<String>,
    /// [`count`](Self::count) with the sub-shelves: the books of the default
    /// view on this shelf or below it, each once.
    pub subtree_count: i64,
    /// [`total_count`](Self::total_count) with the sub-shelves, each book once.
    pub subtree_total_count: i64,
}

/// Get all tags with hierarchy info
pub async fn get_all_tags() -> Result<Vec<FrbTag>, String> {
    let db = db().ok_or("Database not initialized")?;
    get_all_tags_in(db, covers_dir().map(|d| d.as_path())).await
}

/// Number of books in the default library view that sit on no shelf, for the
/// "to shelve" card of the shelves screen. Same scope as the `count` of
/// [`get_all_tags`]: wished books are left out.
pub async fn count_unshelved_books() -> Result<i64, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::book_service::unshelved_count(db)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// [`get_all_tags`] over an explicit connection and covers directory, so it
/// can be tested without the FFI globals. A flat list: the tree, and its
/// tolerance of a cycle merged from sync, are built by the client.
async fn get_all_tags_in(
    db: &sea_orm::DatabaseConnection,
    covers_dir: Option<&std::path::Path>,
) -> Result<Vec<FrbTag>, String> {
    use crate::services::book_service::{CoverChannel, shelf_listing};

    // The `tags` rows, the badge counts scoped to the default library view
    // (ADR-063) and the thumbnail covers, from one read of the books. The
    // rule lives in the service, shared with `GET /api/books/tags`.
    let listing = shelf_listing(db, CoverChannel::OnDevice(covers_dir))
        .await
        .map_err(|e| format!("{:?}", e))?;
    let tag_counts = listing.counts;
    let subtree_counts = listing.subtree_counts;
    let covers = listing.covers;

    // Merge: Prioritize DB hierarchy, add legacy tags as roots if missing
    let mut result = Vec::new();
    let mut processed_names = std::collections::HashSet::new();

    // Add DB tags
    for t in listing.tags {
        let counts = tag_counts.get(&t.name).copied().unwrap_or_default();
        let subtree = subtree_counts.get(&t.name).copied().unwrap_or_default();
        processed_names.insert(t.name.clone());
        result.push(FrbTag {
            cover_paths: covers.get(&t.name).cloned().unwrap_or_default(),
            subtree_count: subtree.in_default_view,
            subtree_total_count: subtree.total,
            id: t.id,
            name: t.name,
            parent_id: t.parent_id,
            count: counts.in_default_view,
            total_count: counts.total,
        });
    }

    // Add remaining legacy tags (as orphans)
    // Give them synthetic "legacy:" string ids to distinguish from DB tags (uuids).
    let mut next_legacy_id = -1;
    for (name, counts) in tag_counts {
        if !processed_names.contains(&name) {
            let subtree = subtree_counts.get(&name).copied().unwrap_or_default();
            result.push(FrbTag {
                id: format!("legacy:{next_legacy_id}"),
                cover_paths: covers.get(&name).cloned().unwrap_or_default(),
                subtree_count: subtree.in_default_view,
                subtree_total_count: subtree.total,
                name,
                parent_id: None,
                count: counts.in_default_view,
                total_count: counts.total,
            });
            next_legacy_id -= 1;
        }
    }

    // Sort by name
    result.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(result)
}

/// Create a new tag
pub async fn create_tag(name: String, parent_id: Option<String>) -> Result<FrbTag, String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::models::tag;
    use sea_orm::{ActiveModelTrait, Set};

    let new_tag = tag::ActiveModel {
        name: Set(name),
        parent_id: Set(parent_id),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    match new_tag.insert(db).await {
        Ok(t) => {
            let _ = crate::sync::log_operation(db, "tag", &t.id, "INSERT", None).await;
            Ok(FrbTag {
                id: t.id,
                name: t.name,
                parent_id: t.parent_id,
                count: 0,
                total_count: 0,
                cover_paths: Vec::new(),
                subtree_count: 0,
                subtree_total_count: 0,
            })
        }
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Update a tag
pub async fn update_tag(
    id: String,
    name: String,
    parent_id: Option<String>,
) -> Result<FrbTag, String> {
    let db = db().ok_or("Database not initialized")?;
    update_tag_in(db, id, name, parent_id).await
}

/// [`update_tag`] over an explicit connection, so it can be tested without
/// the global FFI database. Refuses a move that would put a shelf under
/// itself or one of its sub-shelves, before anything is written.
async fn update_tag_in(
    db: &sea_orm::DatabaseConnection,
    id: String,
    name: String,
    parent_id: Option<String>,
) -> Result<FrbTag, String> {
    use crate::models::tag;
    use crate::services::book_service::ServiceError;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};

    crate::services::shelf_tree_service::ensure_parent_allowed(db, &id, parent_id.as_deref())
        .await
        .map_err(|e| match e {
            ServiceError::InvalidInput(msg) => msg,
            other => format!("{other:?}"),
        })?;

    let tag_model = tag::Entity::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| format!("{:?}", e))?;
    let Some(tag_model) = tag_model else {
        return Err("Tag not found".to_string());
    };

    let old_name = tag_model.name.clone();

    let mut active: tag::ActiveModel = tag_model.into();
    active.name = Set(name.clone());
    active.parent_id = Set(parent_id);
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());

    match active.update(db).await {
        Ok(t) => {
            // Also rename the subject in all books that reference the old name
            if old_name != name {
                rename_subject_in_books(db, &old_name, &name).await;
            }
            let _ = crate::sync::log_operation(db, "tag", &t.id, "UPDATE", None).await;
            Ok(FrbTag {
                id: t.id,
                name: t.name,
                parent_id: t.parent_id,
                count: 0,
                total_count: 0,
                cover_paths: Vec::new(),
                subtree_count: 0,
                subtree_total_count: 0,
            })
        }
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Public FFI entry point: rename a subject in all books.
pub async fn rename_subject(old_name: String, new_name: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    rename_subject_in_books(db, &old_name, &new_name).await;
    Ok(())
}

/// Rename a subject across all books' subjects JSON array.
/// Used when renaming a tag/shelf to keep book associations in sync.
async fn rename_subject_in_books(db: &sea_orm::DatabaseConnection, old_name: &str, new_name: &str) {
    use crate::models::book::{Column as BookColumn, Entity as BookEntity};
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

    let books = match BookEntity::find()
        .filter(BookColumn::Subjects.contains(old_name))
        .all(db)
        .await
    {
        Ok(b) => b,
        Err(_) => return,
    };

    for book in books {
        let Some(subjects_str) = &book.subjects else {
            continue;
        };
        let Ok(mut subjects) = serde_json::from_str::<Vec<String>>(subjects_str) else {
            continue;
        };
        let mut changed = false;
        for s in &mut subjects {
            if s == old_name {
                *s = new_name.to_string();
                changed = true;
            }
        }
        if changed {
            let new_subjects = serde_json::to_string(&subjects).unwrap_or_default();
            let mut active: crate::models::book::ActiveModel = book.into();
            active.subjects = Set(Some(new_subjects));
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active.update(db).await;
        }
    }
}

/// Delete a tag
pub async fn delete_tag(id: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    match crate::services::book_service::delete_shelf(db, &id).await {
        Ok(()) => {
            let _ = crate::sync::log_operation(db, "tag", &id, "DELETE", None).await;
            Ok(())
        }
        Err(crate::services::book_service::ServiceError::NotFound) => {
            Err("Tag not found".to_string())
        }
        Err(e) => Err(format!("{e:?}")),
    }
}

/// Delete a shelf that has no `tags` row: a name that only lives in the
/// books' subjects (a synthetic orphan on the shelves screen). Same outcome
/// as `delete_tag` for the books, with nothing to cascade.
pub async fn remove_subject(name: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::book_service::remove_subject_from_books(db, &name)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

#[cfg(test)]
mod tag_tree_tests {
    use super::{get_all_tags_in, update_tag_in};
    use crate::models::tag;
    use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};

    async fn shelf(db: &DatabaseConnection, name: &str, parent: Option<&str>) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        tag::ActiveModel {
            name: Set(name.to_owned()),
            parent_id: Set(parent.map(str::to_owned)),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
        .id
    }

    async fn parent_of(db: &DatabaseConnection, id: &str) -> Option<String> {
        tag::Entity::find_by_id(id.to_owned())
            .one(db)
            .await
            .unwrap()
            .unwrap()
            .parent_id
    }

    #[tokio::test]
    async fn update_tag_refuses_to_move_a_shelf_under_itself() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let top = shelf(&db, "Living room", None).await;

        let err = update_tag_in(&db, top.clone(), "Living room".into(), Some(top.clone()))
            .await
            .err()
            .expect("a shelf cannot be its own parent");

        assert!(err.contains("cannot be moved under itself"), "{err}");
        assert_eq!(parent_of(&db, &top).await, None, "nothing was written");
    }

    #[tokio::test]
    async fn update_tag_refuses_to_move_a_shelf_under_its_grandchild() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let top = shelf(&db, "Living room", None).await;
        let middle = shelf(&db, "Novels", Some(&top)).await;
        let leaf = shelf(&db, "Crime", Some(&middle)).await;

        let err = update_tag_in(&db, top.clone(), "Living room".into(), Some(leaf))
            .await
            .err()
            .expect("a shelf cannot move under its own sub-shelf");

        assert!(err.contains("cannot be moved under itself"), "{err}");
        assert_eq!(parent_of(&db, &top).await, None, "nothing was written");
    }

    #[tokio::test]
    async fn update_tag_still_moves_a_shelf_elsewhere_and_to_the_first_level() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let top = shelf(&db, "Living room", None).await;
        let other = shelf(&db, "Office", None).await;
        let middle = shelf(&db, "Novels", Some(&top)).await;

        assert!(
            update_tag_in(&db, middle.clone(), "Novels".into(), Some(other.clone()))
                .await
                .is_ok()
        );
        assert_eq!(parent_of(&db, &middle).await, Some(other));

        assert!(
            update_tag_in(&db, middle.clone(), "Novels".into(), None)
                .await
                .is_ok()
        );
        assert_eq!(parent_of(&db, &middle).await, None);
    }

    #[tokio::test]
    async fn get_all_tags_answers_over_a_cycle_merged_from_sync() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let a = shelf(&db, "A", None).await;
        let b = shelf(&db, "B", Some(&a)).await;
        // Close the loop behind the gate, the way a sync merge can.
        let mut active: tag::ActiveModel = tag::Entity::find_by_id(a.clone())
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .into();
        active.parent_id = Set(Some(b.clone()));
        active.update(&db).await.unwrap();

        let Ok(tags) = get_all_tags_in(&db, None).await else {
            panic!("a cycle is not an error");
        };

        let ids: Vec<&str> = tags.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&a.as_str()) && ids.contains(&b.as_str()));
    }

    // ---- Shelf thumbnails: `cover_paths` ----

    /// A book filed on `shelves`, last modified at `updated_at` (RFC 3339, so
    /// the order between books is the one the test states).
    async fn book(
        db: &DatabaseConnection,
        shelves: &[&str],
        cover: Option<&str>,
        owned: bool,
        updated_at: &str,
    ) -> String {
        use crate::models::book;
        let id = uuid::Uuid::new_v4().to_string();
        book::Entity::insert(book::ActiveModel {
            id: Set(id.clone()),
            title: Set(format!("Book {updated_at}")),
            owned: Set(owned),
            subjects: Set(Some(serde_json::to_string(shelves).unwrap())),
            cover_url: Set(cover.map(str::to_owned)),
            created_at: Set(updated_at.to_owned()),
            updated_at: Set(updated_at.to_owned()),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap();
        id
    }

    fn covers_of<'a>(tags: &'a [super::FrbTag], name: &str) -> Vec<&'a str> {
        tags.iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} is listed"))
            .cover_paths
            .iter()
            .map(String::as_str)
            .collect()
    }

    #[tokio::test]
    async fn a_shelf_shows_its_own_books_then_the_newest_of_its_subtree() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let top = shelf(&db, "Living room", None).await;
        let middle = shelf(&db, "Novels", Some(&top)).await;
        shelf(&db, "Crime", Some(&middle)).await;
        let t = |d: u32| format!("2026-01-{d:02}T10:00:00+00:00");
        book(&db, &["Crime"], Some("https://c/a.jpg"), true, &t(1)).await;
        book(&db, &["Novels"], Some("https://c/b.jpg"), true, &t(2)).await;
        book(&db, &["Living room"], Some("https://c/c.jpg"), true, &t(3)).await;
        book(&db, &["Crime"], Some("https://c/d.jpg"), true, &t(4)).await;
        // The newest book has no cover: it is skipped, it takes no slot.
        book(&db, &["Novels"], None, true, &t(5)).await;
        book(&db, &["Novels"], Some(""), true, &t(6)).await;

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(
            covers_of(&tags, "Living room"),
            ["https://c/c.jpg", "https://c/d.jpg", "https://c/b.jpg"]
        );
        assert_eq!(
            covers_of(&tags, "Novels"),
            ["https://c/b.jpg", "https://c/d.jpg", "https://c/a.jpg"]
        );
        assert_eq!(
            covers_of(&tags, "Crime"),
            ["https://c/d.jpg", "https://c/a.jpg"]
        );
    }

    #[tokio::test]
    async fn an_old_book_of_the_shelf_comes_before_a_newer_one_of_its_sub_shelf() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let top = shelf(&db, "Living room", None).await;
        shelf(&db, "Novels", Some(&top)).await;
        let t = |d: u32| format!("2026-01-{d:02}T10:00:00+00:00");
        book(
            &db,
            &["Living room"],
            Some("https://c/own.jpg"),
            true,
            &t(1),
        )
        .await;
        book(&db, &["Novels"], Some("https://c/sub.jpg"), true, &t(2)).await;

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(
            covers_of(&tags, "Living room"),
            ["https://c/own.jpg", "https://c/sub.jpg"]
        );
        // The sub-shelf is not given its parent's book.
        assert_eq!(covers_of(&tags, "Novels"), ["https://c/sub.jpg"]);
    }

    #[tokio::test]
    async fn three_own_covers_leave_no_room_for_a_newer_sub_shelf_book() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let top = shelf(&db, "Living room", None).await;
        shelf(&db, "Novels", Some(&top)).await;
        let t = |d: u32| format!("2026-01-{d:02}T10:00:00+00:00");
        for (d, cover) in [(1, "o1"), (2, "o2"), (3, "o3"), (4, "o4")] {
            let url = format!("https://c/{cover}.jpg");
            book(&db, &["Living room"], Some(&url), true, &t(d)).await;
        }
        book(&db, &["Novels"], Some("https://c/sub.jpg"), true, &t(5)).await;

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(
            covers_of(&tags, "Living room"),
            ["https://c/o4.jpg", "https://c/o3.jpg", "https://c/o2.jpg"]
        );
    }

    #[tokio::test]
    async fn a_shelf_without_own_books_shows_those_of_its_descendants() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let top = shelf(&db, "Genre", None).await;
        let middle = shelf(&db, "Essay", Some(&top)).await;
        shelf(&db, "Politics", Some(&middle)).await;
        let t = |d: u32| format!("2026-01-{d:02}T10:00:00+00:00");
        book(&db, &["Politics"], Some("https://c/p.jpg"), true, &t(1)).await;
        book(&db, &["Essay"], Some("https://c/e.jpg"), true, &t(2)).await;

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(
            covers_of(&tags, "Genre"),
            ["https://c/e.jpg", "https://c/p.jpg"]
        );
    }

    #[tokio::test]
    async fn a_book_outside_the_default_view_gives_no_cover() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        shelf(&db, "Novels", None).await;
        book(
            &db,
            &["Novels"],
            Some("https://c/own.jpg"),
            true,
            "2026-01-01T10:00:00+00:00",
        )
        .await;
        // Newer, but wished: the opened shelf does not show it.
        book(
            &db,
            &["Novels"],
            Some("https://c/wish.jpg"),
            false,
            "2026-01-02T10:00:00+00:00",
        )
        .await;

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(covers_of(&tags, "Novels"), ["https://c/own.jpg"]);
    }

    #[tokio::test]
    async fn books_stamped_alike_keep_one_order_between_reloads() {
        use crate::models::book;
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        shelf(&db, "Novels", None).await;
        // Bulk shelving gives every book the same `updated_at`.
        for id in ["a", "b", "c"] {
            book::Entity::insert(book::ActiveModel {
                id: Set(id.to_owned()),
                title: Set(format!("Book {id}")),
                owned: Set(true),
                subjects: Set(Some(r#"["Novels"]"#.to_owned())),
                cover_url: Set(Some(format!("https://c/{id}.jpg"))),
                created_at: Set("2026-01-01T10:00:00+00:00".to_owned()),
                updated_at: Set("2026-01-01T10:00:00+00:00".to_owned()),
                ..Default::default()
            })
            .exec(&db)
            .await
            .unwrap();
        }

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(
            covers_of(&tags, "Novels"),
            ["https://c/c.jpg", "https://c/b.jpg", "https://c/a.jpg"]
        );
    }

    #[tokio::test]
    async fn a_book_on_a_shelf_and_on_its_parent_counts_once_for_the_parent() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let top = shelf(&db, "Living room", None).await;
        shelf(&db, "Novels", Some(&top)).await;
        book(
            &db,
            &["Novels", "Living room"],
            Some("https://c/a.jpg"),
            true,
            "2026-01-01T10:00:00+00:00",
        )
        .await;

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(covers_of(&tags, "Living room"), ["https://c/a.jpg"]);
    }

    #[tokio::test]
    async fn covers_climb_a_cycle_merged_from_sync_without_looping() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let a = shelf(&db, "A", None).await;
        let b = shelf(&db, "B", Some(&a)).await;
        let mut active: tag::ActiveModel = tag::Entity::find_by_id(a.clone())
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .into();
        active.parent_id = Set(Some(b));
        active.update(&db).await.unwrap();
        book(
            &db,
            &["B"],
            Some("https://c/b.jpg"),
            true,
            "2026-01-01T10:00:00+00:00",
        )
        .await;

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(covers_of(&tags, "A"), ["https://c/b.jpg"]);
        assert_eq!(covers_of(&tags, "B"), ["https://c/b.jpg"]);
    }

    #[tokio::test]
    async fn own_books_come_first_round_a_cycle_too() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let a = shelf(&db, "A", None).await;
        let b = shelf(&db, "B", Some(&a)).await;
        let mut active: tag::ActiveModel = tag::Entity::find_by_id(a.clone())
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .into();
        active.parent_id = Set(Some(b));
        active.update(&db).await.unwrap();
        let t = |d: u32| format!("2026-01-{d:02}T10:00:00+00:00");
        book(&db, &["A"], Some("https://c/a.jpg"), true, &t(1)).await;
        book(&db, &["B"], Some("https://c/b.jpg"), true, &t(2)).await;

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(
            covers_of(&tags, "A"),
            ["https://c/a.jpg", "https://c/b.jpg"]
        );
        assert_eq!(
            covers_of(&tags, "B"),
            ["https://c/b.jpg", "https://c/a.jpg"]
        );
    }

    // ---- Counts of a shelf with its sub-shelves: `subtree_*` ----

    fn subtree_of(tags: &[super::FrbTag], name: &str) -> (i64, i64) {
        let tag = tags
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} is listed"));
        (tag.subtree_count, tag.subtree_total_count)
    }

    #[tokio::test]
    async fn a_book_on_a_shelf_and_its_sub_shelf_counts_once_in_the_subtree() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let essay = shelf(&db, "Essay", None).await;
        let politics = shelf(&db, "Politics", Some(&essay)).await;
        shelf(&db, "Crime", Some(&politics)).await;
        let t = |d: u32| format!("2026-01-{d:02}T10:00:00+00:00");
        book(&db, &["Essay", "Politics"], None, true, &t(1)).await;
        book(&db, &["Politics"], None, true, &t(2)).await;
        book(&db, &["Crime"], None, true, &t(3)).await;

        let tags = get_all_tags_in(&db, None).await.unwrap();

        // Four shelf memberships, three books.
        assert_eq!(subtree_of(&tags, "Essay"), (3, 3));
        assert_eq!(subtree_of(&tags, "Politics"), (3, 3));
        assert_eq!(subtree_of(&tags, "Crime"), (1, 1));
    }

    #[tokio::test]
    async fn the_two_subtree_counts_follow_the_two_ownership_scopes() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let top = shelf(&db, "Top", None).await;
        shelf(&db, "Sub", Some(&top)).await;
        let t = |d: u32| format!("2026-01-{d:02}T10:00:00+00:00");
        book(&db, &["Sub"], None, true, &t(1)).await;
        // Wished, on the shelf and its sub-shelf: once, and only in the total.
        book(&db, &["Top", "Sub"], None, false, &t(2)).await;

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(subtree_of(&tags, "Top"), (1, 2));
        assert_eq!(subtree_of(&tags, "Sub"), (1, 2));
    }

    #[tokio::test]
    async fn count_and_total_count_keep_meaning_the_shelf_alone() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let top = shelf(&db, "Top", None).await;
        shelf(&db, "Sub", Some(&top)).await;
        let t = |d: u32| format!("2026-01-{d:02}T10:00:00+00:00");
        book(&db, &["Sub"], None, true, &t(1)).await;
        book(&db, &["Top"], None, false, &t(2)).await;

        let tags = get_all_tags_in(&db, None).await.unwrap();
        let top = tags.iter().find(|t| t.name == "Top").unwrap();

        assert_eq!((top.count, top.total_count), (0, 1));
        assert_eq!((top.subtree_count, top.subtree_total_count), (1, 2));
    }

    #[tokio::test]
    async fn a_subtree_round_a_cycle_counts_each_book_once() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let a = shelf(&db, "A", None).await;
        let b = shelf(&db, "B", Some(&a)).await;
        let mut active: tag::ActiveModel = tag::Entity::find_by_id(a.clone())
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .into();
        active.parent_id = Set(Some(b));
        active.update(&db).await.unwrap();
        let t = |d: u32| format!("2026-01-{d:02}T10:00:00+00:00");
        book(&db, &["A"], None, true, &t(1)).await;
        book(&db, &["B"], None, true, &t(2)).await;
        book(&db, &["A", "B"], None, true, &t(3)).await;

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(subtree_of(&tags, "A"), (3, 3));
        assert_eq!(subtree_of(&tags, "B"), (3, 3));
    }

    #[tokio::test]
    async fn a_name_known_only_from_the_books_has_its_own_books_as_subtree() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let t = |d: u32| format!("2026-01-{d:02}T10:00:00+00:00");
        book(&db, &["Loose"], None, true, &t(1)).await;
        book(&db, &["Loose"], None, false, &t(2)).await;

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(subtree_of(&tags, "Loose"), (1, 2));
    }

    #[tokio::test]
    async fn the_subtree_counts_read_the_books_only_once() {
        use std::sync::{Arc, Mutex};
        let mut db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let top = shelf(&db, "Top", None).await;
        shelf(&db, "Sub", Some(&top)).await;
        book(&db, &["Sub"], None, true, "2026-01-01T10:00:00+00:00").await;
        let statements: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = statements.clone();
        db.set_metric_callback(move |info| {
            seen.lock().unwrap().push(info.statement.sql.clone());
        });

        get_all_tags_in(&db, None).await.unwrap();

        let book_reads = statements
            .lock()
            .unwrap()
            .iter()
            .filter(|sql| sql.contains(r#"FROM "books""#))
            .count();
        assert_eq!(book_reads, 1);
    }

    #[tokio::test]
    async fn a_local_cover_missing_on_this_device_is_skipped() {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        shelf(&db, "Novels", None).await;
        let here = book(&db, &["Novels"], None, true, "2026-01-01T10:00:00+00:00").await;
        let gone = book(&db, &["Novels"], None, true, "2026-01-02T10:00:00+00:00").await;
        // Both rows name a local cover; only the first has its bytes here.
        // The stored prefix is a dead container: the path returned is the
        // one derived from the book's identity.
        for id in [&here, &gone] {
            use crate::models::book;
            let mut active: book::ActiveModel = book::Entity::find_by_id(id.clone())
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .into();
            active.cover_url = Set(Some(format!("/var/mobile/old/covers/{id}.jpg")));
            active.updated_at = Set(if id == &here {
                "2026-01-01T10:00:00+00:00".into()
            } else {
                "2026-01-02T10:00:00+00:00".into()
            });
            active.update(&db).await.unwrap();
        }
        std::fs::write(dir.path().join(format!("{here}.jpg")), b"jpg").unwrap();

        let tags = get_all_tags_in(&db, Some(dir.path())).await.unwrap();

        let expected = dir.path().join(format!("{here}.jpg"));
        assert_eq!(covers_of(&tags, "Novels"), [expected.to_str().unwrap()]);
    }

    #[tokio::test]
    async fn two_hundred_shelves_read_the_books_once() {
        use std::sync::{Arc, Mutex};
        let mut db = crate::db::init_db("sqlite::memory:").await.unwrap();
        let mut parent: Option<String> = None;
        for i in 0..200 {
            // Ten chains of twenty levels: deep enough to make a per-shelf
            // or per-level read show up.
            if i % 20 == 0 {
                parent = None;
            }
            let name = format!("Shelf {i}");
            let id = shelf(&db, &name, parent.as_deref()).await;
            let at = format!("2026-01-01T10:{:02}:{:02}+00:00", i / 60, i % 60);
            book(&db, &[&name], Some("https://c/x.jpg"), true, &at).await;
            parent = Some(id);
        }
        let statements: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = statements.clone();
        db.set_metric_callback(move |info| {
            seen.lock().unwrap().push(info.statement.sql.clone());
        });

        let tags = get_all_tags_in(&db, None).await.unwrap();

        assert_eq!(tags.len(), 200);
        // A leaf holds its own book, a shelf two levels up three.
        assert!(tags.iter().all(|t| (1..=3).contains(&t.cover_paths.len())));
        assert_eq!(covers_of(&tags, "Shelf 0").len(), 3);
        assert_eq!(covers_of(&tags, "Shelf 19").len(), 1);
        let book_reads = statements
            .lock()
            .unwrap()
            .iter()
            .filter(|sql| sql.contains(r#"FROM "books""#))
            .count();
        assert_eq!(book_reads, 1, "{:#?}", statements.lock().unwrap());
    }
}
