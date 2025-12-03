# Security Audit & Mitigation Plan

> [!IMPORTANT]
> **Status**: Initial Assessment
> **Date**: 2025-12-03
> **Scope**: Rust Backend, Flutter App, P2P Protocol

## 🛡️ Critical Risks Identified

### 1. Server-Side Request Forgery (SSRF) in P2P

**Risk**: Low (Mitigated)
**Location**: `src/api/peer.rs` (`connect`, `proxy_search`)
**Description**: URL validation now blocks loopback, link-local, and cloud metadata addresses. Redirects are disabled.
**Mitigation**:

- [x] **Allowlist**: Blocked dangerous IP ranges.
- [x] **Validation**: `validate_url` function implemented.
- [x] **Safe Client**: `get_safe_client` disables redirects.

### 2. Cross-Origin Resource Sharing (CORS) Permissiveness

**Risk**: Low (Mitigated)
**Location**: `src/main.rs`
**Description**: `CorsLayer` is now configured to restrict origins.
**Mitigation**:

- [x] **Restrict Origin**: Default allows only `localhost`, `127.0.0.1`, and `localhost:3000`.
- [x] **Configurable**: `CORS_ALLOWED_ORIGINS` env var supported.

### 3. Sensitive Data Logging

**Risk**: Low (Mitigated)
**Location**: `src/auth.rs`, `src/api/peer.rs`
**Description**: Replaced `println!` with structured `tracing` macros (`info!`, `warn!`, `error!`).
**Mitigation**:

- [x] **Audit**: Scanned codebase for `println!`.
- [x] **Replace**: Switched to `tracing` crate.
- [x] **Sanitize**: Ensured no sensitive data (passwords) is logged.ver logged.

### 4. Rate Limiting

**Risk**: Medium
**Location**: Global
**Description**: No rate limiting is currently implemented. Brute-force attacks on `/auth/login` are possible.
**Mitigation**:

- [ ] **Middleware**: Add `tower_governor` or similar middleware to limit requests per IP.

### 5. Input Validation

**Risk**: Low
**Location**: API Endpoints
**Description**: While Rust types provide some safety, string fields (like book titles, ISBNs) lack strict validation (length, format).
**Mitigation**:

- [ ] **Validator Crate**: Use `validator` crate to enforce constraints on DTOs.

## 🔒 Action Plan

### Immediate Fixes (v1.0)

1. **Fix CORS**: Restrict to localhost by default.
2. **Secure Logging**: Audit and replace `println!`.
3. **SSRF Basic Protection**: Block metadata service IPs.

### Post-Launch (v1.1)

1. **Rate Limiting**: Implement login throttling.
2. **Advanced P2P Security**: Mutual TLS (mTLS) or signed requests.

## 🧪 Security Tests (In Progress)

- `tests/security_test.rs`: Verifies Password Hashing & JWT.
- `tests/api_error_handling_test.rs`: Verifies Error Responses.
