# syntax=docker/dockerfile:1.7
FROM rust:1-slim AS build
WORKDIR /src
# The vendored SQLite build (rusqlite "bundled") needs a C compiler.
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
# The KV export the CLI parity suite imports is a repository fixture, so it
# has to be in the image for `cargo test --workspace` to find it.
COPY kv-export.ndjson ./
COPY crates ./crates
RUN cargo test --workspace
RUN cargo build --release --workspace --bins

FROM debian:stable-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata libssl3 && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 shorten && useradd --system --uid 10001 --gid shorten --home /app shorten \
    && mkdir -p /data && chown shorten:shorten /data
WORKDIR /app
COPY --from=build /src/target/release/shorten /usr/local/bin/shorten
COPY --from=build /src/target/release/healthcheck /usr/local/bin/healthcheck
COPY --from=build /src/target/release/backup /usr/local/bin/backup
COPY --from=build /src/target/release/kvimport /usr/local/bin/kvimport
COPY --from=build /src/target/release/seed /usr/local/bin/seed
COPY --from=build /src/target/release/bench /usr/local/bin/bench
COPY --from=build /src/target/release/querybench /usr/local/bin/querybench
USER 10001:10001
ENV SHORTEN_ADDRESS=:8080 SHORTEN_DATABASE_PATH=/data/shorten.db SHORTEN_PUBLIC_BASE_URL=http://localhost:8080
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 CMD ["healthcheck"]
ENTRYPOINT ["shorten"]
