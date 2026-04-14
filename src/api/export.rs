use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use sea_orm::{DatabaseConnection, EntityTrait, Set, sea_query::OnConflict};
use serde::{Deserialize, Serialize};

use crate::models::{
    author, book, book_authors, book_tags, collection, collection_book, contact, copy,
    gamification_achievements, gamification_config, gamification_progress, gamification_streaks,
    library_config, loan, peer, sale, tag,
};

// --- Export ---

#[derive(Serialize)]
pub struct BackupData {
    pub version: String,
    pub exported_at: String,
    pub library_config: Option<library_config::Model>,
    pub books: Vec<book::Model>,
    pub authors: Vec<author::Model>,
    pub book_authors: Vec<book_authors::Model>,
    pub copies: Vec<copy::Model>,
    pub contacts: Vec<contact::Model>,
    pub loans: Vec<loan::Model>,
    pub sales: Vec<sale::Model>,
    pub tags: Vec<tag::Model>,
    pub book_tags: Vec<book_tags::Model>,
    pub collections: Vec<collection::Model>,
    pub collection_books: Vec<collection_book::Model>,
    pub peers: Vec<peer::Model>,
    pub gamification_config: Option<gamification_config::Model>,
    pub gamification_progress: Vec<gamification_progress::Model>,
    pub gamification_achievements: Vec<gamification_achievements::Model>,
    pub gamification_streaks: Vec<gamification_streaks::Model>,
}

pub async fn export_data(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let config = library_config::Entity::find_by_id(1)
        .one(&db)
        .await
        .unwrap_or(None);
    let books = book::Entity::find().all(&db).await.unwrap_or_default();
    let authors = author::Entity::find().all(&db).await.unwrap_or_default();
    let book_authors = book_authors::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();
    let copies = copy::Entity::find().all(&db).await.unwrap_or_default();
    let contacts = contact::Entity::find().all(&db).await.unwrap_or_default();
    let loans = loan::Entity::find().all(&db).await.unwrap_or_default();
    let sales = sale::Entity::find().all(&db).await.unwrap_or_default();
    let tags = tag::Entity::find().all(&db).await.unwrap_or_default();
    let book_tags = book_tags::Entity::find().all(&db).await.unwrap_or_default();
    let collections = collection::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();
    let collection_books = collection_book::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();
    let peers = peer::Entity::find().all(&db).await.unwrap_or_default();
    let gam_config = gamification_config::Entity::find()
        .one(&db)
        .await
        .unwrap_or(None);
    let gam_progress = gamification_progress::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();
    let gam_achievements = gamification_achievements::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();
    let gam_streaks = gamification_streaks::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();

    let backup = BackupData {
        version: "2.0".to_string(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        library_config: config,
        books,
        authors,
        book_authors,
        copies,
        contacts,
        loans,
        sales,
        tags,
        book_tags,
        collections,
        collection_books,
        peers,
        gamification_config: gam_config,
        gamification_progress: gam_progress,
        gamification_achievements: gam_achievements,
        gamification_streaks: gam_streaks,
    };

    let filename = format!(
        "bibliogenius_backup_{}.json",
        chrono::Utc::now().format("%Y-%m-%d")
    );

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{}\"", filename)
            .parse()
            .unwrap(),
    );

    (StatusCode::OK, headers, Json(backup))
}

// --- Import ---

/// Flexible book type that accepts both the full Model format and the simplified
/// FFI export format (with author field, subjects as array, missing timestamps).
#[derive(Deserialize)]
pub struct ImportBook {
    pub id: Option<i32>,
    pub title: String,
    pub isbn: Option<String>,
    pub summary: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
    pub dewey_decimal: Option<String>,
    pub lcc: Option<String>,
    /// Accepts both a JSON string (full export) and an array (FFI export)
    #[serde(default, deserialize_with = "deserialize_subjects")]
    pub subjects: Option<String>,
    pub marc_record: Option<String>,
    pub cataloguing_notes: Option<String>,
    pub source_data: Option<String>,
    pub shelf_position: Option<i32>,
    #[serde(default = "default_reading_status")]
    pub reading_status: String,
    pub finished_reading_at: Option<String>,
    pub started_reading_at: Option<String>,
    pub cover_url: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub user_rating: Option<i32>,
    #[serde(default = "default_true")]
    pub owned: bool,
    pub price: Option<f64>,
    pub digital_formats: Option<String>,
    #[serde(default)]
    pub private: bool,
    pub page_count: Option<i32>,
    pub loan_duration_days: Option<i32>,
    // Ignored fields from simplified format
    #[serde(default)]
    pub author: Option<String>,
}

