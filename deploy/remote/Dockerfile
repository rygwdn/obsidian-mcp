FROM rust:1.99-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
RUN cargo build --locked --release --bin obsidian-mcp

FROM debian:bookworm-slim
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
