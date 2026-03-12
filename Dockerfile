# ── Stage 1: Build ──────────────────────────────────────────────
FROM rust:1.85-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/

RUN cargo build --release

# ── Stage 2: Runtime ────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/omnirag /usr/local/bin/omnirag

# Create mount point for documents
RUN mkdir -p /rag

EXPOSE 3000

ENTRYPOINT ["omnirag"]
CMD ["serve"]
