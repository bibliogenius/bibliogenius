.PHONY: start dev test test-loopback test-lan-only test-two-instances clean help

# Default port
PORT ?= 8000

# Start backend with auto-recompile and Flutter .env sync
start:
	@./scripts/start_backend.sh $(PORT)

# Start in development mode (debug build, faster compile)
dev:
	@echo "🔨 Compiling (debug mode)..."
	@cargo build
	@echo "🚀 Starting on port $(PORT)..."
	@PORT=$(PORT) ./target/debug/bibliogenius

# Run tests
test:
	@cargo test
	@echo "🧪 Running API Regression Tests..."
	@./tests/verify_filters.sh

# Unit tests that need a local mock server behind `validate_url` (see
# TESTING.md). Not part of `make test`: needs the it-loopback feature, no
# Docker. Runs first in `test-two-instances`.
test-loopback:
	@cargo test --features it-loopback --lib plain_http_identity_gate

# Two unregistered backends paired over loopback against a fake hub that
# must see no request (ADR-073). Needs the it-loopback feature, no Docker.
# Runs before the hub starts in `test-two-instances`.
test-lan-only:
	@cargo test --features it-loopback --test lan_only_pairing $(ARGS)

# Two backends against the throwaway dev hub (see TESTING.md). Not part of
# `make test`: needs Docker and the it-loopback feature. The hub is stopped
# and its database wiped whatever the outcome. ARGS scopes the run, e.g.
# ARGS="an_ended_pairing -- --nocapture".
test-two-instances: test-loopback test-lan-only
	@$(MAKE) -C ../bibliogenius-hub it-up
	@cargo test --features it-loopback --test two_instances_hub $(ARGS); \
	  status=$$?; $(MAKE) -C ../bibliogenius-hub it-down; exit $$status

# Clean build artifacts
clean:
	@cargo clean
	@echo "🧹 Build artifacts cleaned"

# Show help
help:
	@echo "BiblioGenius Backend Makefile"
	@echo ""
	@echo "Usage:"
	@echo "  make start          # Build (release) and run on port 8000"
	@echo "  make start PORT=8002 # Build and run on custom port"
	@echo "  make dev            # Quick debug build and run"
	@echo "  make test           # Run tests"
	@echo "  make test-lan-only  # Two unregistered backends, the hub must see nothing"
	@echo "  make test-two-instances # Two backends against the throwaway dev hub"
	@echo "  make clean          # Clean build artifacts"
