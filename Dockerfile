# Multi-stage build for Rust server
FROM rust:latest as builder

WORKDIR /app

# Copy manifests
COPY Cargo.toml ./

# Build dependencies only (caching step)
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm -rf src

# Copy source code
COPY src ./src
COPY migrations ./migrations

# Build release binary (app only)
RUN touch src/main.rs
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install SQLite
RUN apt-get update && apt-get install -y \
    sqlite3 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the built binary from the builder stage
COPY --from=builder /app/target/release/bibliogenius /usr/local/bin/bibliogenius

# Copy static files for Web UI
COPY static /app/static
COPY migrations ./migrations
COPY .env.example .env

# Create data directory
RUN mkdir -p /app/data

# Expose port
EXPOSE 8000

# Run the binary
CMD ["bibliogenius"]
