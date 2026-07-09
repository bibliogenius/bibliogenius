use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use chrono::{Duration, Utc};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use std::env;

use axum::{
    async_trait,
    extract::{ConnectInfo, FromRequestParts, Json},
    http::{StatusCode, request::Parts},
};
use serde_json::json;
use std::net::SocketAddr;

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // username
    pub role: String,
    pub exp: usize,
}

#[async_trait]
impl<S> FromRequestParts<S> for Claims
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get("Authorization")
            .and_then(|h| h.to_str().ok())
            .ok_or((
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing Authorization header" })),
            ))?;

        if !auth_header.starts_with("Bearer ") {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Invalid Authorization header format" })),
            ));
        }

        let token = &auth_header[7..];
        decode_jwt(token).map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Invalid or expired token" })),
            )
        })
    }
}

/// Extractor that admits a request only when it originates from loopback
/// (127.0.0.1 / ::1).
///
/// Device-management endpoints are served on the same 0.0.0.0 listener as peer
/// traffic, but they are only ever called by the local client (via FFI, or over
/// 127.0.0.1 for `/devices/register` and `/devices/sync/:id`). Guarding them
/// keeps the LAN from listing/deleting linked devices or driving sync. Requires
/// the server to be started with `into_make_service_with_connect_info`; if the
/// peer address is unavailable the request is rejected (fail closed).
pub struct LoopbackOnly;

#[async_trait]
impl<S> FromRequestParts<S> for LoopbackOnly
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let is_loopback = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip().is_loopback())
            .unwrap_or(false);

        if is_loopback {
            Ok(LoopbackOnly)
        } else {
            Err((
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "This endpoint is local-only" })),
            ))
        }
    }
}

/// Extractor that admits only a non-browser caller speaking from loopback.
///
/// A loopback source is not proof of a trusted caller: the user's own browser also
/// speaks from 127.0.0.1, and the shared CORS layer replies
/// `Access-Control-Allow-Origin: *`, so a visited page could `fetch()` the endpoint
/// and read the answer. Browsers attach `Origin` to exactly these requests; native
/// clients (the MCP helper, the Flutter app over Dio) never do. Rejecting it also
/// blocks DNS rebinding, where the attacker's page is same-origin with the loopback
/// server.
///
/// Guard any loopback endpoint that returns owner data or secrets with this rather
/// than with [`LoopbackOnly`] alone.
pub struct LoopbackNoBrowser;

#[async_trait]
impl<S> FromRequestParts<S> for LoopbackNoBrowser
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        LoopbackOnly::from_request_parts(parts, state).await?;

        if parts.headers.contains_key("origin") {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "This endpoint is not callable from a browser" })),
            ));
        }

        Ok(LoopbackNoBrowser)
    }
}

/// Extractor guarding the MCP endpoint, which serves the OWNER view of the
/// library (private books, loans, statistics).
///
/// Three conditions, all fail-closed:
///
/// 1. **Loopback source.** Keeps LAN peers out; they reach this same 0.0.0.0
///    listener for catalogue traffic.
/// 2. **No `Origin` header.** See [`LoopbackNoBrowser`].
/// 3. **Bearer token.** The shared secret from the copied assistant configuration,
///    compared in constant time. This is the condition that holds when the other
///    two are subverted.
pub struct McpAuth;

#[async_trait]
impl<S> FromRequestParts<S> for McpAuth
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        LoopbackNoBrowser::from_request_parts(parts, state).await?;

        // No token configured means no way to authenticate: reject, never admit.
        let expected = crate::infrastructure::mcp_token::expected_token().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "MCP token unavailable" })),
        ))?;

        let presented = parts
            .headers
            .get("Authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or((
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing MCP token" })),
            ))?;

        if crate::infrastructure::mcp_token::constant_time_eq(
            presented.as_bytes(),
            expected.as_bytes(),
        ) {
            Ok(McpAuth)
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Invalid MCP token" })),
            ))
        }
    }
}

pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let password_hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| e.to_string())?
        .to_string();
    Ok(password_hash)
}

pub fn verify_password(password: &str, password_hash: &str) -> Result<bool, String> {
    let parsed_hash = PasswordHash::new(password_hash).map_err(|e| e.to_string())?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}

fn get_jwt_secret() -> String {
    env::var("JWT_SECRET").unwrap_or_else(|_| {
        if cfg!(debug_assertions) {
            "secret".to_string()
        } else {
            panic!("JWT_SECRET environment variable must be set in production");
        }
    })
}

pub fn create_jwt(username: &str, role: &str) -> Result<String, String> {
    let secret = get_jwt_secret();
    let expiration = Utc::now()
        .checked_add_signed(Duration::hours(24))
        .expect("valid timestamp")
        .timestamp();

    let claims = Claims {
        sub: username.to_owned(),
        role: role.to_owned(),
        exp: expiration as usize,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| e.to_string())
}

pub fn decode_jwt(token: &str) -> Result<Claims, String> {
    let secret = get_jwt_secret();
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map(|data| data.claims)
    .map_err(|e| e.to_string())
}
