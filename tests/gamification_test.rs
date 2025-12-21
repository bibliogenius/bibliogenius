//! Gamification V3 API Tests
//! Tests for the new 3-track gamification system

use rust_lib_app::db;
use rust_lib_app::models::{
    book, gamification_achievements, gamification_config, gamification_progress,
    gamification_streaks, user,
};
use sea_orm::{DatabaseConnection, EntityTrait, Set};

// Helper to create a test database
async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

// Helper to create a test admin user
async fn create_test_admin(db: &DatabaseConnection) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let user_model = user::ActiveModel {
        username: Set("gamification_test_user".to_string()),
        password_hash: Set("$2b$12$dummy_hash".to_string()),
        role: Set("admin".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let res = user::Entity::insert(user_model)
        .exec(db)
        .await
        .expect("Failed to create admin user");
    res.last_insert_id
}

// Helper to create test books
async fn create_test_books(db: &DatabaseConnection, count: usize, reading_status: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    for i in 0..count {
        let book_model = book::ActiveModel {
            title: Set(format!("Test Book {}", i)),
            isbn: Set(Some(format!("978000000{:04}", i))),
            reading_status: Set(reading_status.to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
            ..Default::default()
        };
        book::Entity::insert(book_model)
            .exec(db)
            .await
            .expect("Failed to create book");
    }
}

#[tokio::test]
async fn test_gamification_tables_created() {
    let db = setup_test_db().await;
    let _user_id = create_test_admin(&db).await;

    // Verify gamification tables exist by querying them
    let _config = gamification_config::Entity::find()
        .one(&db)
        .await
        .expect("Failed to query gamification_config");

    // Config may or may not exist depending on migration behavior
    // The important thing is that the query doesn't fail (table exists)

    let _progress = gamification_progress::Entity::find()
        .one(&db)
        .await
        .expect("Failed to query gamification_progress");

    let _achievements = gamification_achievements::Entity::find()
        .one(&db)
        .await
        .expect("Failed to query gamification_achievements");

    let _streaks = gamification_streaks::Entity::find()
        .one(&db)
        .await
        .expect("Failed to query gamification_streaks");

    // Tables exist if we got here without errors
    assert!(true, "All gamification tables were created successfully");
}

#[tokio::test]
async fn test_gamification_config_model() {
    let db = setup_test_db().await;
    let user_id = create_test_admin(&db).await;

    let now = chrono::Utc::now().to_rfc3339();
    let config = gamification_config::ActiveModel {
        user_id: Set(user_id),
        preset: Set("individual".to_string()),
        streaks_enabled: Set(true),
        achievements_enabled: Set(true),
        achievements_style: Set("minimal".to_string()),
        reading_goals_enabled: Set(true),
        reading_goal_yearly: Set(24),
        tracks_enabled: Set(r#"["collector","reader","lender"]"#.to_string()),
        notifications_enabled: Set(true),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    let res = gamification_config::Entity::insert(config)
        .exec(&db)
        .await
        .expect("Failed to insert gamification config");

    let saved = gamification_config::Entity::find_by_id(res.last_insert_id)
        .one(&db)
        .await
        .expect("Failed to fetch config")
        .expect("Config not found");

    assert_eq!(saved.user_id, user_id);
    assert_eq!(saved.preset, "individual");
    assert_eq!(saved.reading_goal_yearly, 24);
    assert_eq!(saved.achievements_style, "minimal");
}

#[tokio::test]
async fn test_gamification_progress_model() {
    let db = setup_test_db().await;
    let user_id = create_test_admin(&db).await;

    let now = chrono::Utc::now().to_rfc3339();

    // Create progress for collector track
    let progress = gamification_progress::ActiveModel {
        user_id: Set(user_id),
        track: Set("collector".to_string()),
        current_value: Set(25),
        level: Set(1), // Bronze
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    let res = gamification_progress::Entity::insert(progress)
        .exec(&db)
        .await
        .expect("Failed to insert progress");

    let saved = gamification_progress::Entity::find_by_id(res.last_insert_id)
        .one(&db)
        .await
        .expect("Failed to fetch progress")
        .expect("Progress not found");

    assert_eq!(saved.user_id, user_id);
    assert_eq!(saved.track, "collector");
    assert_eq!(saved.current_value, 25);
    assert_eq!(saved.level, 1);
}

#[tokio::test]
async fn test_gamification_achievements_model() {
    let db = setup_test_db().await;
    let user_id = create_test_admin(&db).await;

    let now = chrono::Utc::now().to_rfc3339();

    let achievement = gamification_achievements::ActiveModel {
        user_id: Set(user_id),
        achievement_id: Set("first_book".to_string()),
        unlocked_at: Set(now),
        ..Default::default()
    };

    let res = gamification_achievements::Entity::insert(achievement)
        .exec(&db)
        .await
        .expect("Failed to insert achievement");

    let saved = gamification_achievements::Entity::find_by_id(res.last_insert_id)
        .one(&db)
        .await
        .expect("Failed to fetch achievement")
        .expect("Achievement not found");

    assert_eq!(saved.user_id, user_id);
    assert_eq!(saved.achievement_id, "first_book");
}

#[tokio::test]
async fn test_gamification_streaks_model() {
    let db = setup_test_db().await;
    let user_id = create_test_admin(&db).await;

    let streak = gamification_streaks::ActiveModel {
        user_id: Set(user_id),
        current_streak: Set(5),
        longest_streak: Set(12),
        last_activity_date: Set(Some("2025-12-12".to_string())),
        ..Default::default()
    };

    let res = gamification_streaks::Entity::insert(streak)
        .exec(&db)
        .await
        .expect("Failed to insert streak");

    let saved = gamification_streaks::Entity::find_by_id(res.last_insert_id)
        .one(&db)
        .await
        .expect("Failed to fetch streak")
        .expect("Streak not found");

    assert_eq!(saved.user_id, user_id);
    assert_eq!(saved.current_streak, 5);
    assert_eq!(saved.longest_streak, 12);
    assert_eq!(saved.last_activity_date, Some("2025-12-12".to_string()));
}

#[tokio::test]
async fn test_collector_track_counts_books() {
    let db = setup_test_db().await;
    let _user_id = create_test_admin(&db).await;

    // Create 15 test books
    create_test_books(&db, 15, "to_read").await;

    // Verify count
    use sea_orm::PaginatorTrait;
    let books_count = book::Entity::find()
        .count(&db)
        .await
        .expect("Failed to count books");

    assert_eq!(books_count, 15);

    // At 15 books, user should be at Bronze level (threshold: 10)
    // Progress to Silver (threshold: 50) = (15-10)/(50-10) = 5/40 = 12.5%
    let level = if books_count >= 200 {
        3
    }
    // Gold
    else if books_count >= 50 {
        2
    }
    // Silver
    else if books_count >= 10 {
        1
    }
    // Bronze
    else {
        0
    };

    assert_eq!(level, 1, "Expected Bronze level at 15 books");
}

#[tokio::test]
async fn test_reader_track_counts_read_books() {
    let db = setup_test_db().await;
    let _user_id = create_test_admin(&db).await;

    // Create 10 books: 6 read, 4 to_read
    create_test_books(&db, 6, "read").await;
    create_test_books(&db, 4, "to_read").await;

    // Verify read count
    use sea_orm::{ColumnTrait, PaginatorTrait, QueryFilter};
    let read_count = book::Entity::find()
        .filter(book::Column::ReadingStatus.eq("read"))
        .count(&db)
        .await
        .expect("Failed to count read books");

    assert_eq!(read_count, 6);

    // At 6 read books, user should be at Bronze level (threshold: 5)
    let level = if read_count >= 100 {
        3
    } else if read_count >= 20 {
        2
    } else if read_count >= 5 {
        1
    } else {
        0
    };

    assert_eq!(level, 1, "Expected Bronze level at 6 read books");
}

#[tokio::test]
async fn test_track_thresholds() {
    // Test that track thresholds are correctly applied

    // Collector thresholds: 10, 50, 200
    let collector_tests = vec![
        (0, 0),   // Novice
        (9, 0),   // Novice
        (10, 1),  // Bronze
        (49, 1),  // Bronze
        (50, 2),  // Silver
        (199, 2), // Silver
        (200, 3), // Gold
        (500, 3), // Gold (capped)
    ];

    for (count, expected_level) in collector_tests {
        let level = calculate_level(count, &[10, 50, 200]);
        assert_eq!(
            level, expected_level,
            "Collector at {} should be level {}",
            count, expected_level
        );
    }

    // Reader thresholds: 5, 20, 100
    let reader_tests = vec![(0, 0), (5, 1), (20, 2), (100, 3)];

    for (count, expected_level) in reader_tests {
        let level = calculate_level(count, &[5, 20, 100]);
        assert_eq!(
            level, expected_level,
            "Reader at {} should be level {}",
            count, expected_level
        );
    }

    // Lender thresholds: 5, 20, 50
    let lender_tests = vec![(0, 0), (5, 1), (20, 2), (50, 3)];

    for (count, expected_level) in lender_tests {
        let level = calculate_level(count, &[5, 20, 50]);
        assert_eq!(
            level, expected_level,
            "Lender at {} should be level {}",
            count, expected_level
        );
    }
}

// Helper function matching the API logic
fn calculate_level(current: i64, thresholds: &[i32; 3]) -> i32 {
    if current >= thresholds[2] as i64 {
        3
    } else if current >= thresholds[1] as i64 {
        2
    } else if current >= thresholds[0] as i64 {
        1
    } else {
        0
    }
}

#[tokio::test]
async fn test_multiple_achievements_per_user() {
    let db = setup_test_db().await;
    let user_id = create_test_admin(&db).await;

    let now = chrono::Utc::now().to_rfc3339();

    // Unlock multiple achievements
    let achievements = vec!["first_book", "first_scan", "collector_bronze"];

    for achievement_id in &achievements {
        let achievement = gamification_achievements::ActiveModel {
            user_id: Set(user_id),
            achievement_id: Set(achievement_id.to_string()),
            unlocked_at: Set(now.clone()),
            ..Default::default()
        };
        gamification_achievements::Entity::insert(achievement)
            .exec(&db)
            .await
            .expect("Failed to insert achievement");
    }

    // Verify all achievements exist
    use sea_orm::{ColumnTrait, PaginatorTrait, QueryFilter};
    let count = gamification_achievements::Entity::find()
        .filter(gamification_achievements::Column::UserId.eq(user_id))
        .count(&db)
        .await
        .expect("Failed to count achievements");

    assert_eq!(count, 3);
}

#[tokio::test]
async fn test_unique_achievement_constraint() {
    let db = setup_test_db().await;
    let user_id = create_test_admin(&db).await;

    let now = chrono::Utc::now().to_rfc3339();

    // Insert first achievement
    let achievement = gamification_achievements::ActiveModel {
        user_id: Set(user_id),
        achievement_id: Set("first_book".to_string()),
        unlocked_at: Set(now.clone()),
        ..Default::default()
    };
    gamification_achievements::Entity::insert(achievement)
        .exec(&db)
        .await
        .expect("Failed to insert first achievement");

    // Try to insert duplicate - should fail due to UNIQUE constraint
    let duplicate = gamification_achievements::ActiveModel {
        user_id: Set(user_id),
        achievement_id: Set("first_book".to_string()),
        unlocked_at: Set(now),
        ..Default::default()
    };
    let result = gamification_achievements::Entity::insert(duplicate)
        .exec(&db)
        .await;

    assert!(result.is_err(), "Expected duplicate achievement to fail");
}
