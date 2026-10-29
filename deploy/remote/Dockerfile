FROM rust:1.99-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
RUN cargo build --locked --release --bin obsidian-mcp

FROM debian:bookworm-slim AS standalone
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/target/release/obsidian-mcp /usr/local/bin/obsidian-mcp
ENV OBSIDIAN_TRANSPORT=http OBSIDIAN_HTTP_HOST=0.0.0.0 OBSIDIAN_HTTP_PORT=37842 \
    OBSIDIAN_MCP_DATA=/data/index OBSIDIAN_BACKUP_DIR=/data/backups \
    OBSIDIAN_EMBEDDINGS=false OBSIDIAN_SEMANTIC_MODE=local
USER 1000:1000
EXPOSE 37842
ENTRYPOINT ["obsidian-mcp"]
CMD ["/vault"]

# Optional image for service-owned one-shot sync. Build with --target managed-sync.
FROM node:22-bookworm-slim AS managed-sync
ARG OBSIDIAN_HEADLESS_VERSION=0.0.14
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && npm install --global "obsidian-headless@${OBSIDIAN_HEADLESS_VERSION}"
COPY --from=build /build/target/release/obsidian-mcp /usr/local/bin/obsidian-mcp
ENV OBSIDIAN_TRANSPORT=http OBSIDIAN_HTTP_HOST=0.0.0.0 OBSIDIAN_HTTP_PORT=37842 \
    OBSIDIAN_MCP_DATA=/data/index OBSIDIAN_BACKUP_DIR=/data/backups \
    OBSIDIAN_EMBEDDINGS=false OBSIDIAN_SEMANTIC_MODE=local \
    OBSIDIAN_SYNC_MODE=managed XDG_CONFIG_HOME=/data/config HOME=/data
USER 1000:1000
EXPOSE 37842
ENTRYPOINT ["obsidian-mcp"]
CMD ["/vault"]

# Keep the default build free of Node and automatic sync.
FROM standalone AS final
