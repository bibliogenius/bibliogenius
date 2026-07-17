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
    pub count: i64,
}

/// Get all tags with hierarchy info
pub async fn get_all_tags() -> Result<Vec<FrbTag>, String> {
    let db = db().ok_or("Database not initialized")?;

    // 1. Fetch hierarchical tags from DB
    use crate::models::tag;
    use sea_orm::{EntityTrait, QueryOrder};
    let db_tags = tag::Entity::find()
        .order_by_asc(tag::Column::Name)
        .all(db)
        .await
        .map_err(|e| format!("{:?}", e))?;

    // 2. Fetch counts from legacy book subjects (JSON)
    // We reuse the logic from `list_tags` because `book_tags` table might be empty
    let books = crate::models::book::Entity::find()
        .all(db)
        .await
        .map_err(|e| format!("{:?}", e))?;

    let mut tag_counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for book in books {
        if let Some(subjects_json) = book.subjects
            && let Ok(subjects) = serde_json::from_str::<Vec<String>>(&subjects_json)
        {
            for subject in subjects {
                if !subject.trim().is_empty() {
                    *tag_counts.entry(subject.trim().to_string()).or_insert(0) += 1;
                }
            }
        }
    }

    // 3. Merge: Prioritize DB hierarchy, add legacy tags as roots if missing
    let mut result = Vec::new();
    let mut processed_names = std::collections::HashSet::new();

    // Add DB tags
    for t in db_tags {
        let count = *tag_counts.get(&t.name).unwrap_or(&0);
        processed_names.insert(t.name.clone());
        result.push(FrbTag {
            id: t.id,
            name: t.name,
            parent_id: t.parent_id,
            count,
        });
    }

    // Add remaining legacy tags (as orphans)
    // Give them synthetic "legacy:" string ids to distinguish from DB tags (uuids).
    let mut next_legacy_id = -1;
    for (name, count) in tag_counts {
        if !processed_names.contains(&name) {
            result.push(FrbTag {
                id: format!("legacy:{next_legacy_id}"),
                name,
                parent_id: None,
                count,
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
    use crate::models::tag;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};

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
    use sea_orm::TransactionTrait;

    // Cascade the tag's book links and re-parent its children in one
    // transaction: the database no longer cascades these since the replicated
    // tables lost their foreign keys (ADR-044).
    let txn = db.begin().await.map_err(|e| format!("{e:?}"))?;
    crate::infrastructure::referential_integrity::delete_tag_cascade(&txn, &id)
        .await
        .map_err(|e| format!("{e:?}"))?;
    txn.commit().await.map_err(|e| format!("{e:?}"))?;

    let _ = crate::sync::log_operation(db, "tag", &id, "DELETE", None).await;
    Ok(())
}
