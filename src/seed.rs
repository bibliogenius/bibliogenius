use crate::auth::hash_password;
use crate::models::{author, book, library, tag, user};
use sea_orm::*;

pub async fn seed_demo_data(db: &DatabaseConnection) -> Result<(), DbErr> {
    // 1. Create Users
    let admin_password = hash_password("admin").unwrap();
    let user_password = hash_password("user").unwrap();

    let admin = user::ActiveModel {
        username: Set("admin".to_owned()),
        password_hash: Set(admin_password),
        role: Set("admin".to_owned()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    let normal_user = user::ActiveModel {
        username: Set("user".to_owned()),
        password_hash: Set(user_password),
        role: Set("user".to_owned()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    user::Entity::insert(admin).on_conflict(
        sea_orm::sea_query::OnConflict::column(user::Column::Username)
            .do_nothing()
            .to_owned()
    ).exec(db).await?;

    user::Entity::insert(normal_user).on_conflict(
        sea_orm::sea_query::OnConflict::column(user::Column::Username)
            .do_nothing()
            .to_owned()
    ).exec(db).await?;

    // 1.5. Create Default Library for admin
    let default_library = library::ActiveModel {
        name: Set("My Library".to_owned()),
        description: Set(Some("Default personal library".to_owned())),
        owner_id: Set(1), // admin user
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    library::Entity::insert(default_library).on_conflict(
        sea_orm::sea_query::OnConflict::column(library::Column::Id)
            .do_nothing()
            .to_owned()
    ).exec(db).await?;

    // 2. Create Authors
    let authors = vec!["J.R.R. Tolkien", "Isaac Asimov", "Frank Herbert"];

    for name in authors {
        let author = author::ActiveModel {
            name: Set(name.to_owned()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let res = author::Entity::insert(author).exec(db).await;
        if let Ok(_res) = res {
            // author_ids.push(res.last_insert_id);
        } else {
             // Handle existing authors if needed, for now just skip or find
             // Simplified: assume empty DB for seed or ignore errors
        }
    }

    // 3. Create Tags
    let tags = vec!["Fantasy", "Sci-Fi", "Classic"];

    for name in tags {
        let tag = tag::ActiveModel {
            name: Set(name.to_owned()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
         let res = tag::Entity::insert(tag).on_conflict(
            sea_orm::sea_query::OnConflict::column(tag::Column::Name)
                .do_nothing()
                .to_owned()
        ).exec(db).await;
        
        // Retrieve ID (simplified, in real app we'd query back if exists)
    }

    // 4. Create Books (Simplified)
    let book = book::ActiveModel {
        title: Set("Dune".to_owned()),
        isbn: Set(Some("978-0441172719".to_owned())),
        summary: Set(Some("A spice planet story.".to_owned())),
        publisher: Set(Some("Ace Books".to_owned())),
        publication_year: Set(Some(1965)),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    
    book::Entity::insert(book).exec(db).await?;

    Ok(())
}
