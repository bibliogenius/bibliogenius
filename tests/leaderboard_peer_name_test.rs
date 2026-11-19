//! Which answers may change a peer's name during a leaderboard refresh.
//!
//! Plain HTTP answers at a peer's stored address carry no proof of who
//! answered (ADR-072 rule g): another library on the same host must not
//! rename the peer row, its contact, or its leaderboard line. The peer
//! itself, recognised by its keys, is still renamed.

use axum::extract::State;
use axum::response::IntoResponse;
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::{contact, installation_profile, peer, peer_gamification_stats};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PEER_KEY: &str = "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11";
const OTHER_KEY: &str = "bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22";
const PEER_UUID: &str = "3f0c1c52-6c6e-4d0f-9a55-0d2f4f3f7a10";
const OTHER_UUID: &str = "9b7e2a41-1d3c-4e8a-8f60-5c4b3a2d1e0f";

async fn setup_db() -> DatabaseConnection {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("init_db in memory");
    enable_network_gamification(&db).await;
    db
}

async fn enable_network_gamification(db: &DatabaseConnection) {
    let now = chrono::Utc::now().to_rfc3339();
    let modules = serde_json::json!(["network_gamification"]).to_string();
    match installation_profile::Entity::find_by_id(1)
        .one(db)
        .await
        .unwrap()
    {
        Some(existing) => {
            let mut active: installation_profile::ActiveModel = existing.into();
            active.enabled_modules = Set(modules);
            active.update(db).await.expect("enable module");
        }
        None => {
            installation_profile::ActiveModel {
                id: Set(1),
                profile_type: Set("individual".to_string()),
                enabled_modules: Set(modules),
                created_at: Set(now.clone()),
                updated_at: Set(now),
                ..Default::default()
            }
            .insert(db)
            .await
            .expect("insert profile");
        }
    }
}

async fn insert_peer(db: &DatabaseConnection, name: &str, url: &str) -> peer::Model {
    insert_peer_with_identity(db, name, url, Some(PEER_KEY), None).await
}

