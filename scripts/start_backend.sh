#!/bin/bash
# BiblioGenius Backend Launcher
# Ensures the backend is always running with the latest compiled code.
#
# Usage:
#   ./start_backend.sh                  # Default: port 8000, default database
#   ./start_backend.sh 8002             # Custom port
#   ./start_backend.sh 8002 ~/my_lib.db # Custom port + custom database
#
# The script will:
# 1. Recompile the backend (release mode for speed)
# 2. Start the server on the specified port
# 3. Automatically update the Flutter app's .env file

set -e  # Exit on any error

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
APP_DIR="$PROJECT_ROOT/../bibliogenius-app"

PORT="${1:-8000}"
DATABASE="${2:-}"

echo "🔨 Compiling backend (release mode)..."
cd "$PROJECT_ROOT"
cargo build --release

echo "✅ Compilation successful!"

# Update Flutter .env if the app directory exists
if [ -d "$APP_DIR" ]; then
    ENV_FILE="$APP_DIR/.env"
    if [ -f "$ENV_FILE" ]; then
        # Use sed to update API_BASE_URL (macOS compatible)
        sed -i '' "s|^API_BASE_URL=.*|API_BASE_URL=http://localhost:$PORT|" "$ENV_FILE"
        echo "📝 Updated $ENV_FILE with port $PORT"
    fi
fi

echo "🚀 Starting BiblioGenius on port $PORT..."

# Set environment variables
export PORT="$PORT"
if [ -n "$DATABASE" ]; then
    export DATABASE_URL="sqlite://$DATABASE?mode=rwc"
    echo "📚 Using database: $DATABASE"
fi

# Run the server
exec "$PROJECT_ROOT/target/release/bibliogenius"
