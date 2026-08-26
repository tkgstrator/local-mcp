FROM rust:1.98-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 mcp \
 && mkdir -p /workspace \
 && chown mcp:mcp /workspace
COPY --from=build /src/target/release/local-mcp /usr/local/bin/local-mcp
USER mcp
WORKDIR /workspace
EXPOSE 8080
ENTRYPOINT ["local-mcp"]