/// Flexible contact type that accepts both formats.
#[derive(Deserialize)]
pub struct ImportContact {
    pub id: Option<i32>,
    #[serde(default = "default_contact_type")]
    pub r#type: String,
    pub name: String,
    pub first_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub address: Option<String>,
    pub street_address: Option<String>,
    pub postal_code: Option<String>,
    pub city: Option<String>,
    pub country: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub notes: Option<String>,
    pub user_id: Option<i32>,
    #[serde(default = "default_one")]
    pub library_owner_id: i32,
    #[serde(default = "default_true")]
    pub is_active: bool,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Flexible tag type that accepts both formats.
#[derive(Deserialize)]
pub struct ImportTag {
    pub id: Option<i32>,
    pub name: String,
    pub parent_id: Option<i32>,
    #[serde(default)]
    pub path: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Deserialize)]
pub struct ImportBackupData {
    pub version: Option<String>,
    #[serde(alias = "timestamp")]
    pub exported_at: Option<String>,
    pub library_config: Option<library_config::Model>,
    pub books: Option<Vec<ImportBook>>,
    pub authors: Option<Vec<author::Model>>,
    pub book_authors: Option<Vec<book_authors::Model>>,
    pub copies: Option<Vec<copy::Model>>,
    pub contacts: Option<Vec<ImportContact>>,
    pub loans: Option<Vec<loan::Model>>,
    pub sales: Option<Vec<sale::Model>>,
    pub tags: Option<Vec<ImportTag>>,
    pub book_tags: Option<Vec<book_tags::Model>>,
    pub collections: Option<Vec<collection::Model>>,
    pub collection_books: Option<Vec<collection_book::Model>>,
    pub peers: Option<Vec<peer::Model>>,
    pub gamification_config: Option<gamification_config::Model>,
    pub gamification_progress: Option<Vec<gamification_progress::Model>>,
    pub gamification_achievements: Option<Vec<gamification_achievements::Model>>,
    pub gamification_streaks: Option<Vec<gamification_streaks::Model>>,
}

#[derive(Serialize)]
pub struct ImportResult {
    pub success: bool,
    pub books_imported: usize,
    pub copies_imported: usize,
    pub contacts_imported: usize,
    pub loans_imported: usize,
    pub tags_imported: usize,
    pub authors_imported: usize,
    pub collections_imported: usize,
    pub peers_imported: usize,
    pub sales_imported: usize,
    pub gamification_imported: usize,
    pub message: String,
}

