FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY migrations/ migrations/
COPY proto/ proto/

# Build release binary
RUN cargo build --release --bin roz-server

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/roz-server /usr/local/bin/roz-server
COPY --from=builder /app/migrations /app/migrations

ENV RUST_LOG=info
EXPOSE 8080 50051

CMD ["roz-server"]
