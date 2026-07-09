//! Local authentication token for the MCP endpoint.
//!
//! The MCP endpoint exposes the OWNER view of the library (private books, loans,
//! statistics) on a router that is bound to `0.0.0.0`. The loopback guard alone is
//! not enough: the user's own web browser speaks from `127.0.0.1`, so any visited
//! page could otherwise `fetch()` the endpoint and read the reply (the shared CORS
//! layer answers `Access-Control-Allow-Origin: *`). A secret the page cannot guess
//! is what actually closes that hole, and it also survives DNS rebinding, where the
//! attacker's page becomes same-origin with the loopback server.
//!
//! The token is a 256-bit random value stored next to the database, readable only by
//! the user (`0600` where the platform supports it). It is generated on first use and
//! kept across restarts, because the copied assistant configuration embeds it: a token
//! regenerated at every launch would silently break that configuration.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Environment variable carrying the token to the stdio helper, which the AI
/// assistant spawns with the environment block from the copied configuration.
pub const TOKEN_ENV_VAR: &str = "BIBLIOGENIUS_MCP_TOKEN";

/// File name of the token, stored in the database directory.
const TOKEN_FILE_NAME: &str = "mcp_token";

/// Cached token for the running process. Only a successful resolution is cached: a
/// transient failure (directory not yet writable at first call) must not condemn the
/// endpoint to answer 503 until the process restarts.
static TOKEN: OnceLock<String> = OnceLock::new();

/// Serializes resolution attempts, so two threads racing on a missing token file
/// cannot each generate one and have the loser cache a value the file no longer holds.
static RESOLVE_LOCK: Mutex<()> = Mutex::new(());

/// Generate a 256-bit random token, base64url-encoded.
fn generate_token() -> String {
    use base64::Engine;
    use rand::RngCore;
    use rand::rngs::OsRng;

    let mut bytes = [0u8; 32];
    // Cryptographic randomness comes from the OS (SECURITY_GUIDELINES part E).
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Extract the database file path from a SeaORM SQLite URL.
///
/// Returns `None` for in-memory databases (tests), which have no directory to
/// store a token in.
fn database_file_path(database_url: &str) -> Option<PathBuf> {
    let rest = database_url.strip_prefix("sqlite:")?;
    // Both `sqlite:<path>` and `sqlite://<path>` appear in the wild; the app itself
    // uses the single-colon form (a double slash breaks on "Application Support").
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    // `sqlite::memory:` and `sqlite://:memory:` have no backing file.
    if rest.starts_with(':') {
        return None;
    }
    let file = rest.split('?').next()?;
    if file.is_empty() {
        return None;
    }
    Some(PathBuf::from(file))
}

/// Absolute path of the token file for a given database URL.
pub fn token_file_path(database_url: &str) -> Option<PathBuf> {
    Some(database_file_path(database_url)?.with_file_name(TOKEN_FILE_NAME))
}

/// Read the token from `path`, or create it if absent.
///
/// The file is created with owner-only permissions; an existing file is trusted as
/// is, so a user who tightened the mode keeps their choice. A mode that is looser
/// than owner-only is warned about rather than silently corrected.
fn load_or_create_at(path: &std::path::Path) -> std::io::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            #[cfg(unix)]
            warn_if_readable_beyond_owner(path);
            return Ok(trimmed.to_string());
        }
    }

    let token = generate_token();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        use std::io::Write;
        let mut file = options.open(path)?;
        file.write_all(token.as_bytes())?;
        file.flush()?;
    }
    Ok(token)
}

/// Warn when an existing token file is readable by anyone but its owner.
///
/// An existing file is trusted as is, so a user who tightened the mode keeps their
/// choice. But a file restored from a backup, or created under a permissive umask,
/// hands the owner-view secret to every local account without a word.
#[cfg(unix)]
fn warn_if_readable_beyond_owner(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        tracing::warn!(
            "MCP token file {} is readable beyond its owner (mode {:o}); run `chmod 600` on it",
            path.display(),
            mode
        );
    }
}

/// Resolve the token from `DATABASE_URL`, creating it on first use.
fn resolve_token() -> Option<String> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let path = token_file_path(&database_url)?;
    match load_or_create_at(&path) {
        Ok(token) => Some(token),
        Err(e) => {
            // Never log the token itself, only why it is unavailable.
            tracing::warn!("MCP token unavailable ({e}); MCP endpoint will reject calls");
            None
        }
    }
}

/// The token expected by this process, resolved from `DATABASE_URL` on first use.
///
/// `None` when no token could be established. Callers MUST treat that as "reject
/// every request" rather than "no authentication required". A failed resolution is
/// retried on the next call, so a directory that becomes writable later recovers
/// without a restart.
pub fn expected_token() -> Option<&'static str> {
    if let Some(token) = TOKEN.get() {
        return Some(token.as_str());
    }

    let _guard = RESOLVE_LOCK.lock().ok()?;
    // Another thread may have resolved it while we waited for the lock.
    if let Some(token) = TOKEN.get() {
        return Some(token.as_str());
    }

    let token = resolve_token()?;
    let _ = TOKEN.set(token);
    TOKEN.get().map(String::as_str)
}

/// Compare two secrets without leaking their content through timing.
///
/// Lengths are compared first: the length of a rejected candidate is not a secret.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_file_sits_next_to_the_database() {
        let path = token_file_path("sqlite:/tmp/lib/bibliogenius.db?mode=rwc")
            .expect("path for a file-backed database");
        assert_eq!(path, PathBuf::from("/tmp/lib/mcp_token"));
    }

    #[test]
    fn token_file_handles_the_double_slash_form_and_spaces() {
        let path = token_file_path("sqlite://Users/f/Application Support/bg.db")
            .expect("path for a file-backed database");
        assert_eq!(path, PathBuf::from("Users/f/Application Support/mcp_token"));
    }

    #[test]
    fn in_memory_databases_have_no_token_file() {
        // Tests run against `sqlite::memory:`; they must not create stray files,
        // and the endpoint must fail closed rather than run unauthenticated.
        assert!(token_file_path("sqlite::memory:").is_none());
        assert!(token_file_path("sqlite://:memory:").is_none());
        assert!(token_file_path("postgres://host/db").is_none());
    }

    #[test]
    fn generated_tokens_are_256_bit_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        // 32 bytes, base64url without padding.
        assert_eq!(a.len(), 43);
    }

    #[test]
    fn load_or_create_persists_the_first_token() {
        let dir = std::env::temp_dir().join(format!("bg-mcp-token-{}", generate_token()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(TOKEN_FILE_NAME);

        let first = load_or_create_at(&path).expect("create");
        let second = load_or_create_at(&path).expect("reuse");
        // A token regenerated at each launch would break the copied assistant config.
        assert_eq!(first, second);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "token must not be world-readable");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn constant_time_eq_matches_plain_equality() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