pub async fn import_data(
    State(db): State<DatabaseConnection>,
    Json(backup): Json<ImportBackupData>,
) -> impl IntoResponse {
    use sea_orm::{ActiveModelTrait, IntoActiveModel};

    let now = chrono::Utc::now().to_rfc3339();
    let mut books_count = 0;
    let mut copies_count = 0;
    let mut contacts_count = 0;
    let mut loans_count = 0;
    let mut tags_count = 0;
    let mut authors_count = 0;
    let mut collections_count = 0;
    let mut peers_count = 0;
    let mut sales_count = 0;
    let mut gamification_count = 0;

    // 1. Import authors (no FK deps)
    if let Some(authors) = backup.authors {
        for a in authors {
            let active = a.into_active_model();
            if active.insert(&db).await.is_ok() {
                authors_count += 1;
            }
        }
    }

    // 2. Import books
    if let Some(books) = backup.books {
        for b in books {
            let active = book::ActiveModel {
                id: b.id.map_or(sea_orm::ActiveValue::NotSet, Set),
                title: Set(b.title),
                isbn: Set(b.isbn),
                summary: Set(b.summary),
                publisher: Set(b.publisher),
                publication_year: Set(b.publication_year),
                dewey_decimal: Set(b.dewey_decimal),
                lcc: Set(b.lcc),
                subjects: Set(b.subjects),
                marc_record: Set(b.marc_record),
                cataloguing_notes: Set(b.cataloguing_notes),
                source_data: Set(b.source_data),
                shelf_position: Set(b.shelf_position),
                reading_status: Set(b.reading_status),
                finished_reading_at: Set(b.finished_reading_at),
                started_reading_at: Set(b.started_reading_at),
                cover_url: Set(b.cover_url),
                created_at: Set(b.created_at.unwrap_or_else(|| now.clone())),
                updated_at: Set(b.updated_at.unwrap_or_else(|| now.clone())),
                user_rating: Set(b.user_rating),
                owned: Set(b.owned),
                price: Set(b.price),
                digital_formats: Set(b.digital_formats),
                private: Set(b.private),
                page_count: Set(b.page_count),
                loan_duration_days: Set(b.loan_duration_days),
            };
            if active.insert(&db).await.is_ok() {
                books_count += 1;
            }
        }
    }

    // 3. Import book-authors (FK to books, authors)
    if let Some(bas) = backup.book_authors {
        for ba in bas {
            let active = ba.into_active_model();
            if active.insert(&db).await.is_ok() {
                // counted with authors
            }
        }
    }

    // 4. Import tags
    if let Some(tags) = backup.tags {
        for t in tags {
            let active = tag::ActiveModel {
                id: t.id.map_or(sea_orm::ActiveValue::NotSet, Set),
                name: Set(t.name),
                parent_id: Set(t.parent_id),
                path: Set(t.path),
                created_at: Set(t.created_at.unwrap_or_else(|| now.clone())),
                updated_at: Set(t.updated_at.unwrap_or_else(|| now.clone())),
            };
            if active.insert(&db).await.is_ok() {
                tags_count += 1;
            }
        }
    }

    // 5. Import book-tags (FK to books, tags)
    if let Some(bts) = backup.book_tags {
        for bt in bts {
            let active = bt.into_active_model();
            if active.insert(&db).await.is_ok() {
                // counted with tags
            }
        }
    }

    // 6. Import contacts
    if let Some(contacts) = backup.contacts {
        for c in contacts {
            let active = contact::ActiveModel {
                id: c.id.map_or(sea_orm::ActiveValue::NotSet, Set),
                r#type: Set(c.r#type),
                name: Set(c.name),
                first_name: Set(c.first_name),
                email: Set(c.email),
                phone: Set(c.phone),
                address: Set(c.address),
                street_address: Set(c.street_address),
                postal_code: Set(c.postal_code),
                city: Set(c.city),
                country: Set(c.country),
                latitude: Set(c.latitude),
                longitude: Set(c.longitude),
                notes: Set(c.notes),
                user_id: Set(c.user_id),
                library_owner_id: Set(c.library_owner_id),
                is_active: Set(c.is_active),
                created_at: Set(c.created_at.unwrap_or_else(|| now.clone())),
                updated_at: Set(c.updated_at.unwrap_or_else(|| now.clone())),
            };
            if active.insert(&db).await.is_ok() {
                contacts_count += 1;
            }
        }
    }

    // 7. Import copies (FK to books, libraries)
    if let Some(copies) = backup.copies {
        for c in copies {
            let active = c.into_active_model();
            if active.insert(&db).await.is_ok() {
                copies_count += 1;
            }
        }
    }

    // 8. Import loans (FK to copies, contacts)
    if let Some(loans) = backup.loans {
        for l in loans {
            let active = l.into_active_model();
            if active.insert(&db).await.is_ok() {
                loans_count += 1;
            }
        }
    }

    // 9. Import sales (FK to copies, contacts)
    if let Some(sales) = backup.sales {
        for s in sales {
            let active = s.into_active_model();
            if active.insert(&db).await.is_ok() {
                sales_count += 1;
            }
        }
    }

    // 10. Import collections
    if let Some(collections) = backup.collections {
        for c in collections {
            let active = c.into_active_model();
            if active.insert(&db).await.is_ok() {
                collections_count += 1;
            }
        }
    }

    // 11. Import collection-books (FK to collections, books)
    if let Some(cbs) = backup.collection_books {
        for cb in cbs {
            let active = cb.into_active_model();
            if active.insert(&db).await.is_ok() {
                // counted with collections
            }
        }
    }

    // 12. Import peers (reset E2EE state -- new install has a new identity,
    //     peers will need to re-exchange keys on next contact)
    if let Some(peers) = backup.peers {
        for p in peers {
            let active = peer::ActiveModel {
                id: sea_orm::ActiveValue::NotSet,
                name: Set(p.name),
                display_name: Set(p.display_name),
                url: Set(p.url),
                library_uuid: Set(p.library_uuid),
                public_key: Set(None),
                x25519_public_key: Set(None),
                key_exchange_done: Set(false),
                mailbox_id: Set(p.mailbox_id),
                relay_url: Set(p.relay_url),
                relay_write_token: Set(None),
                latitude: Set(p.latitude),
                longitude: Set(p.longitude),
                auto_approve: Set(p.auto_approve),
                connection_status: Set("pending".to_string()),
                last_seen: Set(None),
                avatar_config: Set(None),
                catalog_hash: Set(None),
                last_catalog_sync: Set(None),
                last_delta_cursor: Set(None),
                created_at: Set(p.created_at),
                updated_at: Set(now.clone()),
            };
            if active.insert(&db).await.is_ok() {
                peers_count += 1;
            }
        }
    }

    // 13. Import gamification
    if let Some(gc) = backup.gamification_config {
        let active = gc.into_active_model();
        if active.insert(&db).await.is_ok() {
            gamification_count += 1;
        }
    }

    if let Some(gps) = backup.gamification_progress {
        for gp in gps {
            let active = gp.into_active_model();
            if active.insert(&db).await.is_ok() {
                gamification_count += 1;
            }
        }
    }

    if let Some(gas) = backup.gamification_achievements {
        for ga in gas {
            let active = ga.into_active_model();
            if active.insert(&db).await.is_ok() {
                gamification_count += 1;
            }
        }
    }

    if let Some(gss) = backup.gamification_streaks {
        for gs in gss {
            let active = gs.into_active_model();
            if active.insert(&db).await.is_ok() {
                gamification_count += 1;
            }
        }
    }

    // 14. Import library config (upsert)
    if let Some(lc) = backup.library_config {
        let active = lc.into_active_model();
        // Try insert first, ignore if already exists (id=1)
        let _ = active.insert(&db).await;
    }

    let total = books_count
        + copies_count
        + contacts_count
        + loans_count
        + tags_count
        + authors_count
        + collections_count
        + peers_count
        + sales_count
        + gamification_count;

    let result = ImportResult {
        success: true,
        books_imported: books_count,
        copies_imported: copies_count,
        contacts_imported: contacts_count,
        loans_imported: loans_count,
        tags_imported: tags_count,
        authors_imported: authors_count,
        collections_imported: collections_count,
        peers_imported: peers_count,
        sales_imported: sales_count,
        gamification_imported: gamification_count,
        message: format!("Successfully imported {} items", total),
    };

    (StatusCode::OK, Json(result))
}

