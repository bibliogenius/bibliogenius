//! Authentication tests for the MCP endpoint.
//!
//! `/api/mcp/rpc` serves the OWNER view of the library (private books, loans,
//! statistics) from a router bound to 0.0.0.0. Three independent conditions guard
//! it, and each one is asserted here:
//!
//!  - a LAN peer must be refused (it reaches the same listener as catalogue traffic);
//!  - a caller without a valid token must be refused, even from loopback;
//!  - a web page in the user's own browser must be refused, because it too speaks
//!    from 127.0.0.1 and the shared CORS layer answers `Access-Control-Allow-Origin: *`.
//!
//! The last one is the subtle case: the loopback guard alone let any visited site
//! `fetch()` the endpoint and read the reply.

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use rust_lib_app::api::mcp::rpc_endpoint;
use rust_lib_app::db;
use rust_lib_app::infrastructure::mcp_token;
use sea_orm::{ConnectionTrait, DatabaseConnection};
use std::net::SocketAddr;
use std::sync::Once;
use tower::ServiceExt;

static INIT_TOKEN_DIR: Once = Once::new();

/// Point `DATABASE_URL` at a scratch directory so the process resolves a real token
/// file. `expected_token()` caches in a `OnceLock`, so this must happen before the
/// first call, once for the whole test binary.
fn token() -> &'static str {
    INIT_TOKEN_DIR.call_once(|| {
        let dir = std::env::temp_dir().join("bg-mcp-auth-test");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let db_path = dir.join("bibliogenius.db");
        // SAFETY: single-threaded initialization, before any token read.
        unsafe { std::env::set_var("DATABASE_URL", format!("sqlite:{}", db_path.display())) };
    });
    mcp_token::expected_token().expect("token in a writable scratch directory")
}

/// State of the "AI assistants (MCP)" switch as the app pushes it into
/// `installation_profile.enabled_modules` (ADR-047, fourth condition).
#[derive(Clone, Copy)]
enum Module {
    Enabled,
    Disabled,
    /// No profile row at all: must be read as disabled (fail closed).
    NoProfile,
}

async fn test_db(module: Module) -> DatabaseConnection {
    let db: DatabaseConnection = db::init_db("sqlite::memory:")
        .await
        .expect("init_db in memory");
    set_module(&db, module).await;
    db
}

async fn set_module(db: &DatabaseConnection, module: Module) {
    let sql = match module {
        // Other modules around `mcp` prove the check is a membership test, not
        // an equality on the whole list.
        Module::Enabled => {
            r#"UPDATE installation_profile SET enabled_modules = '["network","mcp","quotes"]'"#
        }
        Module::Disabled => {
            r#"UPDATE installation_profile SET enabled_modules = '["network","quotes"]'"#
        }
        Module::NoProfile => "DELETE FROM installation_profile",
    };
    db.execute_unprepared(sql).await.expect("set mcp module");
}

fn rpc_app(db: DatabaseConnection) -> axum::Router {
    axum::Router::new()
        .route("/api/mcp/rpc", axum::routing::post(rpc_endpoint))
        .with_state(db)
}

async fn build_app() -> axum::Router {
    rpc_app(test_db(Module::Enabled).await)
}

/// A well-formed `tools/list` call: valid JSON-RPC, so any rejection comes from the
/// guard rather than from request parsing.
fn rpc_body() -> Body {
    rpc_body_for("tools/list")
}

fn rpc_body_for(method: &str) -> Body {
    let params = if method == "tools/call" {
        r#"{"name":"get_statistics","arguments":{}}"#
    } else {
        "{}"
    };
    Body::from(format!(
        r#"{{"jsonrpc":"2.0","method":"{method}","params":{params},"id":1}}"#
    ))
}

fn request(peer: Option<SocketAddr>, bearer: Option<&str>, origin: Option<&str>) -> Request<Body> {
    request_with_body(peer, bearer, origin, rpc_body())
}

fn request_with_body(
    peer: Option<SocketAddr>,
    bearer: Option<&str>,
    origin: Option<&str>,
    body: Body,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/mcp/rpc")
        .header("content-type", "application/json");
    if let Some(bearer) = bearer {
        builder = builder.header("Authorization", format!("Bearer {}", bearer));
    }
    if let Some(origin) = origin {
        builder = builder.header("Origin", origin);
    }
    let mut req = builder.body(body).expect("request");
    if let Some(peer) = peer {
        req.extensions_mut().insert(ConnectInfo(peer));
    }
    req
}

