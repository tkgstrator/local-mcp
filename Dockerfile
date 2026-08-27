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

# BuildKit pulls each of these for the platform being built, so the binaries
# copied out of them are already the right architecture and the Dockerfile
# needs no arch juggling of its own. Taking them from the upstream images also
# beats curl-into-bash: the publisher decides what a working install looks like.
#
# These are pinned because nothing else in the repository pins them: a moving
# tag would let a rebuild of an old commit produce a different toolchain. rust
# is the exception and stays on the tag the build stages use, for the reason
# given at the top of the file.
FROM ghcr.io/astral-sh/uv:0.12.6 AS uv
FROM oven/bun:1.4.0-slim AS bun
FROM node:24.19.0-bookworm-slim AS node
FROM rust:slim-bookworm AS rust

# The Dev Container base rather than debian:slim, because LOCAL_MCP_ALLOW_EXEC
# makes this somewhere people work rather than somewhere a binary merely runs.
# git, curl, jq, less, unzip, xz, openssh-client, procps, ca-certificates, a
# working zsh and a 1000:1000 vscode user all arrive with it.
#
# 3.0.6 rather than dev-ubuntu24.04: same contents, but a tag that cannot move
# under a rebuild.
FROM mcr.microsoft.com/devcontainers/base:3.0.6-ubuntu24.04 AS runtime

# The lists and the package archive live in cache mounts rather than in the
# layer, so a rebuild re-uses them and neither ends up in the image.
#
# Neither gh nor docker has a package in Ubuntu, so both upstream repositories
# go in before the lists are refreshed a second time. Only the docker client
# is installed: the daemon is expected to be somebody else's, reached through
# a mounted /var/run/docker.sock.
#
# Everything else here is what the base does not already carry, and
# build-essential is what pulls in gcc, g++ and make.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    install -m 0755 -d /etc/apt/keyrings \
 && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
      -o /etc/apt/keyrings/githubcli.gpg \
 && curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
      -o /etc/apt/keyrings/docker.asc \
 && chmod a+r /etc/apt/keyrings/githubcli.gpg /etc/apt/keyrings/docker.asc \
 && ARCH="$(dpkg --print-architecture)" \
 && CODENAME="$(. /etc/os-release && echo "$VERSION_CODENAME")" \
 && echo "deb [arch=$ARCH signed-by=/etc/apt/keyrings/githubcli.gpg]" \
      "https://cli.github.com/packages stable main" \
      > /etc/apt/sources.list.d/github-cli.list \
 && echo "deb [arch=$ARCH signed-by=/etc/apt/keyrings/docker.asc]" \
      "https://download.docker.com/linux/ubuntu $CODENAME stable" \
      > /etc/apt/sources.list.d/docker.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential \
      docker-buildx-plugin \
      docker-ce-cli \
      docker-compose-plugin \
      fd-find \
      gh \
      ripgrep \
      shellcheck \
 && groupadd --system docker \
 && usermod --append --groups docker vscode

# uv ships as two static binaries, so it needs nothing else alongside it.
COPY --from=uv /uv /uvx /usr/local/bin/
COPY --from=bun /usr/local/bin/bun /usr/local/bin/
# node keeps npm as a package under lib rather than as a real binary, and the
# wrappers in bin are relative symlinks into it, so both halves have to travel.
COPY --from=node /usr/local/bin/node /usr/local/bin/
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
# rustup drives itself through these two directories, so they move as a pair.
# They belong to vscode because `cargo install` and `rustup update` write into
# them, and this image only ever has the one user.
COPY --from=rust --chown=vscode:vscode /usr/local/rustup /usr/local/rustup
COPY --from=rust --chown=vscode:vscode /usr/local/cargo /usr/local/cargo

# uv keeps its interpreters and its tools somewhere shared rather than under a
# home directory, so both survive a mounted-over /home/vscode. The bin dirs
# point at /usr/local/bin, which sits ahead of /usr/bin, so `python` is the
# interpreter uv manages and not one apt happened to drag in.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    UV_PYTHON_INSTALL_DIR=/usr/local/share/uv/python \
    UV_PYTHON_BIN_DIR=/usr/local/bin \
    UV_TOOL_DIR=/usr/local/share/uv/tools \
    UV_TOOL_BIN_DIR=/usr/local/bin \
    PATH=/usr/local/cargo/bin:$PATH
# Both of these are Python, so uv installs them against the interpreter above
# rather than dragging a system one in behind them. hf_transfer is the faster
# download path; it only takes effect once HF_HUB_ENABLE_HF_TRANSFER is set.
RUN uv python install 3.13 --default \
 && uv tool install git-filter-repo \
 && uv tool install "huggingface_hub[cli,hf_transfer]" \
 && chown -R vscode:vscode /usr/local/share/uv
# Ubuntu ships fd as fdfind to keep out of another package's way, and npm and
# npx are the symlinks the node image would have given us in bin.
RUN ln -s "$(command -v fdfind)" /usr/local/bin/fd \
 && ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx
# Claude Code comes from its own installer rather than npm, so it is the native
# build that knows how to update itself. The installer puts everything under
# the invoking user's home and refuses to be useful under root, so it runs as
# vscode and lands in /home/vscode/.local.
RUN su - vscode -c 'curl -fsSL https://claude.ai/install.sh | bash'
ENV PATH=/home/vscode/.local/bin:$PATH

COPY --from=build /src/target/release/local-mcp /usr/local/bin/local-mcp
# The checkout is bind-mounted here, so the sandbox root points at it instead
# of at the /workspace the binary would otherwise default to. A compose file
# that sets LOCAL_MCP_ROOT itself still wins over this.
ENV LOCAL_MCP_ROOT=/home/vscode/app
# Issued OAuth tokens live here, deliberately outside LOCAL_MCP_ROOT so the file
# tools cannot read them. Mount a volume over it, or every restart forgets the
# clients that already hold valid tokens.
RUN install -d -o vscode -g vscode /home/vscode/app /var/lib/local-mcp
USER vscode
WORKDIR /home/vscode/app
EXPOSE 8080
ENTRYPOINT ["local-mcp"]
