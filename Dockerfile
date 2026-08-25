# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------
# libgit2, libssh2 and OpenSSL are vendored and compiled from source, so the
# build needs a C toolchain, cmake and perl. It also takes a few minutes the
# first time; the dependency layer below is cached so code changes rebuild fast.
FROM rust:1-bookworm AS builder

RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential cmake perl \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Dependencies first, in their own layer. `migrations/` has to come along
# because `sqlx::migrate!` reads it at compile time.
COPY Cargo.toml Cargo.lock ./
COPY migrations ./migrations
RUN mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src target/release/dondude target/release/deps/mikrotik_dondude*

COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

# `ca-certificates` is what lets the vendored OpenSSL verify github.com.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# The vendored OpenSSL has no compiled-in certificate path that matches Debian's,
# so point it at the system bundle explicitly. Without this, pushing to GitHub
# fails with a certificate error.
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    SSL_CERT_DIR=/etc/ssl/certs \
    DONDUDE_REPO_PATH=/data/backups \
    DONDUDE_BIND=0.0.0.0:8080 \
    # HOME lives on the volume so `known_hosts` survives a restart — otherwise
    # host-key pinning would be relearned every time the container comes up.
    HOME=/data

RUN useradd --system --uid 10001 --home-dir /data --create-home dondude \
 && mkdir -p /data/backups /data/.ssh \
 && chown -R dondude:dondude /data \
 && chmod 700 /data/.ssh

COPY --from=builder /src/target/release/dondude /usr/local/bin/dondude

USER dondude
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8080

ENTRYPOINT ["dondude"]
CMD ["serve"]
