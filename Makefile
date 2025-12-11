.PHONY: start dev test clean help

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
	@echo "  make clean          # Clean build artifacts"