// --- Upsert Import (for auto-backup sync) ---

pub async fn import_data_upsert(
    State(db): State<DatabaseConnection>,
    Json(backup): Json<ImportBackupData>,
) -> impl IntoResponse {
    use sea_orm::IntoActiveModel;

    let now = chrono::Utc::now().to_rfc3339();
    let mut books_count = 0;
    let mut copies_count = 0;
    let mut contacts_count = 0;
    let mut loans_count = 0;
    let mut tags_count = 0;
    let mut authors_count = 0;
    let mut collections_count = 0;
    let mut sales_count = 0;
    let mut gamification_count = 0;

    // 1. Upsert authors (no FK deps)
    if let Some(authors) = backup.authors {
        for a in authors {
            let res = author::Entity::insert(a.into_active_model())
                .on_conflict(
                    OnConflict::column(author::Column::Id)
                        .update_column(author::Column::Name)
                        .to_owned(),
                )
                .exec(&db)
                .await;
            if res.is_ok() {
                authors_count += 1;
            }
        }
    }

    // 2. Upsert books
    if let Some(books) = backup.books {
        for b in books {
            let active = book::ActiveModel {
                id: b.id.map_or(sea_orm::ActiveValue::NotSet, Set),
                title: Set(b.title),
                isbn: Set(b.isbn),
                summary: Set(b.summary),
                publisher: Set(b.publisher),
                publication_year: Set(b.publication_year),
                dewey_decimal: Set(b.dewey_decimal),
                lcc: Set(b.lcc),
                subjects: Set(b.subjects),
                marc_record: Set(b.marc_record),
                cataloguing_notes: Set(b.cataloguing_notes),
                source_data: Set(b.source_data),
                shelf_position: Set(b.shelf_position),
                reading_status: Set(b.reading_status),
                finished_reading_at: Set(b.finished_reading_at),
                started_reading_at: Set(b.started_reading_at),
                cover_url: Set(b.cover_url),
                created_at: Set(b.created_at.unwrap_or_else(|| now.clone())),
                updated_at: Set(b.updated_at.unwrap_or_else(|| now.clone())),
                user_rating: Set(b.user_rating),
                owned: Set(b.owned),
                price: Set(b.price),
                digital_formats: Set(b.digital_formats),
                private: Set(b.private),
                page_count: Set(b.page_count),
                loan_duration_days: Set(b.loan_duration_days),
            };
            let res = book::Entity::insert(active)
                .on_conflict(
                    OnConflict::column(book::Column::Id)
                        .update_columns([
                            book::Column::Title,
                            book::Column::Isbn,
                            book::Column::Summary,
                            book::Column::Publisher,
                            book::Column::PublicationYear,
                            book::Column::DeweyDecimal,
                            book::Column::Lcc,
                            book::Column::Subjects,
                            book::Column::MarcRecord,
                            book::Column::CataloguingNotes,
                            book::Column::SourceData,
                            book::Column::ShelfPosition,
                            book::Column::ReadingStatus,
                            book::Column::FinishedReadingAt,
                            book::Column::StartedReadingAt,
                            book::Column::CoverUrl,
                            book::Column::UpdatedAt,
                            book::Column::UserRating,
                            book::Column::Owned,
                            book::Column::Price,
                            book::Column::DigitalFormats,
                        ])
                        .to_owned(),
                )
                .exec(&db)
                .await;
            if res.is_ok() {
                books_count += 1;
            }
        }
    }

    // 3. Book-authors junction: INSERT OR IGNORE
    if let Some(bas) = backup.book_authors {
        for ba in bas {
            let _ = book_authors::Entity::insert(ba.into_active_model())
                .on_conflict(
                    OnConflict::columns([
                        book_authors::Column::BookId,
                        book_authors::Column::AuthorId,
                    ])
                    .do_nothing()
                    .to_owned(),
                )
                .do_nothing()
                .exec(&db)
                .await;
        }
    }

    // 4. Upsert tags
    if let Some(tags) = backup.tags {
        for t in tags {
            let active = tag::ActiveModel {
                id: t.id.map_or(sea_orm::ActiveValue::NotSet, Set),
                name: Set(t.name),
                parent_id: Set(t.parent_id),
                path: Set(t.path),
                created_at: Set(t.created_at.unwrap_or_else(|| now.clone())),
                updated_at: Set(t.updated_at.unwrap_or_else(|| now.clone())),
            };
            let res = tag::Entity::insert(active)
                .on_conflict(
                    OnConflict::column(tag::Column::Id)
                        .update_columns([
                            tag::Column::Name,
                            tag::Column::ParentId,
                            tag::Column::Path,
                        ])
                        .to_owned(),
                )
                .exec(&db)
                .await;
            if res.is_ok() {
                tags_count += 1;
            }
        }
    }

    // 5. Book-tags junction: INSERT OR IGNORE
    if let Some(bts) = backup.book_tags {
        for bt in bts {
            let _ = book_tags::Entity::insert(bt.into_active_model())
                .on_conflict(
                    OnConflict::columns([book_tags::Column::BookId, book_tags::Column::TagId])
                        .do_nothing()
                        .to_owned(),
                )
                .do_nothing()
                .exec(&db)
                .await;
        }
    }

    // 6. Upsert contacts
    if let Some(contacts) = backup.contacts {
        for c in contacts {
            let active = contact::ActiveModel {
                id: c.id.map_or(sea_orm::ActiveValue::NotSet, Set),
                r#type: Set(c.r#type),
                name: Set(c.name),
                first_name: Set(c.first_name),
                email: Set(c.email),
                phone: Set(c.phone),
                address: Set(c.address),
                street_address: Set(c.street_address),
                postal_code: Set(c.postal_code),
                city: Set(c.city),
                country: Set(c.country),
                latitude: Set(c.latitude),
                longitude: Set(c.longitude),
                notes: Set(c.notes),
                user_id: Set(c.user_id),
                library_owner_id: Set(c.library_owner_id),
                is_active: Set(c.is_active),
                created_at: Set(c.created_at.unwrap_or_else(|| now.clone())),
                updated_at: Set(c.updated_at.unwrap_or_else(|| now.clone())),
            };
            let res = contact::Entity::insert(active)
                .on_conflict(
                    OnConflict::column(contact::Column::Id)
                        .update_columns([
                            contact::Column::Type,
                            contact::Column::Name,
                            contact::Column::FirstName,
                            contact::Column::Email,
                            contact::Column::Phone,
                            contact::Column::Address,
                            contact::Column::StreetAddress,
                            contact::Column::PostalCode,
                            contact::Column::City,
                            contact::Column::Country,
                            contact::Column::Latitude,
                            contact::Column::Longitude,
                            contact::Column::Notes,
                            contact::Column::UserId,
                            contact::Column::LibraryOwnerId,
                            contact::Column::IsActive,
                            contact::Column::UpdatedAt,
                        ])
                        .to_owned(),
                )
                .exec(&db)
                .await;
            if res.is_ok() {
                contacts_count += 1;
            }
        }
    }

    // 7. Upsert copies
    if let Some(copies) = backup.copies {
        for c in copies {
            let res = copy::Entity::insert(c.into_active_model())
                .on_conflict(
                    OnConflict::column(copy::Column::Id)
                        .update_columns([
                            copy::Column::BookId,
                            copy::Column::LibraryId,
                            copy::Column::AcquisitionDate,
                            copy::Column::Notes,
                            copy::Column::Status,
                            copy::Column::IsTemporary,
                            copy::Column::UpdatedAt,
                            copy::Column::SoldAt,
                            copy::Column::Price,
                        ])
                        .to_owned(),
                )
                .exec(&db)
                .await;
            if res.is_ok() {
                copies_count += 1;
            }
        }
    }

    // 8. Upsert loans
    if let Some(loans) = backup.loans {
        for l in loans {
            let res = loan::Entity::insert(l.into_active_model())
                .on_conflict(
                    OnConflict::column(loan::Column::Id)
                        .update_columns([
                            loan::Column::CopyId,
                            loan::Column::ContactId,
                            loan::Column::LibraryId,
                            loan::Column::LoanDate,
                            loan::Column::DueDate,
                            loan::Column::ReturnDate,
                            loan::Column::Status,
                            loan::Column::Notes,
                            loan::Column::UpdatedAt,
                        ])
                        .to_owned(),
                )
                .exec(&db)
                .await;
            if res.is_ok() {
                loans_count += 1;
            }
        }
    }

    // 9. Upsert sales
    if let Some(sales) = backup.sales {
        for s in sales {
            let res = sale::Entity::insert(s.into_active_model())
                .on_conflict(
                    OnConflict::column(sale::Column::Id)
                        .update_columns([
                            sale::Column::CopyId,
                            sale::Column::ContactId,
                            sale::Column::LibraryId,
                            sale::Column::SaleDate,
                            sale::Column::SalePrice,
                            sale::Column::Status,
                            sale::Column::Notes,
                            sale::Column::UpdatedAt,
                        ])
                        .to_owned(),
                )
                .exec(&db)
                .await;
            if res.is_ok() {
                sales_count += 1;
            }
        }
    }

    // 10. Upsert collections
    if let Some(collections) = backup.collections {
        for c in collections {
            let res = collection::Entity::insert(c.into_active_model())
                .on_conflict(
                    OnConflict::column(collection::Column::Id)
                        .update_columns([
                            collection::Column::Name,
                            collection::Column::Description,
                            collection::Column::Source,
                        ])
                        .to_owned(),
                )
                .exec(&db)
                .await;
            if res.is_ok() {
                collections_count += 1;
            }
        }
    }

    // 11. Collection-books junction: INSERT OR IGNORE
    if let Some(cbs) = backup.collection_books {
        for cb in cbs {
            let _ = collection_book::Entity::insert(cb.into_active_model())
                .on_conflict(
                    OnConflict::columns([
                        collection_book::Column::CollectionId,
                        collection_book::Column::BookId,
                    ])
                    .do_nothing()
                    .to_owned(),
                )
                .do_nothing()
                .exec(&db)
                .await;
        }
    }

    // 12. Skip peers (local network config, not relevant for backup)

    // 13. Upsert gamification
    if let Some(gc) = backup.gamification_config {
        let res = gamification_config::Entity::insert(gc.into_active_model())
            .on_conflict(
                OnConflict::column(gamification_config::Column::Id)
                    .update_columns([
                        gamification_config::Column::UserId,
                        gamification_config::Column::Preset,
                        gamification_config::Column::StreaksEnabled,
                        gamification_config::Column::AchievementsEnabled,
                        gamification_config::Column::AchievementsStyle,
                        gamification_config::Column::ReadingGoalsEnabled,
                        gamification_config::Column::ReadingGoalYearly,
                        gamification_config::Column::TracksEnabled,
                        gamification_config::Column::NotificationsEnabled,
                        gamification_config::Column::UpdatedAt,
                    ])
                    .to_owned(),
            )
            .exec(&db)
            .await;
        if res.is_ok() {
            gamification_count += 1;
        }
    }

    if let Some(gps) = backup.gamification_progress {
        for gp in gps {
            let res = gamification_progress::Entity::insert(gp.into_active_model())
                .on_conflict(
                    OnConflict::column(gamification_progress::Column::Id)
                        .update_columns([
                            gamification_progress::Column::UserId,
                            gamification_progress::Column::Track,
                            gamification_progress::Column::CurrentValue,
                            gamification_progress::Column::Level,
                            gamification_progress::Column::UpdatedAt,
                        ])
                        .to_owned(),
                )
                .exec(&db)
                .await;
            if res.is_ok() {
                gamification_count += 1;
            }
        }
    }

    if let Some(gas) = backup.gamification_achievements {
        for ga in gas {
            let res = gamification_achievements::Entity::insert(ga.into_active_model())
                .on_conflict(
                    OnConflict::column(gamification_achievements::Column::Id)
                        .update_columns([
                            gamification_achievements::Column::UserId,
                            gamification_achievements::Column::AchievementId,
                            gamification_achievements::Column::UnlockedAt,
                        ])
                        .to_owned(),
                )
                .exec(&db)
                .await;
            if res.is_ok() {
                gamification_count += 1;
            }
        }
    }

    if let Some(gss) = backup.gamification_streaks {
        for gs in gss {
            let res = gamification_streaks::Entity::insert(gs.into_active_model())
                .on_conflict(
                    OnConflict::column(gamification_streaks::Column::Id)
                        .update_columns([
                            gamification_streaks::Column::UserId,
                            gamification_streaks::Column::CurrentStreak,
                            gamification_streaks::Column::LongestStreak,
                            gamification_streaks::Column::LastActivityDate,
                        ])
                        .to_owned(),
                )
                .exec(&db)
                .await;
            if res.is_ok() {
                gamification_count += 1;
            }
        }
    }

    // 14. Upsert library config
    if let Some(lc) = backup.library_config {
        let _ = library_config::Entity::insert(lc.into_active_model())
            .on_conflict(
                OnConflict::column(library_config::Column::Id)
                    .update_columns([
                        library_config::Column::Name,
                        library_config::Column::Description,
                        library_config::Column::Tags,
                        library_config::Column::Latitude,
                        library_config::Column::Longitude,
                        library_config::Column::ShareLocation,
                        library_config::Column::ShowBorrowedBooks,
                        library_config::Column::UpdatedAt,
                    ])
                    .to_owned(),
            )
            .exec(&db)
            .await;
    }

    let total = books_count
        + copies_count
        + contacts_count
        + loans_count
        + tags_count
        + authors_count
        + collections_count
        + sales_count
        + gamification_count;

    let result = ImportResult {
        success: true,
        books_imported: books_count,
        copies_imported: copies_count,
        contacts_imported: contacts_count,
        loans_imported: loans_count,
        tags_imported: tags_count,
        authors_imported: authors_count,
        collections_imported: collections_count,
        peers_imported: 0,
        sales_imported: sales_count,
        gamification_imported: gamification_count,
        message: format!("Successfully upserted {} items", total),
    };

    (StatusCode::OK, Json(result))
}

// --- Helpers ---

fn default_reading_status() -> String {
    "to_read".to_string()
}

fn default_contact_type() -> String {
    "Person".to_string()
}

fn default_true() -> bool {
    true
}

fn default_one() -> i32 {
    1
}

/// Deserialize `subjects` from either a JSON string or a JSON array.
/// - Full export: `"subjects": "[\"tag1\",\"tag2\"]"` (string)
/// - FFI export: `"subjects": ["tag1", "tag2"]` (array)
/// - Both: `"subjects": null` (null)
fn deserialize_subjects<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(serde_json::Value::Array(arr)) => {
            // Convert array to JSON string for storage
            Ok(Some(serde_json::to_string(&arr).unwrap_or_default()))
        }
        Some(other) => Ok(Some(other.to_string())),
    }
}
