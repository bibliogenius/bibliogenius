.PHONY: start dev test test-two-instances clean help

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

# Two backends against the throwaway dev hub (see TESTING.md). Not part of
# `make test`: needs Docker and the it-loopback feature. The hub is stopped
# and its database wiped whatever the outcome. ARGS scopes the run, e.g.
# ARGS="an_ended_pairing -- --nocapture".
test-two-instances:
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
	@echo "  make test-two-instances # Two backends against the throwaway dev hub"
	@echo "  make clean          # Clean build artifacts"
