# Build stage
# Using Rust 1.92 for Edition 2024 support (matching local version)
FROM rust:1.92-bookworm AS builder

WORKDIR /app

# Install dependencies for building
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests and build.rs (needed for dependency build)
# Note: mock-github-oauth and rules Cargo.toml are needed because they're workspace members
COPY Cargo.toml Cargo.lock ./
COPY server/Cargo.toml ./server/
COPY server/build.rs ./server/
COPY mock-github-oauth/Cargo.toml ./mock-github-oauth/
COPY rules/Cargo.toml ./rules/

# Create dummy files for dependency caching
# Note: arena has both lib.rs and main.rs, plus bin/arena-cli.rs and bin/stress_test.rs
# The dummy lib.rs needs cli::config module stub since arena-cli imports it
RUN mkdir -p server/src/bin server/src/cli mock-github-oauth/src rules/src && \
    echo "fn main() {}" > server/src/main.rs && \
    echo "pub mod cli;" > server/src/lib.rs && \
    echo "pub mod config;" > server/src/cli/mod.rs && \
    echo "pub struct AuthConfig { pub token: Option<String> } pub struct CliConfig { pub auth: Option<AuthConfig> } impl CliConfig { pub fn load() -> color_eyre::Result<Self> { todo!() } pub fn api_url(&self) -> &str { todo!() } pub fn save(&self) -> color_eyre::Result<()> { todo!() } }" > server/src/cli/config.rs && \
    echo "fn main() {}" > server/src/bin/arena-cli.rs && \
    echo "fn main() {}" > server/src/bin/stress_test.rs && \
    echo "fn main() {}" > mock-github-oauth/src/main.rs && \
    echo "" > mock-github-oauth/src/lib.rs && \
    echo "" > rules/src/lib.rs

# Build dependencies only (for caching)
# VERGEN_IDEMPOTENT allows build without .git directory (uses placeholder values)
RUN VERGEN_IDEMPOTENT=1 cargo build --release --package arena

# Remove dummy files
RUN rm -rf server/src

# Copy actual source code (only arena, not mock-github-oauth)
COPY server/src ./server/src
COPY server/static ./server/static
COPY migrations ./migrations
COPY .sqlx ./.sqlx

# Copy .git directory for vergen to extract real git info in final binary
COPY .git ./.git

# Set SQLX offline mode
ENV SQLX_OFFLINE=true

# Touch the main.rs to ensure rebuild with actual source
RUN touch server/src/main.rs

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
