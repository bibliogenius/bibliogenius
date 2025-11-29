# Running BiblioGenius Integration Tests

## Prerequisites

The tests require Rust and Cargo installed on your development machine.

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Running All Tests

From the `bibliogenius` directory:

```bash
cd /Users/federico/Sites/bibliotech/bibliogenius
cargo test
```

## Running Specific Test Suites

### Integration Tests Only

```bash
cargo test --test api_integration_test
```

### Unit Tests Only

```bash
cargo test --lib
```

### Run a Specific Test

```bash
cargo test test_cannot_accept_request_without_available_copy
```

## Running Tests with Output

To see println! statements and more detailed output:

```bash
cargo test -- --nocapture
```

## Understanding Test Results

### Example Output

```
running 11 tests
test test_book_crud ... ok
test test_p2p_connect ... ok
test test_cannot_accept_request_without_available_copy ... ok
test test_can_accept_request_with_available_copy ... ok
test test_cannot_accept_request_when_copy_is_borrowed ... ok
test test_library_exists_after_admin_creation ... ok
test test_copy_creation_requires_valid_library ... ok
test test_sync_clears_old_peer_books ... ok
test test_inventory_sync ... ok
test test_borrow_request_auto_approve ... ok

test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## Test Coverage

The integration tests cover:

### Critical P2P Borrow Flow Tests

1. **`test_cannot_accept_request_without_available_copy`**
   - ✅ **Would have caught the 409 bug!**
   - Verifies that accepting a request fails when no copy exists

2. **`test_can_accept_request_with_available_copy`**
   - Verifies successful borrow request when copy is available

3. **`test_cannot_accept_request_when_copy_is_borrowed`**
   - Verifies that borrowed copies can't be re-borrowed

4. **`test_library_exists_after_admin_creation`**
   - ✅ **Would have caught the missing library bug!**
   - Verifies the database migration creates default library

5. **`test_copy_creation_requires_valid_library`**
   - Verifies foreign key constraints are enforced

6. **`test_sync_clears_old_peer_books`**
   - Verifies sync completely replaces old cache

### Basic CRUD Tests

7. **`test_book_crud`** - Book create/read/update/delete
8. **`test_p2p_connect`** - Peer registration
9. **`test_inventory_sync`** - Mock server sync test
10. **`test_borrow_request_auto_approve`** - Auto-approval logic

## Continuous Integration

### Adding Tests to CI/CD

Create `.github/workflows/test.yml`:

```yaml
name: Test

on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      
      - name: Setup Rust
        uses: actions-rs/toolchain@v1
        with:
          toolchain: stable
      
      - name: Run tests
        run: cd bibliogenius && cargo test
      
      - name: Upload coverage
        uses: codecov/codecov-action@v3
```

## Troubleshooting

### "command not found: cargo"

Install Rust using the command above.

### "failed to connect to database"

Tests use in-memory SQLite, no database required.

### Tests are slow

Add `--release` flag for optimized builds:

```bash
cargo test --release
```

## Adding New Tests

### Test Structure

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

### Helper Functions Available

- `create_test_admin(db)` - Creates admin user
- `create_test_library(db, owner_id, name)` - Creates library
- `create_test_book(db, title, isbn)` - Creates book
- `create_test_copy(db, book_id, library_id, status)` - Creates copy
- `create_test_peer(db, name, url)` - Creates peer
- `create_test_request(db, id, peer_id, isbn, title, status)` - Creates borrow request