async fn insert_peer_with_identity(
    db: &DatabaseConnection,
    name: &str,
    url: &str,
    public_key: Option<&str>,
    library_uuid: Option<&str>,
) -> peer::Model {
    let now = chrono::Utc::now().to_rfc3339();
    peer::ActiveModel {
        name: Set(name.to_string()),
        url: Set(url.to_string()),
        public_key: Set(public_key.map(str::to_string)),
        library_uuid: Set(library_uuid.map(str::to_string)),
        connection_status: Set("accepted".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert peer")
}

async fn insert_library_contact(db: &DatabaseConnection, name: &str) -> contact::Model {
    let now = chrono::Utc::now().to_rfc3339();
    contact::ActiveModel {
        r#type: Set("Library".to_string()),
        name: Set(name.to_string()),
        library_owner_id: Set(1),
        is_active: Set(true),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert contact")
}

fn config_answer(library_name: &str, ed25519_key: &str) -> serde_json::Value {
    serde_json::json!({
        "id": 1,
        "library_name": library_name,
        "library_description": null,
        "profile_type": "individual",
        "enabled_modules": [],
        "theme": "default",
        "latitude": null,
        "longitude": null,
        "share_location": false,
        "show_borrowed_books": false,
        "allow_library_caching": false,
        "share_gamification_stats": true,
        "ed25519_public_key": ed25519_key,
        "x25519_public_key": null,
        "library_uuid": null,
        "relay_url": null,
        "mailbox_id": null,
        "relay_write_token": null,
    })
}

fn config_answer_with_uuid(
    library_name: &str,
    ed25519_key: &str,
    library_uuid: &str,
) -> serde_json::Value {
    let mut answer = config_answer(library_name, ed25519_key);
    answer["library_uuid"] = serde_json::json!(library_uuid);
    answer
}

fn track(level: i32) -> serde_json::Value {
    serde_json::json!({ "level": level, "current": 3 })
}

fn public_stats(library_name: &str) -> serde_json::Value {
    serde_json::json!({
        "library_name": library_name,
        "collector": track(2),
        "reader": track(1),
        "lender": track(1),
        "cataloguer": track(1),
    })
}

async fn mount(server: &MockServer, route: &str, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn refresh_from_profile_screen(state: AppState) {
    let response = rust_lib_app::api::gamification::refresh_leaderboard(State(state))
        .await
        .into_response();
    assert!(response.status().is_success(), "{}", response.status());
}

async fn peer_name(db: &DatabaseConnection, id: i32) -> String {
    peer::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .name
}

async fn contact_name(db: &DatabaseConnection, id: &str) -> String {
    contact::Entity::find_by_id(id.to_string())
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .name
}

async fn cache_leaderboard_line(db: &DatabaseConnection, peer_id: i32, name: &str) {
    peer_gamification_stats::ActiveModel {
        peer_id: Set(peer_id),
        library_name: Set(name.to_string()),
        collector_level: Set(1),
        collector_current: Set(1),
        reader_level: Set(1),
        reader_current: Set(1),
        lender_level: Set(1),
        lender_current: Set(1),
        cataloguer_level: Set(1),
        cataloguer_current: Set(1),
        synced_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("cache leaderboard line");
}

async fn leaderboard_names(db: &DatabaseConnection) -> Vec<String> {
    peer_gamification_stats::Entity::find()
        .all(db)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.library_name)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn profile_refresh_ignores_another_library_at_the_peer_address() {
    let db = setup_db().await;
    let server = MockServer::start().await;
    mount(
        &server,
        "/api/config",
        config_answer("Other Library", OTHER_KEY),
    )
    .await;
    mount(
        &server,
        "/api/gamification/public-stats",
        public_stats("Other Library"),
    )
    .await;
    let peer = insert_peer(&db, "Trusted Peer", &server.uri()).await;
    let contact = insert_library_contact(&db, "Trusted Peer").await;
    cache_leaderboard_line(&db, peer.id, "Trusted Peer").await;

    refresh_from_profile_screen(AppState::new(db.clone())).await;

    assert_eq!(peer_name(&db, peer.id).await, "Trusted Peer");
    assert_eq!(contact_name(&db, &contact.id).await, "Trusted Peer");
    let lines = peer_gamification_stats::Entity::find()
        .all(&db)
        .await
        .unwrap();
    assert_eq!(lines.len(), 1, "the cached line is kept");
    assert_eq!(lines[0].library_name, "Trusted Peer");
    assert_eq!(
        lines[0].collector_level, 1,
        "no stats are taken from another library"
    );
}

/// An answer that passes the identity check but announces no name keeps
/// the stored one, on the row, the contact and the leaderboard line.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn profile_refresh_keeps_the_stored_name_when_the_peer_announces_none() {
    let db = setup_db().await;
    let server = MockServer::start().await;
    mount(&server, "/api/config", config_answer("", PEER_KEY)).await;
    mount(&server, "/api/gamification/public-stats", public_stats("")).await;
    let peer = insert_peer(&db, "Stored Name", &server.uri()).await;
    let contact = insert_library_contact(&db, "Stored Name").await;

    refresh_from_profile_screen(AppState::new(db.clone())).await;

    assert_eq!(peer_name(&db, peer.id).await, "Stored Name");
    assert_eq!(contact_name(&db, &contact.id).await, "Stored Name");
    assert_eq!(leaderboard_names(&db).await, vec!["Stored Name"]);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn profile_refresh_renames_the_peer_when_its_keys_match() {
    let db = setup_db().await;
    let server = MockServer::start().await;
    mount(
        &server,
        "/api/config",
        config_answer("Renamed Peer", PEER_KEY),
    )
    .await;
    // The stats endpoint is plain HTTP too: the line shows the row name.
    mount(
        &server,
        "/api/gamification/public-stats",
        public_stats("Some Other Name"),
    )
    .await;
    let peer = insert_peer(&db, "Old Peer Name", &server.uri()).await;
    let contact = insert_library_contact(&db, "Old Peer Name").await;

    refresh_from_profile_screen(AppState::new(db.clone())).await;

    assert_eq!(peer_name(&db, peer.id).await, "Renamed Peer");
    assert_eq!(contact_name(&db, &contact.id).await, "Renamed Peer");
    assert_eq!(leaderboard_names(&db).await, vec!["Renamed Peer"]);
}

/// A pairing older than the key exchange has no identity to compare: its
/// stored address is all there is, so the answer there is still followed.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn profile_refresh_renames_a_peer_paired_before_the_key_exchange() {
    let db = setup_db().await;
    let server = MockServer::start().await;
    mount(
        &server,
        "/api/config",
        config_answer_with_uuid("Renamed Peer", OTHER_KEY, OTHER_UUID),
    )
    .await;
    mount(
        &server,
        "/api/gamification/public-stats",
        public_stats("Renamed Peer"),
    )
    .await;
    let peer = insert_peer_with_identity(&db, "Old Peer Name", &server.uri(), None, None).await;

    refresh_from_profile_screen(AppState::new(db.clone())).await;

    assert_eq!(peer_name(&db, peer.id).await, "Renamed Peer");
    assert_eq!(leaderboard_names(&db).await, vec!["Renamed Peer"]);
}

/// A row that knows the library uuid but no key is matched on the uuid.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn profile_refresh_follows_the_library_uuid_when_the_row_has_no_key() {
    let db = setup_db().await;
    let server = MockServer::start().await;
    mount(
        &server,
        "/api/config",
        config_answer_with_uuid("Renamed Peer", OTHER_KEY, PEER_UUID),
    )
    .await;
    mount(
        &server,
        "/api/gamification/public-stats",
        public_stats("Renamed Peer"),
    )
    .await;
    let peer =
        insert_peer_with_identity(&db, "Old Peer Name", &server.uri(), None, Some(PEER_UUID)).await;

    refresh_from_profile_screen(AppState::new(db.clone())).await;

    assert_eq!(peer_name(&db, peer.id).await, "Renamed Peer");
    assert_eq!(leaderboard_names(&db).await, vec!["Renamed Peer"]);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn profile_refresh_ignores_another_library_uuid_when_the_row_has_no_key() {
    let db = setup_db().await;
    let server = MockServer::start().await;
    mount(
        &server,
        "/api/config",
        config_answer_with_uuid("Other Library", OTHER_KEY, OTHER_UUID),
    )
    .await;
    mount(
        &server,
        "/api/gamification/public-stats",
        public_stats("Other Library"),
    )
    .await;
    let peer =
        insert_peer_with_identity(&db, "Trusted Peer", &server.uri(), None, Some(PEER_UUID)).await;

    refresh_from_profile_screen(AppState::new(db.clone())).await;

    assert_eq!(peer_name(&db, peer.id).await, "Trusted Peer");
    assert!(
        leaderboard_names(&db).await.is_empty(),
        "no stats are taken from another library"
    );
}

/// A bundle that would overwrite the cached leaderboard line (higher levels)
/// and erase the cached game scores (modules absent, applied with
/// `clear_disabled`).
fn overwriting_bundle() -> serde_json::Value {
    serde_json::json!({
        "share_gamification_stats": true,
        "enabled_modules": [],
        "gamification": public_stats("Other Library"),
        "memory_game": null,
        "sliding_puzzle": null,
        "hangman": null,
        "library_name": "Other Library",
    })
}

/// Mounted with `expect(1)`: the peer was really asked, not skipped.
async fn mount_overwriting_bundle(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/public-stats-bundle"))
        .respond_with(ResponseTemplate::new(200).set_body_json(overwriting_bundle()))
        .expect(1)
        .mount(server)
        .await;
}

async fn cache_memory_score(db: &DatabaseConnection, peer_id: i32, name: &str) {
    use rust_lib_app::modules::memory_game::domain::MemoryGameRepository;
    use rust_lib_app::modules::memory_game::repository::SeaOrmGameRepository;
    SeaOrmGameRepository::new(db.clone())
        .upsert_peer_score(peer_id, name, 100.0, "easy", "2026-11-01T10:00:00Z")
        .await
        .expect("cache memory score");
}

async fn memory_scores(db: &DatabaseConnection) -> Vec<(i32, f64)> {
    use rust_lib_app::modules::memory_game::domain::MemoryGameRepository;
    use rust_lib_app::modules::memory_game::repository::SeaOrmGameRepository;
    SeaOrmGameRepository::new(db.clone())
        .get_peer_scores()
        .await
        .expect("memory scores")
        .into_iter()
        .map(|s| (s.peer_id, s.best_score))
        .collect()
}

async fn cache_puzzle_and_hangman_scores(db: &DatabaseConnection, peer_id: i32, name: &str) {
    use rust_lib_app::modules::hangman::domain::HangmanRepository;
    use rust_lib_app::modules::hangman::repository::SeaOrmHangmanRepository;
    use rust_lib_app::modules::sliding_puzzle::domain::SlidingPuzzleRepository;
    use rust_lib_app::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository;
    SeaOrmPuzzleRepository::new(db.clone())
        .upsert_peer_score(peer_id, name, 200.0, "easy", "2026-11-01T10:00:00Z")
        .await
        .expect("cache puzzle score");
    SeaOrmHangmanRepository::new(db.clone())
        .upsert_peer_score(peer_id, name, 300.0, "easy", "2026-11-01T10:00:00Z")
        .await
        .expect("cache hangman score");
}

async fn puzzle_and_hangman_scores(db: &DatabaseConnection) -> (Vec<(i32, f64)>, Vec<(i32, f64)>) {
    use rust_lib_app::modules::hangman::domain::HangmanRepository;
    use rust_lib_app::modules::hangman::repository::SeaOrmHangmanRepository;
    use rust_lib_app::modules::sliding_puzzle::domain::SlidingPuzzleRepository;
    use rust_lib_app::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository;
    let puzzle = SeaOrmPuzzleRepository::new(db.clone())
        .get_peer_scores()
        .await
        .expect("puzzle scores")
        .into_iter()
        .map(|s| (s.peer_id, s.best_score))
        .collect();
    let hangman = SeaOrmHangmanRepository::new(db.clone())
        .get_peer_scores()
        .await
        .expect("hangman scores")
        .into_iter()
        .map(|s| (s.peer_id, s.best_score))
        .collect();
    (puzzle, hangman)
}

async fn sync_leaderboards(db: &DatabaseConnection) {
    rust_lib_app::utils::leaderboard_relay::sync_all_leaderboards(
        &AppState::new(db.clone()),
        false,
    )
    .await;
}

/// The peer's cached line and game scores are exactly as cached before.
async fn assert_caches_untouched(db: &DatabaseConnection, peer_id: i32) {
    assert_eq!(peer_name(db, peer_id).await, "Trusted Peer");
    let lines = peer_gamification_stats::Entity::find()
        .all(db)
        .await
        .unwrap();
    assert_eq!(lines.len(), 1, "the cached line is kept");
    assert_eq!(lines[0].library_name, "Trusted Peer");
    assert_eq!(lines[0].collector_level, 1, "no stats are written");
    assert_eq!(
        memory_scores(db).await,
        vec![(peer_id, 100.0)],
        "the cached Memory score is not erased"
    );
    assert_eq!(
        puzzle_and_hangman_scores(db).await,
        (vec![(peer_id, 200.0)], vec![(peer_id, 300.0)]),
        "the cached puzzle and hangman scores are not erased"
    );
}

async fn insert_trusted_peer_with_caches(db: &DatabaseConnection, url: &str) -> peer::Model {
    let peer = insert_peer(db, "Trusted Peer", url).await;
    cache_leaderboard_line(db, peer.id, "Trusted Peer").await;
    cache_memory_score(db, peer.id, "Trusted Peer").await;
    cache_puzzle_and_hangman_scores(db, peer.id, "Trusted Peer").await;
    peer
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn leaderboard_sync_ignores_a_bundle_from_another_library() {
    let db = setup_db().await;
    let server = MockServer::start().await;
    mount_overwriting_bundle(&server).await;
    mount(
        &server,
        "/api/config",
        config_answer("Other Library", OTHER_KEY),
    )
    .await;
    let peer = insert_trusted_peer_with_caches(&db, &server.uri()).await;

    sync_leaderboards(&db).await;

    assert_caches_untouched(&db, peer.id).await;
}

/// The legacy branch (no bundle endpoint) reads `/api/config` for the
/// modules: another library's answer must not drive the per-game reads.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn leaderboard_sync_legacy_branch_ignores_another_library() {
    let db = setup_db().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/public-stats-bundle"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    // No module enabled: applied, it would erase the cached Memory score.
    mount(
        &server,
        "/api/config",
        config_answer("Other Library", OTHER_KEY),
    )
    .await;
    mount(
        &server,
        "/api/gamification/public-stats",
        public_stats("Other Library"),
    )
    .await;
    let peer = insert_trusted_peer_with_caches(&db, &server.uri()).await;

    sync_leaderboards(&db).await;

    assert_caches_untouched(&db, peer.id).await;
}

/// Without a readable `/api/config` nothing tells who served the bundle:
/// the peer is handled as silent.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn leaderboard_sync_ignores_a_bundle_when_the_config_is_silent() {
    for config_response in [
        ResponseTemplate::new(500),
        ResponseTemplate::new(200).set_body_string("not json"),
    ] {
        let db = setup_db().await;
        let server = MockServer::start().await;
        mount_overwriting_bundle(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/config"))
            .respond_with(config_response)
            .mount(&server)
            .await;
        let peer = insert_trusted_peer_with_caches(&db, &server.uri()).await;

        sync_leaderboards(&db).await;

        assert_caches_untouched(&db, peer.id).await;
    }
}

/// A pairing older than the key exchange has nothing to compare: the
/// answer at its stored address is still followed.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn leaderboard_sync_follows_a_peer_paired_before_the_key_exchange() {
    let db = setup_db().await;
    let server = MockServer::start().await;
    mount(&server, "/api/public-stats-bundle", overwriting_bundle()).await;
    mount(
        &server,
        "/api/config",
        config_answer_with_uuid("Other Library", OTHER_KEY, OTHER_UUID),
    )
    .await;
    let peer = insert_peer_with_identity(&db, "Old Peer", &server.uri(), None, None).await;
    cache_leaderboard_line(&db, peer.id, "Old Peer").await;
    cache_memory_score(&db, peer.id, "Old Peer").await;

    sync_leaderboards(&db).await;

    let lines = peer_gamification_stats::Entity::find()
        .all(&db)
        .await
        .unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].library_name, "Old Peer");
    assert_eq!(lines[0].collector_level, 2, "the bundle is applied");
    assert!(
        memory_scores(&db).await.is_empty(),
        "the disabled module is cleared as before"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn leaderboard_sync_shows_the_stored_name_for_a_plain_http_bundle() {
    let db = setup_db().await;
    let server = MockServer::start().await;
    // The peer itself answers (same key) but announces another name.
    mount(
        &server,
        "/api/config",
        config_answer("Other Library", PEER_KEY),
    )
    .await;
    mount(
        &server,
        "/api/public-stats-bundle",
        serde_json::json!({
            "share_gamification_stats": true,
            "enabled_modules": [],
            "gamification": public_stats("Other Library"),
            "memory_game": null,
            "sliding_puzzle": null,
            "hangman": null,
            "library_name": "Other Library",
        }),
    )
    .await;
    let peer = insert_peer(&db, "Trusted Peer", &server.uri()).await;

    rust_lib_app::utils::leaderboard_relay::sync_all_leaderboards(
        &AppState::new(db.clone()),
        false,
    )
    .await;

    assert_eq!(peer_name(&db, peer.id).await, "Trusted Peer");
    assert_eq!(leaderboard_names(&db).await, vec!["Trusted Peer"]);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn leaderboard_sync_legacy_peer_shows_the_stored_name() {
    let db = setup_db().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/public-stats-bundle"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    // The peer itself answers (same key) but announces another name.
    mount(
        &server,
        "/api/config",
        config_answer("Other Library", PEER_KEY),
    )
    .await;
    mount(
        &server,
        "/api/gamification/public-stats",
        public_stats("Other Library"),
    )
    .await;
    let peer = insert_peer(&db, "Trusted Peer", &server.uri()).await;

    rust_lib_app::utils::leaderboard_relay::sync_all_leaderboards(
        &AppState::new(db.clone()),
        false,
    )
    .await;

    assert_eq!(peer_name(&db, peer.id).await, "Trusted Peer");
    assert_eq!(
        leaderboard_names(&db).await,
        vec!["Trusted Peer"],
        "the leaderboard line shows the stored name, not the answered one"
    );
}
