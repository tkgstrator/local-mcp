# cargo-chef compiles the dependency graph in its own layer, so editing a
# source file does not rebuild every crate underneath it.
#
# The tag is unpinned on purpose: rust-toolchain.toml pins the toolchain, and
# rustup inside the image honours it, so pinning here as well would only mean
# downloading a second toolchain on top of the baked-in one.
FROM rust:slim-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 mcp \
 && mkdir -p /workspace \
 && chown mcp:mcp /workspace
COPY --from=build /src/target/release/local-mcp /usr/local/bin/local-mcp
# Only a default: run this container as the uid that owns the shared volume.
USER mcp
WORKDIR /workspace
EXPOSE 8080
ENTRYPOINT ["local-mcp"]