const LOOPBACK: &str = "127.0.0.1:54321";
const LAN_PEER: &str = "192.168.1.42:54321";

#[tokio::test]
async fn loopback_with_a_valid_token_is_served() {
    let token = token();
    let app = build_app().await;

    let response = app
        .oneshot(request(Some(LOOPBACK.parse().unwrap()), Some(token), None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert!(
        json["result"]["tools"].is_array(),
        "the legitimate helper still gets its tool list: {json}"
    );
}

#[tokio::test]
async fn a_lan_peer_is_refused_even_with_a_valid_token() {
    let token = token();
    let app = build_app().await;

    let response = app
        .oneshot(request(Some(LAN_PEER.parse().unwrap()), Some(token), None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_call_without_a_token_is_refused_from_loopback() {
    token();
    let app = build_app().await;

    let response = app
        .oneshot(request(Some(LOOPBACK.parse().unwrap()), None, None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_call_with_a_wrong_token_is_refused_from_loopback() {
    token();
    let app = build_app().await;

    let response = app
        .oneshot(request(
            Some(LOOPBACK.parse().unwrap()),
            Some("not-the-token"),
            None,
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_browser_page_is_refused_even_from_loopback_with_a_valid_token() {
    // Models the real attack: the user visits evil.example, whose script fetches the
    // loopback endpoint. The TCP source is 127.0.0.1, so the loopback guard passes.
    // Browsers attach Origin to exactly this request; MCP clients never do.
    let token = token();
    let app = build_app().await;

    let response = app
        .oneshot(request(
            Some(LOOPBACK.parse().unwrap()),
            Some(token),
            Some("https://evil.example"),
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_request_without_peer_address_is_refused() {
    // Fail closed: a router built without `into_make_service_with_connect_info`
    // must not silently admit everyone.
    token();
    let app = build_app().await;

    let response = app
        .oneshot(request(None, Some(token()), None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ── /api/integrations/mcp-config ────────────────────────────────────────────
//
// The configuration endpoint hands out the very token that guards `/api/mcp/rpc`.
// Guarding it with loopback alone would let any visited page read the secret through
// the permissive CORS layer, which would defeat the token everywhere else.

use rust_lib_app::api::integrations::mcp_config;

async fn config_app() -> axum::Router {
    config_app_with(test_db(Module::Enabled).await)
}

fn config_app_with(db: DatabaseConnection) -> axum::Router {
    axum::Router::new()
        .route(
            "/api/integrations/mcp-config",
            axum::routing::get(mcp_config),
        )
        .with_state(db)
}

fn config_request(peer: Option<SocketAddr>, origin: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri("/api/integrations/mcp-config");
    if let Some(origin) = origin {
        builder = builder.header("Origin", origin);
    }
    let mut req = builder.body(Body::empty()).expect("request");
    if let Some(peer) = peer {
        req.extensions_mut().insert(ConnectInfo(peer));
    }
    req
}

#[tokio::test]
async fn the_app_can_read_its_own_mcp_configuration() {
    let token = token();

    let response = config_app()
        .await
        .oneshot(config_request(Some(LOOPBACK.parse().unwrap()), None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let env = &json["config"]["mcpServers"]["bibliogenius"]["env"];
    assert_eq!(
        env[mcp_token::TOKEN_ENV_VAR],
        token,
        "the copy button must receive a usable token"
    );
}

#[tokio::test]
async fn a_browser_page_cannot_read_the_mcp_token() {
    // The attack the token exists to stop: the user visits evil.example, whose script
    // fetches the loopback configuration endpoint. The TCP source is 127.0.0.1, so the
    // loopback guard alone would pass and the CORS layer (`Allow-Origin: *`) would let
    // the page read the token, which then unlocks the owner view of the library.
    token();

    let response = config_app()
        .await
        .oneshot(config_request(
            Some(LOOPBACK.parse().unwrap()),
            Some("https://evil.example"),
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_lan_peer_cannot_read_the_mcp_token() {
    token();

    let response = config_app()
        .await
        .oneshot(config_request(Some(LAN_PEER.parse().unwrap()), None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ── Settings switch (ADR-047, fourth condition) ─────────────────────────────
//
// The "AI assistants (MCP)" switch used to hide the copy button and nothing else:
// an assistant configured once kept the owner view after the user turned it off.

const RPC_METHODS: [&str; 3] = ["initialize", "tools/list", "tools/call"];

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

#[tokio::test]
async fn a_disabled_module_refuses_every_method_even_with_a_valid_token() {
    let token = token();
    for module in [Module::Disabled, Module::NoProfile] {
        let db = test_db(module).await;
        for method in RPC_METHODS {
            let response = rpc_app(db.clone())
                .oneshot(request_with_body(
                    Some(LOOPBACK.parse().unwrap()),
                    Some(token),
                    None,
                    rpc_body_for(method),
                ))
                .await
                .expect("response");

            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method}");
            let json = body_json(response).await;
            assert_eq!(
                json["error"], "MCP is disabled in BiblioGenius settings",
                "{method}: the helper relays this message to the assistant"
            );
            assert!(json.get("result").is_none(), "{method}: no data served");
        }
    }
}

#[tokio::test]
async fn re_enabling_the_module_restores_access_with_the_same_token() {
    let token = token();
    let db = test_db(Module::Disabled).await;

    let refused = rpc_app(db.clone())
        .oneshot(request(Some(LOOPBACK.parse().unwrap()), Some(token), None))
        .await
        .expect("response");
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    set_module(&db, Module::Enabled).await;

    for method in RPC_METHODS {
        let response = rpc_app(db.clone())
            .oneshot(request_with_body(
                Some(LOOPBACK.parse().unwrap()),
                Some(token),
                None,
                rpc_body_for(method),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK, "{method}");
        let json = body_json(response).await;
        assert!(json["result"].is_object(), "{method} served again: {json}");
    }
}

#[tokio::test]
async fn unauthenticated_callers_cannot_tell_whether_the_module_is_enabled() {
    // The token is checked first: without it, the answer must not depend on the
    // setting, otherwise the endpoint would leak it.
    token();
    for bearer in [None, Some("not-the-token")] {
        let mut answers = Vec::new();
        for module in [Module::Enabled, Module::Disabled, Module::NoProfile] {
            let response = rpc_app(test_db(module).await)
                .oneshot(request(Some(LOOPBACK.parse().unwrap()), bearer, None))
                .await
                .expect("response");
            let status = response.status();
            answers.push((status, body_json(response).await));
        }
        assert_eq!(answers[0].0, StatusCode::UNAUTHORIZED);
        assert!(
            answers.iter().all(|a| a == &answers[0]),
            "same answer whatever the setting: {answers:?}"
        );
    }
}

#[tokio::test]
async fn a_disabled_module_does_not_hand_out_the_token() {
    let token = token();
    for module in [Module::Disabled, Module::NoProfile] {
        let response = config_app_with(test_db(module).await)
            .oneshot(config_request(Some(LOOPBACK.parse().unwrap()), None))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert!(
            !String::from_utf8_lossy(&body).contains(token),
            "the token must not leave the app while MCP is disabled"
        );
    }
}

#[tokio::test]
async fn a_disabled_module_refuses_notifications_instead_of_acknowledging_them() {
    // A notification (no `id`) is answered 204 when served. While the switch is
    // off it must get the same refusal as any other call, not a silent 204.
    let token = token();
    let response = rpc_app(test_db(Module::Disabled).await)
        .oneshot(request_with_body(
            Some(LOOPBACK.parse().unwrap()),
            Some(token),
            None,
            Body::from(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_malformed_body_serves_nothing_whatever_the_setting() {
    // Body parsing is rejected by axum before the handler runs. That rejection
    // must not depend on the switch, and must never carry library data.
    let token = token();
    let mut statuses = Vec::new();
    for module in [Module::Enabled, Module::Disabled] {
        let response = rpc_app(test_db(module).await)
            .oneshot(request_with_body(
                Some(LOOPBACK.parse().unwrap()),
                Some(token),
                None,
                Body::from("not json"),
            ))
            .await
            .expect("response");
        assert!(response.status().is_client_error(), "{}", response.status());
        statuses.push(response.status());
    }
    assert_eq!(statuses[0], statuses[1]);
}
