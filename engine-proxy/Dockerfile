# Build stage — must match the runtime stage's glibc (bookworm == cc-debian12,
# both glibc 2.36). Never drop the -bookworm suffix: re-based rust: tags drift
# glibc and crash at runtime with GLIBC_2.39-not-found while CI stays green.
FROM rust:1.99-bookworm AS builder

WORKDIR /app

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# All workspace member manifests are needed to resolve the workspace; the
# dummy sources keep the dependency-cache layer from compiling real code.
COPY Cargo.toml Cargo.lock ./
COPY server/Cargo.toml ./server/
COPY server/build.rs ./server/
COPY mock-github-oauth/Cargo.toml ./mock-github-oauth/
COPY rules/Cargo.toml ./rules/
COPY engine-proxy/Cargo.toml ./engine-proxy/

RUN mkdir -p server/src/bin server/src/cli mock-github-oauth/src rules/src engine-proxy/src && \
    echo "fn main() {}" > server/src/main.rs && \
    echo "pub mod cli;" > server/src/lib.rs && \
    echo "pub mod config;" > server/src/cli/mod.rs && \
    echo "pub struct AuthConfig { pub token: Option<String> } pub struct CliConfig { pub auth: Option<AuthConfig> } impl CliConfig { pub fn load() -> color_eyre::Result<Self> { todo!() } pub fn api_url(&self) -> &str { todo!() } pub fn save(&self) -> color_eyre::Result<()> { todo!() } }" > server/src/cli/config.rs && \
    echo "fn main() {}" > server/src/bin/arena-cli.rs && \
    echo "fn main() {}" > server/src/bin/stress_test.rs && \
    echo "fn main() {}" > mock-github-oauth/src/main.rs && \
    echo "" > mock-github-oauth/src/lib.rs && \
    echo "" > rules/src/lib.rs && \
    echo "fn main() {}" > engine-proxy/src/main.rs && \
    echo "" > engine-proxy/src/lib.rs

# Package-scoped: never compiles arena's sqlx::query! macros, so no
# DATABASE_URL / .sqlx / SQLX_OFFLINE involved. No secrets needed.
RUN VERGEN_IDEMPOTENT=1 cargo build --release -p engine-proxy

RUN rm -rf engine-proxy/src
COPY engine-proxy/src ./engine-proxy/src
RUN touch engine-proxy/src/main.rs engine-proxy/src/lib.rs
RUN cargo build --release -p engine-proxy && ls -lh target/release/engine

# Runtime: distroless cc (glibc, CA certs), non-root, no shell.
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /app/target/release/engine /usr/local/bin/engine

# RUST_LOG is baked (DD 16): cja's default filter would print 2-3 lines per
# request plus an ERROR on every snake 5xx; with this filter the single
# engine_proxy line per request is the only per-request stdout record.
# tower_http=off silences the TraceLayer noise; the more specific
# tower_http::catch_panic=error directive wins by longest-prefix and keeps
# panic evidence.
# CAVEAT: docker --env-file/-e beats image ENV at run time — never add
# RUST_LOG to /run/engine-proxy.env.
ENV PORT=8080 \
    JSON_LOGS=1 \
    RUST_LOG=info,cja::server::trace=warn,tower_http=off,tower_http::catch_panic=error \
    CJA_SHUTDOWN_JOB_DRAIN_SECS=1 \
    CJA_SHUTDOWN_EXIT_GRACE_SECS=3

# distroless PATH includes /usr/local/bin, so the infrastructure bootstrap
# line `docker run ... --entrypoint engine <image> proxy_server` runs
# unchanged (Go's image resolves --entrypoint engine via /bin/engine).
USER nonroot:nonroot
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/engine"]
CMD ["proxy_server"]
