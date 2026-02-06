# Security Audit & Mitigation Plan

> **Status**: Ongoing
> **Last Updated**: 2026-02-06
> **Scope**: Rust Backend, Flutter App, P2P Protocol

## Critical Risks

### 1. SQL Injection in Setup Endpoint

**Severity**: CRITICAL | **Status**: Open
**Location**: `src/api/setup.rs:131-136, 179-184`

User-controlled `admin_username` is directly interpolated into raw SQL queries via `format!()`, bypassing SeaORM's parameterized query system.

```rust
// VULNERABLE — admin_username is not escaped
format!("SELECT COUNT(*) FROM users WHERE username = '{}'", admin_username)
```

**Impact**: Authentication bypass, arbitrary SQL execution, data exfiltration.

**Mitigation**:

- [ ] Replace with SeaORM parameterized queries or use `sea_orm::Statement::from_sql_and_values()`.

---

### 2. Default Credentials on Setup

**Severity**: CRITICAL | **Status**: Open
**Location**: `src/api/setup.rs:120-127`

The `/api/setup` endpoint defaults to `admin:admin` when credentials are not provided in the request.

**Impact**: Uninitialized instances are trivially compromisable.

**Mitigation**:

- [ ] Require `admin_username` and `admin_password` as mandatory fields (reject the request if missing).
- [ ] Enforce minimum password complexity.

---

### 3. SSRF in P2P

**Severity**: Low (Mitigated)
**Location**: `src/api/peer.rs` (`connect`, `proxy_search`)

URL validation now blocks loopback, link-local, and cloud metadata addresses. Redirects are disabled.

**Mitigation**:

- [x] Blocked dangerous IP ranges via `validate_url`.
- [x] `get_safe_client` disables redirects.

---

### 4. CORS Permissiveness

**Severity**: Low (Mitigated)
**Location**: `src/main.rs`

`CorsLayer` is now configured to restrict origins.

**Mitigation**:

- [x] Default allows only `localhost`, `127.0.0.1`, and `localhost:3000`.
- [x] `CORS_ALLOWED_ORIGINS` env var supported for custom origins.

---

### 5. Sensitive Data Logging

**Severity**: Low (Mitigated)
**Location**: `src/infrastructure/auth.rs`, `src/api/peer.rs`

Replaced `println!` with structured `tracing` macros (`info!`, `warn!`, `error!`).

**Mitigation**:

- [x] Scanned codebase for `println!`.
- [x] Switched to `tracing` crate.
- [x] Ensured no sensitive data (passwords, tokens) is logged.

---

### 6. Weak JWT Secret in Debug Mode

**Severity**: HIGH | **Status**: Open
**Location**: `src/infrastructure/auth.rs:75-83`

JWT secret defaults to `"secret"` when `JWT_SECRET` env var is not set in debug builds. Release builds panic correctly, but a debug build accidentally deployed would be fully compromisable.

**Impact**: JWT tokens can be forged, leading to complete authentication bypass.

**Mitigation**:

- [ ] Log a visible warning when the fallback secret is used.
- [ ] Consider requiring `JWT_SECRET` in all modes, or generate a random secret at startup.

---

### 7. No Token Revocation / Refresh Mechanism

**Severity**: MEDIUM-HIGH | **Status**: Open
**Location**: `src/infrastructure/auth.rs:87-90`

JWT tokens are valid for 24 hours with no way to revoke them before expiry. No refresh token mechanism exists.

**Impact**: A compromised token grants access for the full 24-hour window.

**Mitigation**:

- [ ] Implement short-lived access tokens (e.g. 15 min) with a refresh token flow.
- [ ] Add a server-side revocation list for logout / password change scenarios.

---

### 8. Path Traversal / File Upload in Scanner

**Severity**: HIGH | **Status**: Open
**Location**: `src/api/scan.rs:28-36`

Uploaded files are saved to `/tmp` with a UUID-based name. While the filename itself is safe (UUID), there are no checks on:
- File size (no upload limit — risk of disk exhaustion).
- MIME type (any content accepted as `.jpg`).

**Impact**: Denial of service via disk exhaustion. No path traversal in practice (UUID filename), but no defense in depth.

**Mitigation**:

- [ ] Enforce a maximum file size (e.g., 10 MB).
- [ ] Validate MIME type / magic bytes before saving.

---

### 9. Error Information Leakage

**Severity**: HIGH | **Status**: Open
**Location**: Multiple API handlers (9+ files)

