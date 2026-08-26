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
- `Origin` is validated on every request, as the MCP spec requires, to stop a
  page in someone's browser from driving the server.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `LOCAL_MCP_ROOT` | `/workspace` | Directory the file tools are confined to. |
| `LOCAL_MCP_TOKEN` | *(required)* | Bearer token. At least 16 characters. The server refuses to start without it. |
| `LOCAL_MCP_BIND` | `0.0.0.0:8080` | Listen address. |
| `LOCAL_MCP_ALLOW_EXEC` | `true` | `false` removes all shell tools. |
| `LOCAL_MCP_ALLOWED_ORIGINS` | *(empty)* | Comma-separated origins. Empty means any request carrying `Origin` is refused. |
| `LOCAL_MCP_MAX_OUTPUT` | `1048576` | Byte ceiling on tool output. |
| `LOCAL_MCP_COMMAND_TIMEOUT` | `30` | Seconds before `execute` hands back a `job_id`. |

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

### Match the uid, do not chown the volume

The files in that shared volume belong to whatever user your application runs
as. This image defaults to `10001`, which almost certainly is not it — so run
this container as the application's user instead:

```yaml
local-mcp:
  user: "1000:1000"   # whatever `id -u` / `id -g` print in the app container
```

Give both halves. `user: "1000"` alone leaves the group as root, and everything
written through the connector comes out group-owned by root.

Do not fix the mismatch by chowning the shared volume. Those files belong to the
other container, and changing their owner is how you break it. Reads work
regardless; a uid mismatch shows up as every write failing with
`Permission denied`.

Or standalone, for a quick look:

```sh
LOCAL_MCP_TOKEN=$(openssl rand -hex 32) LOCAL_MCP_ROOT=$PWD cargo run --release
```

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

```sh
cargo test    # containment tests live in src/root.rs
cargo build
```

## License

MIT
