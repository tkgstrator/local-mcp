# local-mcp

A filesystem MCP server that speaks **Streamable HTTP**, so it can be added
directly to ChatGPT as a custom connector. Run it as a sidecar next to the
container you want ChatGPT to work in.

Built on [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk), the official
Rust MCP SDK, which negotiates every protocol revision from `2024-11-05` through
`2026-07-28` — whichever one the client offers.

> Not a fork of [`nakasyou/local-mcp`](https://github.com/nakasyou/local-mcp).
> That one speaks stdio and asks a human to approve each unsandboxed command
> through a terminal UI. This one speaks HTTP and has no approval UI, because a
> sidecar has nobody sitting in front of it. The trade-off that replaces it is
> described below.

## Tools

| Tool | What it does |
| --- | --- |
| `read_file` | Read a text file, returned with line numbers. Supports `offset` / `limit`. |
| `write_file` | Create a file or replace its contents. Creates parent directories. |
| `edit_file` | Replace one exact occurrence. Fails if absent or ambiguous. |
| `list_dir` | List entries with sizes, to a given depth. |
| `search` | Regex search over file contents, honouring `.gitignore`. |
| `execute` | Run a shell command. Returns a `job_id` if it outlives the timeout. |
| `start_command` | Start a command in the background immediately. |
| `poll_job` | Status and output so far. |
| `stop_job` | Kill a job and everything it spawned. |

The last four disappear entirely when `LOCAL_MCP_ALLOW_EXEC=false`.

## Security model

Read this part. The design only makes sense if you know what it does and does
not protect.

**File tools cannot leave the root.** Every path goes through `src/root.rs`,
which resolves `..` textually and then canonicalises the deepest existing
ancestor before checking containment. That blocks `..`, absolute paths,
symlinks pointing outside, and dangling symlinks that would otherwise be treated
as new files to create. This is covered by tests; run them.

**Shell commands can.** `execute` runs `sh -c` with the service user's full
permissions. It starts in the root, but nothing stops it from reading
`/etc/hostname` or reaching the network. This is not a bug that can be fixed
without a real sandbox — it is the cost of having a shell at all.

So the container is the security boundary, not the root path. Give the container
only what you are willing to lose, and:

- **Never expose this without authentication.** It refuses to start without
  `LOCAL_MCP_TOKEN`, and the bundled `compose.yaml` uses `expose` rather than
  `ports` so the only way in is the tunnel.
- **Put a real identity layer in front of it.** A bearer token is one shared
  secret; Cloudflare Access (or equivalent) is what actually decides who gets to
  reach the container.
- **Set `LOCAL_MCP_ALLOW_EXEC=false`** if you only need reads and writes. It
  removes the shell tools from the tool list entirely.
- `Origin` checking is available via `LOCAL_MCP_ALLOWED_ORIGINS` but off by
  default. It guards against a page in someone's browser driving the server,
  which the bearer token already prevents — a browser does not attach an
  `Authorization` header on its own.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `LOCAL_MCP_ROOT` | `/workspace` | Directory the file tools are confined to. |
| `LOCAL_MCP_TOKEN` | *(required)* | Bearer token. At least 16 characters. The server refuses to start without it. |
| `LOCAL_MCP_BIND` | `0.0.0.0:8080` | Listen address. |
| `LOCAL_MCP_ALLOW_EXEC` | `true` | `false` removes all shell tools. |
| `LOCAL_MCP_ALLOWED_ORIGINS` | *(empty)* | Comma-separated origins. Empty disables the check; set it to restrict which browser origins may reach the server. |
| `LOCAL_MCP_ALLOWED_HOSTS` | *(empty)* | Comma-separated hostnames accepted in the `Host` header. Empty disables the check. Set it to your own hostname to reject requests arriving under any other name. |
| `LOCAL_MCP_MAX_OUTPUT` | `1048576` | Byte ceiling on tool output. |
| `LOCAL_MCP_COMMAND_TIMEOUT` | `30` | Seconds before `execute` hands back a `job_id`. |
| `LOCAL_MCP_LOG` | `local_mcp=info,tower_http=info` | Log filter. `debug` for request bodies and transport detail; `warn` to keep only refusals. `RUST_LOG` is honoured too. |

Every request is logged with its method, path and status, and refusals say
whether the token was missing or wrong. A client that cannot connect is visible
here — silence means the request never arrived.

Endpoints: `POST /mcp` (authenticated) and `GET /healthz` (not).

## Running

`compose.yaml` shows the intended shape: your application container and this one
sharing a volume, with only a tunnel reaching in.

```sh
export LOCAL_MCP_TOKEN=$(openssl rand -hex 32)
export TUNNEL_TOKEN=...                        # Cloudflare Zero Trust dashboard
export LOCAL_MCP_USER="$(docker compose exec -T app id -u):$(docker compose exec -T app id -g)"
docker compose up -d --build
```

### Using the published image

Pushes to `master` publish to GHCR, so you do not have to build anything:

```
ghcr.io/tkgstrator/local-mcp:latest
```

Replace `build: .` with `image: ghcr.io/tkgstrator/local-mcp:latest` in
`compose.yaml` to use it. `latest` follows `master`, `sha-<commit>` pins one
exact build, and `vX.Y.Z` / `vX.Y` appear when a git tag is pushed.

### Match the uid, do not chown the volume

The image runs as `1000:1000`, which is what a Dev Container checkout and most
single-user setups are owned by, so usually there is nothing to configure.

When the neighbouring container runs as something else, override it rather than
touching the volume:

```yaml
local-mcp:
  user: "1001:1001"   # whatever `id -u` / `id -g` print in the app container
```

Give both halves. `user: "1001"` alone leaves the group as root, and everything
written through the connector comes out group-owned by root.

Do not fix the mismatch by chowning the shared volume. Those files belong to the
other container, and changing their owner is how you break it. Reads work
regardless; a uid mismatch shows up as every write failing with
`Permission denied`.

Or standalone, for a quick look:

```sh
LOCAL_MCP_TOKEN=$(openssl rand -hex 32) LOCAL_MCP_ROOT=$PWD cargo run --release
```

### Several repositories at once

Give each repository its own sidecar on its own host port, and run a single
`cloudflared` beside them all. A wildcard DNS record is created once; adding a
repository after that is two lines of ingress, not a new domain.

In each repository's `compose.yaml`:

```yaml
services:
  local-mcp:
    image: ghcr.io/tkgstrator/local-mcp:latest
    restart: unless-stopped
    # Runs as 1000:1000 by default; add `user:` only if the checkout is owned
    # by something else.
    environment:
      # Mount point below, not the /workspace default.
      LOCAL_MCP_ROOT: /home/vscode/app
      LOCAL_MCP_TOKEN: ${LOCAL_MCP_TOKEN:?openssl rand -hex 32}
    volumes:
      - ../../:/home/vscode/app:cached
    ports:
      # Bound to loopback: the tunnel is the only way in. Pick a distinct
      # host port per repository.
      - "127.0.0.1:8081:8080"
```

Then one tunnel for the whole machine, outside any project:

```yaml
name: mcp-tunnel
services:
  cloudflared:
    image: cloudflare/cloudflared:latest
    restart: unless-stopped
    # Host networking so the loopback ports above are reachable.
    network_mode: host
    command: tunnel --no-autoupdate --config /etc/cloudflared/config.yml run
    volumes:
      - ./config.yml:/etc/cloudflared/config.yml:ro
      - ./credentials.json:/etc/cloudflared/credentials.json:ro
```

```yaml
# config.yml
tunnel: <tunnel-uuid>
credentials-file: /etc/cloudflared/credentials.json

ingress:
  - hostname: repo-a.mcp.example.com
    service: http://localhost:8081
  - hostname: repo-b.mcp.example.com
    service: http://localhost:8082
  - service: http_status:404
```

Point `*.mcp.example.com` at `<tunnel-uuid>.cfargotunnel.com` once, and each new
repository needs only a port, an ingress entry, and `cloudflared` reloading.

Use a different `LOCAL_MCP_TOKEN` per repository. One leaked token then costs
one checkout rather than all of them.

## Connecting ChatGPT

1. Publish the container over a tunnel so it has an HTTPS URL. Do not open a
   port to the internet directly.
2. In ChatGPT, add a custom connector pointing at `https://<your-host>/mcp`.
3. Supply the bearer token as the connector's authorization header.

Verify by hand first:

```sh
curl -sS https://<your-host>/mcp \
  -H "Authorization: Bearer $LOCAL_MCP_TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
```

A successful response is an SSE stream whose `data:` line carries the server's
`protocolVersion` and instructions.

## Development

A Dev Container is included. Opening the repository in it brings the Rust
toolchain, `act`, and the GitHub CLI with it, and keeps `target/` and the cargo
registry in named volumes so rebuilds survive container restarts.

```sh
cargo test    # containment tests live in src/root.rs
cargo clippy --all-targets -- -D warnings
```

Commit messages follow Conventional Commits (`.commitlintrc.yaml`); CI checks
them, so `wip` will not pass.

## License

MIT
