# Build stage
# Rust 1.99 is required by cja-build / rolldown.
FROM rust:1.99-bookworm AS builder

WORKDIR /app

# Install dependencies for building
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests and build.rs (needed for dependency build)
# Note: mock-github-oauth and rules Cargo.toml are needed because they're workspace members
# engine-proxy Cargo.toml is needed for the same reason (DEV-1943)
COPY Cargo.toml Cargo.lock ./
COPY server/Cargo.toml ./server/
COPY server/build.rs ./server/
COPY server/frontend ./server/frontend
COPY server/static ./server/static
COPY mock-github-oauth/Cargo.toml ./mock-github-oauth/
COPY rules/Cargo.toml ./rules/
COPY engine-proxy/Cargo.toml ./engine-proxy/
COPY arena-api-scope/Cargo.toml ./arena-api-scope/
COPY cli/Cargo.toml ./cli/

# Create dummy files for dependency caching
# Note: arena has both lib.rs and main.rs, plus bin/stress_test.rs; every
# workspace member's targets must exist for manifest parsing. cli keeps
# its dummy sources through the final build (like mock-github-oauth):
# -p arena only parses its manifest. arena-api-scope gets real sources
# later because arena compiles it.
RUN mkdir -p server/src/bin mock-github-oauth/src rules/src engine-proxy/src arena-api-scope/src cli/src && \
    echo "fn main() {}" > server/src/main.rs && \
    echo "" > server/src/lib.rs && \
    echo "fn main() {}" > server/src/bin/stress_test.rs && \
    echo "fn main() {}" > mock-github-oauth/src/main.rs && \
    echo "" > mock-github-oauth/src/lib.rs && \
    echo "" > rules/src/lib.rs && \
    echo "fn main() {}" > engine-proxy/src/main.rs && \
    echo "" > engine-proxy/src/lib.rs && \
    echo "" > arena-api-scope/src/lib.rs && \
    echo "" > cli/src/lib.rs && \
    echo "fn main() {}" > cli/src/main.rs

# Build dependencies only (for caching)
# VERGEN_IDEMPOTENT allows build without .git directory (uses placeholder values)
RUN VERGEN_IDEMPOTENT=1 cargo build --release --package arena

# Remove dummy files
RUN rm -rf server/src rules/src arena-api-scope/src

# Copy actual source code (arena and rules, not mock-github-oauth or cli;
# arena-api-scope is compiled by arena so it needs real sources too)
COPY server/src ./server/src
COPY rules/src ./rules/src
COPY arena-api-scope/src ./arena-api-scope/src
COPY migrations ./migrations
COPY .sqlx ./.sqlx

# Copy .git directory for vergen to extract real git info in final binary
COPY .git ./.git

# Set SQLX offline mode
ENV SQLX_OFFLINE=true

# Touch source files to ensure rebuild with actual source
RUN touch server/src/main.rs rules/src/lib.rs arena-api-scope/src/lib.rs

# Build the application (with real git info from .git)
RUN cargo build --release --package arena

# Runtime stage
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy the binary from builder
COPY --from=builder /app/target/release/arena /app/arena

# Cloud Run uses PORT env var, default to 8080
ENV PORT=8080

# Expose the port
EXPOSE 8080

# Run the application
CMD ["/app/arena"]
