# Running BiblioGenius Tests

## Prerequisites

Rust and Cargo must be installed on your development machine.

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Running All Tests

From the `bibliogenius` directory:

```bash
cargo test
```

## Running Specific Test Suites

### Integration Tests (P2P, CRUD, Loans, Sync)

```bash
cargo test --test api_integration_test
```

### Error Handling Tests (API status codes, response format)

```bash
cargo test --test api_error_handling_test
```

### Security Tests (Auth, JWT, Passwords)

```bash
cargo test --test security_test
```

### Gamification Tests (Achievements, Streaks, Progress)

```bash
cargo test --test gamification_test
```

### Unit Tests Only (in-module tests)

```bash
cargo test --lib
```

### Run a Specific Test

```bash
cargo test test_cannot_accept_request_without_available_copy
```

## Running Tests with Output

To see `println!` statements and detailed output:

```bash
cargo test -- --nocapture
```

## Test Inventory

The project currently has **43 tests** across 4 test files and in-module unit tests.

### Integration Tests — `tests/api_integration_test.rs` (16 tests)

#### P2P Borrow Flow

| Test | What it verifies |
|------|-----------------|
| `test_cannot_accept_request_without_available_copy` | Accepting a borrow request fails when no copy exists |
| `test_can_accept_request_with_available_copy` | Successful borrow when copy is available |
| `test_cannot_accept_request_when_copy_is_borrowed` | Already-borrowed copies can't be re-borrowed |
| `test_borrow_request_auto_approve` | Auto-approval logic |
| `test_p2p_connect` | Peer registration |

#### Loan Returns

| Test | What it verifies |
|------|-----------------|
| `test_loan_return_deletes_book_when_not_owned_and_no_copies` | Book deleted on return if not owned and no copies left |
| `test_loan_return_deletes_borrowed_copy` | Borrowed copy removed on return |
| `test_loan_return_keeps_book_if_has_other_copies` | Book kept if other copies exist |
| `test_loan_return_keeps_book_if_owned` | Owned books are never deleted on return |
| `test_loan_return_keeps_book_if_wishlist` | Wishlisted books are kept on return |

#### CRUD & Sync

| Test | What it verifies |
|------|-----------------|
| `test_book_crud` | Book create / read / update / delete |
| `test_library_exists_after_admin_creation` | Database migration creates default library |
| `test_copy_creation_requires_valid_library` | Foreign key constraints enforced |
| `test_sync_clears_old_peer_books` | Sync replaces old peer book cache |
| `test_inventory_sync` | Mock server sync (using wiremock) |
| `test_search_unified_endpoint` | Unified search returns expected results |

### Error Handling Tests — `tests/api_error_handling_test.rs` (14 tests)

| Test | What it verifies |
|------|-----------------|
| `test_create_book_success` | Book creation returns 201 |
| `test_create_book_invalid_input` | Invalid input returns 400 |
| `test_get_book_not_found` | Missing book returns 404 |
| `test_update_book_success` | Book update returns 200 |
| `test_delete_book_idempotency` | Deleting a missing book is idempotent |
| `test_list_books_with_pagination` | Pagination query params work |
| `test_collection_crud_via_repository` | Collection CRUD via repository layer |
| `test_collection_book_operations` | Adding/removing books from collections |
| `test_get_collection_not_found` | Missing collection returns 404 |
| `test_copy_crud_via_repository` | Copy CRUD via repository layer |
| `test_borrowed_copies_via_repository` | Borrowed copies listing |
| `test_get_copy_not_found` | Missing copy returns 404 |
| `test_update_copy_not_found` | Updating missing copy returns 404 |
| `test_author_crud_via_repository` | Author CRUD via repository layer |

### Security Tests — `tests/security_test.rs` (3 tests)

| Test | What it verifies |
|------|-----------------|
| `test_password_hashing` | Argon2 hash + verify (correct & wrong password) |
| `test_jwt_creation_and_verification` | JWT encode/decode round-trip |
| `test_login_flow` | Full login: success, wrong password, unknown user |

### Gamification Tests — `tests/gamification_test.rs` (10 tests)

| Test | What it verifies |
|------|-----------------|
| `test_gamification_tables_created` | Migration creates gamification tables |
| `test_gamification_config_model` | Config model CRUD |
| `test_gamification_progress_model` | Progress model CRUD |
| `test_gamification_achievements_model` | Achievements model CRUD |
| `test_gamification_streaks_model` | Streaks model CRUD |
| `test_collector_track_counts_books` | Collector track counts books correctly |
| `test_reader_track_counts_read_books` | Reader track counts read books |
| `test_track_thresholds` | Level thresholds work as expected |
| `test_multiple_achievements_per_user` | Multiple achievements per user |
| `test_unique_achievement_constraint` | Duplicate achievements rejected |

### Unit Tests — in-module (10 tests)

| Test | Module |
|------|--------|
| `test_parse_inventaire_csv` | `modules::import` |
| `test_parse_inventaire_json` | `modules::import` |
| `test_lookup_bnf_sru` | `modules::integrations::bnf` |
| `test_search_bnf` | `modules::integrations::bnf` |
| `test_search_bnf_sru` | `modules::integrations::bnf` |
| `test_search_inventaire` | `modules::integrations::inventaire` |
| `test_search_with_enrichment` | `modules::integrations::inventaire` |
| `test_fetch_inventaire_metadata` | `modules::integrations::inventaire` |
| `test_service_type_format` | `services::mdns` |
| `test_apply_book_create_operation` | `sync::processor` |

## Shell-Based Regression Tests

The Makefile also runs `tests/verify_filters.sh`, a shell script that tests API filter endpoints against a running server with seeded data. This is not part of `cargo test` and requires the backend to be running.

```bash
make test   # runs cargo test + verify_filters.sh
```

## Helper Functions

### `api_integration_test.rs`

- `setup_test_db()` — In-memory SQLite database
- `create_test_admin(db)` — Creates admin user
- `create_test_library(db, owner_id, name)` — Creates library
- `create_test_book(db, title, isbn)` — Creates book
- `create_test_copy(db, book_id, library_id, status)` — Creates copy
- `create_test_peer(db, name, url)` — Creates peer
- `create_test_request(db, id, peer_id, isbn, title, status)` — Creates borrow request

### `api_error_handling_test.rs`

- `setup_test_state()` — In-memory DB wrapped in `AppState`
- `create_test_admin(db)` — Creates admin user
- `create_auth_token()` — Creates a valid JWT for authenticated requests

### `gamification_test.rs`

- `setup_test_db()` — In-memory SQLite database
- `create_test_admin(db)` — Creates a test user for gamification

## Adding New Tests

```rust
#[tokio::test]
async fn test_your_feature() {
    let db = setup_test_db().await;

    // Setup test data using helpers
    let admin_id = create_test_admin(&db).await;
    let library_id = create_test_library(&db, admin_id, "Test Library").await;

    // Test your feature
    // ...

    // Assert expected behavior
    assert_eq!(actual, expected);
}
```

## Troubleshooting

### "command not found: cargo"

Install Rust using the command in Prerequisites above.

### "failed to connect to database"

Tests use in-memory SQLite — no external database is required.

### Tests are slow

Use the `--release` flag for optimized builds:

```bash
cargo test --release
```

### A unit test calls an external API and fails

Some unit tests (`test_search_bnf`, `test_fetch_inventaire_metadata`, etc.) hit live external APIs. They may fail if the service is down or your network is unavailable. This is expected — these are effectively smoke tests, not hermetic unit tests.
