#![allow(clippy::needless_update)] // SeaORM ActiveModels require ..Default::default()
//! Loans and sales name the contact by full name ("First Last"), the same way
//! the app's `Contact.fullName` does, and fall back to the last name alone when
//! the contact has no first name.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::{get, put},
};
use rust_lib_app::api::loan;
use rust_lib_app::db;
use rust_lib_app::domain::NotificationEventType;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::copy;
use rust_lib_app::models::loan::LoanDto;
use rust_lib_app::models::notification::{self, Entity as Notification};
use rust_lib_app::models::sale::SaleDto;
use rust_lib_app::services::{loan_service, sale_service};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use tower::util::ServiceExt;

struct Seed {
    library_id: i32,
    book_id: String,
}

async fn seed(db: &DatabaseConnection) -> Seed {
    let now = chrono::Utc::now().to_rfc3339();
    let user = rust_lib_app::models::user::ActiveModel {
        username: Set("admin".to_string()),
        password_hash: Set("hash".to_string()),
        role: Set("admin".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    let library = rust_lib_app::models::library::ActiveModel {
        name: Set("Lib".to_string()),
        owner_id: Set(user.id),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set("Test Book".to_string()),
        owned: Set(true),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    Seed {
        library_id: library.id,
        book_id: book.id,
    }
}

async fn insert_contact(
    db: &DatabaseConnection,
    library_id: i32,
    first_name: Option<&str>,
    name: &str,
) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    rust_lib_app::models::contact::ActiveModel {
        r#type: Set("person".to_string()),
        name: Set(name.to_string()),
        first_name: Set(first_name.map(str::to_string)),
        library_owner_id: Set(library_id),
        is_active: Set(true),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap()
    .id
}

async fn insert_copy(db: &DatabaseConnection, seed: &Seed) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    copy::ActiveModel {
        book_id: Set(seed.book_id.clone()),
        library_id: Set(seed.library_id),
        status: Set("available".to_string()),
        is_temporary: Set(false),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap()
    .id
}

fn loan_dto(copy_id: String, contact_id: String, library_id: i32) -> LoanDto {
    LoanDto {
        id: None,
        copy_id,
        contact_id,
        library_id,
        loan_date: "2026-01-01".to_string(),
        due_date: "2026-02-01".to_string(),
        return_date: None,
        status: None,
        notes: None,
    }
}

async fn setup() -> (DatabaseConnection, Seed) {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB");
    let seed = seed(&db).await;
    (db, seed)
}

#[tokio::test]
async fn loan_service_lists_the_contact_full_name() {
    let (db, seed) = setup().await;
    let jeanne = insert_contact(&db, seed.library_id, Some("Jeanne"), "Dupont").await;
    let copy_id = insert_copy(&db, &seed).await;
    loan_service::create_loan(&db, loan_dto(copy_id, jeanne, seed.library_id))
        .await
        .unwrap();

    let loans = loan_service::list_loans(&db, Default::default())
        .await
        .unwrap();

    assert_eq!(loans.len(), 1);
    assert_eq!(loans[0].contact_name, "Jeanne Dupont");
}

#[tokio::test]
async fn loan_service_falls_back_to_the_last_name_without_a_first_name() {
    let (db, seed) = setup().await;
    let alice = insert_contact(&db, seed.library_id, None, "Alice").await;
    let copy_id = insert_copy(&db, &seed).await;
    loan_service::create_loan(&db, loan_dto(copy_id, alice, seed.library_id))
        .await
        .unwrap();

    let loans = loan_service::list_loans(&db, Default::default())
        .await
        .unwrap();

    assert_eq!(loans[0].contact_name, "Alice");
}

fn loan_router() -> Router<AppState> {
    Router::new()
        .route("/loans", get(loan::list_loans).post(loan::create_loan))
        .route("/loans/:id/return", put(loan::return_loan))
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn http_loan_list_and_return_notification_use_the_full_name() {
    let (db, seed) = setup().await;
    let jeanne = insert_contact(&db, seed.library_id, Some("Jeanne"), "Dupont").await;
    let copy_id = insert_copy(&db, &seed).await;
    let loan = loan_service::create_loan(&db, loan_dto(copy_id, jeanne, seed.library_id))
        .await
        .unwrap();

    let app = loan_router().with_state(AppState::new(db.clone()));

    let resp = app
        .clone()
        .oneshot(Request::get("/loans").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["loans"][0]["contact_name"], "Jeanne Dupont");

    let resp = app
        .oneshot(
            Request::put(format!("/loans/{}/return", loan.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let returned = Notification::find()
        .filter(notification::Column::EventType.eq(NotificationEventType::BookReturned.as_str()))
        .filter(notification::Column::RefId.eq(loan.id.to_string()))
        .one(&db)
        .await
        .unwrap()
        .expect("book_returned notification emitted");
    assert_eq!(returned.body.as_deref(), Some("Jeanne Dupont"));
}

#[tokio::test]
async fn sale_list_uses_the_contact_full_name() {
    let (db, seed) = setup().await;
    let jeanne = insert_contact(&db, seed.library_id, Some("Jeanne"), "Dupont").await;
    let copy_id = insert_copy(&db, &seed).await;
    sale_service::record_sale(
        &db,
        SaleDto {
            id: None,
            copy_id,
            contact_id: Some(jeanne),
            library_id: seed.library_id,
            sale_date: "2026-01-15".to_string(),
            sale_price: 10.0,
            status: None,
            notes: None,
        },
    )
    .await
    .unwrap();

    let sales = sale_service::list_sales(&db, Default::default())
        .await
        .unwrap();

    assert_eq!(sales[0].contact_name.as_deref(), Some("Jeanne Dupont"));
}
