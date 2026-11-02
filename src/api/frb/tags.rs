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
}

/// Get all tags with hierarchy info
pub async fn get_all_tags() -> Result<Vec<FrbTag>, String> {
    let db = db().ok_or("Database not initialized")?;
    get_all_tags_in(db).await
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

/// [`get_all_tags`] over an explicit connection, so it can be tested without
/// the global FFI database. A flat list: the tree, and its tolerance of a
/// cycle merged from sync, are built by the client.
async fn get_all_tags_in(db: &sea_orm::DatabaseConnection) -> Result<Vec<FrbTag>, String> {
    // 1. Fetch hierarchical tags from DB
    use crate::models::tag;
    use sea_orm::{EntityTrait, QueryOrder};
    let db_tags = tag::Entity::find()
        .order_by_asc(tag::Column::Name)
        .all(db)
        .await
        .map_err(|e| format!("{:?}", e))?;

    // 2. Fetch counts from legacy book subjects (JSON), scoped to the default
    // library view so the badge agrees with the opened shelf (ADR-063). The
    // rule lives in `book_service::subject_counts`, shared with the HTTP path.
    let tag_counts = crate::services::book_service::subject_counts(db)
        .await
        .map_err(|e| format!("{:?}", e))?;

    // 3. Merge: Prioritize DB hierarchy, add legacy tags as roots if missing
    let mut result = Vec::new();
    let mut processed_names = std::collections::HashSet::new();

    // Add DB tags
    for t in db_tags {
        let counts = tag_counts.get(&t.name).copied().unwrap_or_default();
        processed_names.insert(t.name.clone());
        result.push(FrbTag {
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
            result.push(FrbTag {
                id: format!("legacy:{next_legacy_id}"),
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

        let Ok(tags) = get_all_tags_in(&db).await else {
            panic!("a cycle is not an error");
        };

        let ids: Vec<&str> = tags.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&a.as_str()) && ids.contains(&b.as_str()));
    }
}