Database errors are returned directly to the API consumer via `e.to_string()`, potentially exposing schema details, table/column names, constraint names, and file paths.

**Affected files**: `api/setup.rs`, `api/peer.rs`, `api/loan.rs`, `api/collections.rs`, `api/author.rs`, `api/books.rs`, `api/batch.rs`, `api/auth.rs`, `api/tag.rs`, `api/user.rs`, `api/scan.rs`.

**Impact**: Aids reconnaissance for attackers.

**Mitigation**:

- [ ] Map all `DbErr` to generic user-facing messages (`"Internal server error"`).
- [ ] Log detailed errors server-side only via `tracing::error!`.

---

### 10. Rate Limiting

**Severity**: MEDIUM | **Status**: Open
**Location**: Global

No rate limiting is currently implemented. Brute-force attacks on `/auth/login` and other endpoints are possible.

**Mitigation**:

- [ ] Add `tower_governor` or similar middleware to limit requests per IP.

---

### 11. Input Validation

**Severity**: LOW | **Status**: Open
**Location**: API endpoints

While Rust types provide some safety, string fields (book titles, ISBNs, usernames) lack strict validation on length and format.

**Mitigation**:

- [ ] Use `validator` crate to enforce constraints on DTOs.

---

### 12. P2P Protocol Trusts Incoming Data

**Severity**: MEDIUM | **Status**: Open
**Location**: `src/api/peer.rs:659-680`

The `push_operations` endpoint accepts arbitrary operations from peers without authentication or schema validation. Fields like `entity_type`, `operation`, `payload`, and `created_at` are stored as-is.

**Impact**: Malicious peers can inject fake operations, backdate timestamps, or flood the operation log.

**Mitigation**:

- [ ] Require peer authentication (HMAC signatures or mutual TLS).
- [ ] Validate operation schemas (allowed entity types and operation names).
- [ ] Reject operations with implausible `created_at` timestamps.
- [ ] Rate limit per peer.

---

### 13. SQLite File Permissions

**Severity**: MEDIUM | **Status**: Open
**Location**: `src/infrastructure/db.rs`

SQLite database files (`.db`, `.db-wal`, `.db-shm`) are created with default OS permissions. On multi-user systems, this may allow other local users to read the database.

**Mitigation**:

- [ ] Set file permissions to `0600` after database creation.

---

### 14. HTTP Client Created Per-Request

**Severity**: MEDIUM | **Status**: Open (Technical Debt)
**Location**: `src/modules/integrations/openlibrary.rs`, `src/api/loan.rs`, `src/api/hub.rs`, `src/api/integrations.rs`, `src/api/peer.rs`

Several integration modules create a new `reqwest::Client` for each request instead of reusing a shared client from `AppState`. This wastes resources (TLS handshake per call) and under load can lead to resource exhaustion.

**Mitigation**:

- [ ] Migrate all HTTP calls to the shared `AppState.http_client`.

---

## Remediation Priority

### Block Release (v1.0)

1. Fix SQL injection in `setup.rs` (use parameterized queries)
2. Require credentials on setup (reject default `admin:admin`)
3. Sanitize all error messages returned to API consumers

### Pre-Launch

4. Enforce file size limits on scanner upload
5. Add JWT secret warning in debug mode
6. Set SQLite file permissions to `0600`
7. Run `cargo audit` and fix any CVEs

### Post-Launch (v1.1)

8. Implement refresh tokens with short-lived access tokens
9. Add rate limiting middleware
10. Add input validation via `validator` crate
11. Authenticate P2P peer operations
12. Migrate all HTTP clients to shared instance

---

## Security Tests

- `tests/security_test.rs` — Password hashing (Argon2), JWT creation/verification, login flow (success, wrong password, unknown user).
- `tests/api_error_handling_test.rs` — Verifies error response format and status codes for CRUD operations.

---

## Positive Findings

The following security measures are already in place:

- Password hashing with Argon2 (`infrastructure/auth.rs`)
- JWT expiry enforced (24h) with validation (`infrastructure/auth.rs`)
- SSRF protection via `validate_url()` blocking localhost/metadata IPs (`api/peer.rs`)
- CORS restricted to localhost by default
- Structured logging via `tracing` (no `println!` with sensitive data)
- SeaORM parameterized queries used in all handlers except `setup.rs`
- CSV import properly escapes ISBNs (`modules/import/`)
