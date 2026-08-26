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
# 1000:1000 because that is what the checkout is owned by in a Dev Container
# and in most single-user setups, so the common case needs no `user:` override.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --gid 1000 mcp \
 && useradd --create-home --uid 1000 --gid 1000 mcp \
 && mkdir -p /workspace \
 && chown mcp:mcp /workspace
COPY --from=build /src/target/release/local-mcp /usr/local/bin/local-mcp
USER mcp
WORKDIR /workspace
EXPOSE 8080
ENTRYPOINT ["local-mcp"]
