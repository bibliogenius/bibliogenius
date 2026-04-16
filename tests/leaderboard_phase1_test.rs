//! Phase 1 (direct HTTP) path of `sync_all_leaderboards`.
//!
//! Covers the two branches introduced when `/api/public-stats-bundle` became
//! the preferred LAN entry point:
//!
//! 1. New endpoint returns 200 → bundle is applied in a single round-trip,
//!    `peers.name` is refreshed, per-game score caches are populated.
//! 2. New endpoint returns 404 → legacy fallback path (`/api/config` +
//!    `/api/game/memory/public-best` + ...) is used so peers on an older
//!    backend still appear on the leaderboard.

use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::peer;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("init_db in memory")
}

async fn insert_peer(db: &DatabaseConnection, name: &str, url: &str) -> peer::Model {
    let now = chrono::Utc::now().to_rfc3339();
    peer::ActiveModel {
        name: Set(name.to_string()),
        url: Set(url.to_string()),
        connection_status: Set("accepted".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert peer")
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn phase1_uses_new_bundle_endpoint() {
    let db = setup_test_db().await;
    let server = MockServer::start().await;

    let bundle = serde_json::json!({
        "share_gamification_stats": false,
        "enabled_modules": ["memory_game"],
        "gamification": null,
        "memory_game": {
            "best_score": 1337.0,
            "difficulty": "medium",
            "played_at": "2026-04-16T10:00:00Z",
        },
        "sliding_puzzle": null,
        "hangman": null,
        "library_name": "Fresh Library Name",
    });

    Mock::given(method("GET"))
        .and(path("/api/public-stats-bundle"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&bundle))
        .expect(1)
        .mount(&server)
        .await;

    let peer = insert_peer(&db, "Old Library Name", &server.uri()).await;

    let state = AppState::new(db.clone());
    rust_lib_app::utils::leaderboard_relay::sync_all_leaderboards(&state, false).await;

    // Peer name should have been updated from the bundle.
    let refreshed = peer::Entity::find_by_id(peer.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        refreshed.name, "Fresh Library Name",
        "peer.name should be refreshed from the bundle's library_name"
    );

    // Memory game score should have been cached.
    use rust_lib_app::modules::memory_game::domain::MemoryGameRepository;
    use rust_lib_app::modules::memory_game::repository::SeaOrmGameRepository;
    let repo = SeaOrmGameRepository::new(db.clone());
    let scores = repo.get_peer_scores().await.expect("get_peer_scores");
    assert_eq!(scores.len(), 1, "one peer score should be cached");
    assert_eq!(scores[0].peer_id, peer.id);
    assert_eq!(scores[0].best_score, 1337.0);
    assert_eq!(scores[0].difficulty, "medium");
    assert_eq!(
        scores[0].library_name, "Fresh Library Name",
        "cached library_name should match the bundle"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn phase1_falls_back_to_legacy_on_404() {
    let db = setup_test_db().await;
    let server = MockServer::start().await;

    // New endpoint is missing on a peer running an older backend.
    Mock::given(method("GET"))
        .and(path("/api/public-stats-bundle"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    // Legacy path: /api/config exposes enabled_modules so the caller knows
    // which per-game endpoints to hit.
    let config = serde_json::json!({
        "id": 1,
        "library_name": "Legacy Library",
        "library_description": null,
        "profile_type": "individual",
        "enabled_modules": ["memory_game"],
        "theme": "default",
        "latitude": null,
        "longitude": null,
        "share_location": false,
        "show_borrowed_books": false,
        "allow_library_caching": false,
        "share_gamification_stats": false,
        "ed25519_public_key": null,
        "x25519_public_key": null,
        "library_uuid": null,
        "relay_url": null,
        "mailbox_id": null,
        "relay_write_token": null,
    });
    Mock::given(method("GET"))
        .and(path("/api/config"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&config))
        .expect(1)
        .mount(&server)
        .await;

    let legacy_score = serde_json::json!({
        "best_score": 4242.0,
        "difficulty": "hard",
        "played_at": "2026-04-16T11:00:00Z",
    });
    Mock::given(method("GET"))
        .and(path("/api/game/memory/public-best"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&legacy_score))
        .expect(1)
        .mount(&server)
        .await;

    let peer = insert_peer(&db, "Legacy Peer", &server.uri()).await;

    let state = AppState::new(db.clone());
    rust_lib_app::utils::leaderboard_relay::sync_all_leaderboards(&state, false).await;

    // Legacy path populates the memory score via per-game endpoint.
    use rust_lib_app::modules::memory_game::domain::MemoryGameRepository;
    use rust_lib_app::modules::memory_game::repository::SeaOrmGameRepository;
    let repo = SeaOrmGameRepository::new(db.clone());
    let scores = repo.get_peer_scores().await.expect("get_peer_scores");
    assert_eq!(scores.len(), 1);
    assert_eq!(scores[0].peer_id, peer.id);
    assert_eq!(scores[0].best_score, 4242.0);
    assert_eq!(scores[0].difficulty, "hard");
    // Legacy path uses peers.name (not refreshed — that's the known limitation
    // this fallback accepts until all peers upgrade).
    assert_eq!(scores[0].library_name, "Legacy Peer");
}
