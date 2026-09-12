# local-mcp

A filesystem MCP server that speaks **Streamable HTTP**, so it can be added
directly to ChatGPT as a custom connector. Drop `compose.yaml` into a repository
and `docker compose up` puts that repository behind an HTTPS hostname, with
nothing listening on the host.

**Cloudflare Tunnel is the supported way to publish it.** `cloudflared` is in
the compose file, the hostname settings assume it, and so does the identity
advice below. Anything that can forward HTTPS to a container will work instead —
that part is yours to work out.

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
  `LOCAL_MCP_TOKEN`, and the bundled `compose.yaml` publishes no ports at all,
  so the tunnel is the only way in.
- **Put Cloudflare Access in front of it.** A bearer token is one shared secret;
  Access is what actually decides who gets to reach the container.
- **Set `LOCAL_MCP_ALLOW_EXEC=false`** if you only need reads and writes. It
  removes the shell tools from the tool list entirely.
- **Set `LOCAL_MCP_ALLOWED_HOSTS`** to the hostname you publish under. Requests
  arriving under any other name are then refused before they reach a tool.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `LOCAL_MCP_ROOT` | `/workspace` | Directory the file tools are confined to. The published image overrides this to `/home/vscode/app`, where `compose.yaml` mounts the checkout. |
| `LOCAL_MCP_TOKEN` | *(required)* | Bearer token. At least 16 characters. The server refuses to start without it. |
| `LOCAL_MCP_BIND` | `0.0.0.0:8080` | Listen address. |
| `LOCAL_MCP_ALLOW_EXEC` | `true` | `false` removes all shell tools. |
| `LOCAL_MCP_ALLOWED_HOSTS` | *(empty)* | Comma-separated hostnames accepted in the `Host` header. Empty disables the check. Set it to your own hostname to reject requests arriving under any other name. |
| `LOCAL_MCP_PUBLIC_URL` | *(empty)* | Public origin, e.g. `https://mcp.example.com`. Setting it enables the OAuth flow below; without it only the static token is accepted. |
| `LOCAL_MCP_STATE_DB` | `/var/lib/local-mcp/oauth.db` | SQLite file holding issued tokens and registered OAuth clients. Only read when the OAuth flow is enabled. Must sit outside `LOCAL_MCP_ROOT`, or the server refuses to start: the file tools can read anything under the root, and this file is a set of live credentials. Put a volume on its directory. |
| `LOCAL_MCP_MAX_OUTPUT` | `1048576` | Byte ceiling on tool output. |
| `LOCAL_MCP_COMMAND_TIMEOUT` | `30` | Seconds before `execute` hands back a `job_id`. |
| `LOCAL_MCP_LOG` | `local_mcp=info,tower_http=info` | Log filter. `debug` for request bodies and transport detail; `warn` to keep only refusals. `RUST_LOG` is honoured too. |

Every request is logged with its method, path and status, and refusals say
whether the token was missing or wrong. A client that cannot connect is visible
here — silence means the request never arrived.

Endpoints: `POST /local` (authenticated) and `GET /healthz` (not).

## OAuth

Some clients will not accept a static token. ChatGPT is one: its connector form
offers OAuth and nothing else. Setting `LOCAL_MCP_PUBLIC_URL` makes this server
its own authorization server so those clients can connect.

```
/.well-known/oauth-protected-resource   names the authorization server
/.well-known/oauth-authorization-server lists the endpoints
/register                               dynamic client registration (RFC 7591)
/authorize                              consent screen
/token                                  authorization code -> access token
```

The consent screen asks for `LOCAL_MCP_TOKEN`. **Nothing here federates
identity** — the shared secret still decides who gets in, and the flow only
packages it in the shape those clients require. Enter it once in the browser and
the client holds an access token from then on. PKCE (S256) is required, codes
are single-use and expire in ten minutes, and tokens last 30 days.

Registered clients and issued tokens are kept in the SQLite file named by
`LOCAL_MCP_STATE_DB`, so a restart does not invalidate them. Mount a volume on
its directory — on ephemeral storage the file goes away with the container, and
a client holding a 30-day token then gets 401s that look like a rejected
credential rather than a forgotten one.

The static token keeps working, so a client that can hold a secret skips the
round trip entirely:

```sh
claude mcp add --transport http local-mcp https://mcp.example.com/local \
  --header "Authorization: Bearer $LOCAL_MCP_TOKEN"
```

To put a real identity provider in front instead, protect `/authorize` with
Cloudflare Access — it is a browser page, so SSO applies — and leave `/local` and
`/token` alone, since those are machine-to-machine.

## Running

Copy `compose.yaml` into the repository you want ChatGPT to work on. It mounts
that repository and runs `cloudflared` beside it, and publishes no ports at all —
the tunnel reaches the server over the compose network, so nothing is listening
on the host.

```yaml
services:
  local-mcp:
    image: ghcr.io/tkgstrator/local-mcp:latest
    restart: unless-stopped
    environment:
      # Anything the commands run through `execute` need goes here too.
      LOCAL_MCP_ALLOWED_HOSTS: mcp.example.com
      LOCAL_MCP_ALLOW_EXEC: true
      LOCAL_MCP_PUBLIC_URL: https://mcp.example.com
      LOCAL_MCP_ROOT: /home/vscode/app
      LOCAL_MCP_TOKEN: ${LOCAL_MCP_TOKEN}
    group_add:
      - "${DOCKER_GID}"
    volumes:
      - ./:/home/vscode/app:cached
      - /var/run/docker.sock:/var/run/docker.sock
      - ~/.config/gh:/home/vscode/.config/gh:cached,readonly
      - ~/.gitconfig:/home/vscode/.gitconfig:cached,readonly
      - ~/.ssh:/home/vscode/.ssh:cached,readonly
      - local-mcp-state:/var/lib/local-mcp

  cloudflared:
    image: cloudflare/cloudflared:latest
    restart: unless-stopped
    command: tunnel --no-autoupdate run
    environment:
      TUNNEL_TOKEN: ${TUNNEL_TOKEN}

volumes:
  local-mcp-state:
```

The `${...}` values come from a `.env` file beside `compose.yaml`, which is
where they belong rather than in your shell — `docker compose` reads it every
time, so bringing the stack back up in six months does not depend on what you
happened to export back then.

```sh
cat > .env <<EOF
LOCAL_MCP_TOKEN=$(openssl rand -hex 32)
DOCKER_GID=$(stat -c '%g' /var/run/docker.sock)
TUNNEL_TOKEN=
EOF
```

Paste the tunnel token into the blank line — the Zero Trust dashboard hands it
over when the tunnel is created. Then:

```sh
docker compose up -d
```

`.env` is gitignored, and it holds two secrets, so keep it that way.

### What each piece is for

**Three settings have to agree on one hostname.** `LOCAL_MCP_ALLOWED_HOSTS`
decides which requests are let in, `LOCAL_MCP_PUBLIC_URL` is what the OAuth
metadata tells clients to come back to, and the tunnel's public hostname is what
actually resolves. Get one wrong and the failure is silent in a different way
each time: a 403 before any tool runs, a client redirected somewhere it cannot
reach, or a 404 from Cloudflare.

**`LOCAL_MCP_ROOT` has to match the mount point.** `./:/home/vscode/app` puts
the checkout at that path inside the container, and the file tools are confined
to whatever `LOCAL_MCP_ROOT` names. The image already sets it to that path, so
the line above is only saying out loud what is already true — but move the mount
and you have to move both, or the server refuses to start against a root that is
not there.

**`environment` is where the tools get theirs too.** Only `LOCAL_MCP_*` is read
by the server, and those are in [Configuration](#configuration). Anything else
you put in that block is passed through for whatever `execute` runs — an API
token a CLI expects, a proxy setting, credentials for a service the repository
talks to. The tools pick them up from the environment like they would anywhere
else. Nothing here needs them, so the list is however long your own tooling
makes it.

The volumes divide the same way:

| Mount | Why |
| --- | --- |
| `./:/home/vscode/app` | The repository itself. This is what the file tools read and write. |
| `local-mcp-state:/var/lib/local-mcp` | Issued OAuth tokens and registered clients. Without it a restart invalidates every token already handed out. |
| `/var/run/docker.sock` | Lets commands run through `execute` drive containers. Needs `group_add` below. |
| `~/.config/gh`, `~/.gitconfig`, `~/.ssh` | Read-only, so `git` and `gh` work inside the container as you. |

The bottom two rows are conveniences, not requirements — the server runs with
only the checkout and the state volume. Weigh them honestly: the Docker socket is
enough to start a container that mounts anything on the host, and `~/.ssh` is
your key. Both are handing a shell your credentials, which is the whole point and
also the whole risk.

**`group_add` and the Docker socket are one decision, not two.** Mounting the
socket is what lets `execute` drive containers, and `DOCKER_GID` is what makes
that mount usable — the socket is owned by a different group on every machine,
which is why the `.env` above reads it with `stat -c '%g'` instead of carrying a
number. Keep both or drop both. Mount it without the group and the socket is
right there while every docker command fails on permissions, which reads like a
broken install rather than a deliberate one.

### Cloudflare Tunnel

Create a tunnel in the Zero Trust dashboard, add a public hostname, and point it
at `http://local-mcp:8080` — the compose network resolves the service name, so
that address is the same in every repository. The dashboard hands back a token;
that is `TUNNEL_TOKEN`.

Nothing is published to the host, so the tunnel is the only way in. That is what
makes the bearer token the whole boundary rather than a second lock behind a
firewall.

### Using the published image

Pushes to `master` publish to GHCR, which is what `compose.yaml` already points
at:

```
ghcr.io/tkgstrator/local-mcp:latest
```

`latest` follows `master`, `sha-<commit>` pins one exact build, and `vX.Y.Z` /
`vX.Y` appear when a git tag is pushed. Swap in `build: .` to run your own
working copy instead.

### What is in the image

Because `execute` makes this somewhere people work rather than somewhere a
binary merely runs, the image is built on the Dev Container base image and comes
with the tools you would expect to find in a terminal:

| | |
| --- | --- |
| **Runtimes** | Rust via `rustup` (honouring `rust-toolchain.toml`), Node 24 with `npm` / `npx`, Bun, and Python 3.13 managed by `uv`. |
| **Search** | `ripgrep`, `fd`, `jq`. |
| **Git** | `git`, `gh`, `git-filter-repo`. |
| **Containers** | `docker` with the `buildx` and `compose` plugins. |
| **Agents** | Claude Code, from its own installer rather than npm, so it is the native build that can update itself. |
| **Building** | `gcc`, `g++` and `make` by way of `build-essential`, plus `shellcheck`. |
| **From the base** | `curl`, `jq`, `less`, `unzip`, `xz`, `openssh-client`, `procps`, `ca-certificates` and a working `zsh`. |

Only the Docker *client* is installed. The daemon is expected to be somebody
else's, reached through the socket `compose.yaml` mounts — which is why that
mount and `group_add` decide whether any of it works.

The tool versions are pinned, so rebuilding an old commit gets the same
toolchain rather than whatever the tags point at today. Rust is the exception,
since `rust-toolchain.toml` already pins it from inside.

This is also what the `environment` block is for. Claude Code reads
`ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_BASE_URL`; the Hugging Face CLI reads
`HF_TOKEN`, and `HF_HUB_ENABLE_HF_TRANSFER` to use the faster download path.
None of that is the server's business, and all of it arrives the same way — put
it in that block and the tools find it. `gh` is the exception, and only because
`compose.yaml` mounts its config directory instead.

### Match the uid, do not chown the checkout

The image runs as `1000:1000`, which is what a Dev Container checkout and most
single-user setups are owned by, so usually there is nothing to configure.

When the checkout belongs to someone else, override it rather than touching the
files:

```yaml
local-mcp:
  user: "1001:1001"   # whatever `id -u` / `id -g` print for the checkout
```

Give both halves. `user: "1001"` alone leaves the group as root, and everything
written through the connector comes out group-owned by root.

Do not fix the mismatch by chowning the checkout. Those files are yours, and
changing their owner is how you break the tools that already use them. Reads
work regardless; a uid mismatch shows up as every write failing with
`Permission denied`.

Or standalone, for a quick look:

```sh
LOCAL_MCP_TOKEN=$(openssl rand -hex 32) LOCAL_MCP_ROOT=$PWD cargo run --release
```

### Alongside a Dev Container

If the same repository is also open in a Dev Container, the two containers see
almost the same tree — and the exception is worth knowing before it confuses
you.

The checkout is genuinely shared: both bind-mount the same directory from the
host, so a source file edited on one side is the same file on the other. What
differs is anything the Dev Container covers with a named volume. Here that is
`target/`, mounted at `/home/vscode/app/target`, which sits *inside* the tree
this server mounts:

```
/home/vscode/app          bind mount from the host   ← both containers
/home/vscode/app/target   named volume               ← the Dev Container only
```

So `cargo build` run through `execute` neither reuses what the Dev Container
compiled nor writes anywhere it can see. Two caches, twice the disk, and a cold
rebuild on each side. Nothing breaks — it is just quietly wasteful, and the kind
of thing you notice as "why is this rebuilding everything again".

The simple answer is to build in the Dev Container and let the connector read,
write and search source. To share the cache instead, mount the same volume in.
`docker volume ls` prints the name; it is the compose project name with
`_devcontainer_target` on the end:

```yaml
services:
  local-mcp:
    image: ghcr.io/tkgstrator/local-mcp:latest
    # ...environment and the rest as in the example above...
    volumes:
      - ./:/home/vscode/app:cached
      - local-mcp-state:/var/lib/local-mcp
      - <project>_devcontainer_target:/home/vscode/app/target

volumes:
  local-mcp-state:
  # external: the volume belongs to the Dev Container's compose project, so this
  # file must not create its own under a different name — only refer to it.
  <project>_devcontainer_target:
    external: true
```

`~/.cargo/registry` goes the same way, though it lands outside the root so it
only ever costs a second download. `~/.claude` is worth a thought too: the Dev
Container bind-mounts it and `compose.yaml` here does not, so Claude Code
started inside this container has none of that history and authenticates from
`ANTHROPIC_AUTH_TOKEN` instead.

The other direction is to stop covering the path in the first place. A volume
over `target/` — or `node_modules/`, or `.venv/` — is usually there because bind
mounts are slow on macOS, where every file crosses a virtualisation boundary. On
a Linux host they are the host's own filesystem and that cost is not being paid,
so the volume buys nothing and hides a directory the connector would otherwise
be able to read. Dropping it from the Dev Container's `compose.yaml` leaves one
tree that both containers, and you, see the same way.

Which matters more than it sounds for anything generated rather than written.
Build output you can rebuild; the contents of `node_modules/`, or whatever
`build.rs` left in `OUT_DIR`, are often exactly what you wanted to go and read.

### Data that lives outside the checkout

Datasets, model weights and anything else too big for the repository tend to sit
on their own disk and get bind-mounted in — `/mnt/data:/mnt/data`, mounted at the
same path inside and out so absolute paths in configs resolve either way. Two
separate things then stop the connector from reading it, and fixing one without
the other looks like the mount is broken.

The first is that `compose.yaml` here mounts the checkout and nothing else, so
the disk is simply not in this container. The second is that even after adding
it, `/mnt/data` is outside `LOCAL_MCP_ROOT` and the file tools refuse every path
that lands outside the root.

So mount it twice — once where the absolute paths expect it, once inside the
root where the tools can reach it:

```yaml
services:
  local-mcp:
    image: ghcr.io/tkgstrator/local-mcp:latest
    # ...environment and the rest as in the example above...
    volumes:
      - ./:/home/vscode/app:cached
      # Same directory, reachable two ways. /mnt/data keeps absolute paths in
      # configs working; data/ puts it under LOCAL_MCP_ROOT, which is the only
      # place the file tools will read. One bind mount is not a copy of the
      # other — they are the same inodes, so a write through either is the
      # same write.
      - /mnt/data:/mnt/data
      - /mnt/data:/home/vscode/app/data:cached
```

Give the Dev Container the same pair and the tree has the same shape in both
containers: `data/train/...` relative to the checkout resolves in either, and so
does `/mnt/data/...` from a config. In `.devcontainer/compose.yaml`:

```yaml
services:
  app:
    build:
      context: .
      dockerfile: Dockerfile
    volumes:
      - ../:/home/vscode/app:cached
      # The same two mounts as on the local-mcp side. Without the second one,
      # data/ exists in one container and not the other, and a relative path
      # that works here quietly fails there.
      - /mnt/data:/mnt/data
      - /mnt/data:/home/vscode/app/data:cached
```

Either way the checkout now contains a directory git knows nothing about, and
`git status` will happily crawl every byte of it. Put `/data/` in `.gitignore`
before the first status that takes a minute makes the point for you.

A symlink will not do instead, and fails in a way worth knowing about. Paths are
resolved by canonicalising the deepest part that exists and requiring the result
to be under the root — which a symlink is not, because it resolves to wherever it
points. `/home/vscode/app/data` as a symlink to `/mnt/data` canonicalises to
`/mnt/data` and is refused. As a bind mount it stays `/home/vscode/app/data`,
because a mount changes what the kernel finds there without changing the path,
and it is allowed. Same apparent shortcut, opposite outcome.

Raising `LOCAL_MCP_ROOT` to cover both is the obvious-looking third option and is
not one: it means handing the connector everything under the new root, and the
server refuses to start outright if the widened root ends up containing
`LOCAL_MCP_STATE_DB`, since the file tools would then be able to read the tokens
it has issued.

### Several repositories at once

The tidiest arrangement is to put `compose.yaml` one level up, in the directory
your checkouts already live in:

```
Developer/
├── .env
├── compose.yaml
├── repository-a
├── repository-b
└── repository-c
```

Nothing in the file changes. `./:/home/vscode/app` now mounts the parent, so all
three appear under the one root, and `cd repository-b` in a shell command is all
it takes to move between them. One tunnel, one hostname, one token, one
container. Adding a fourth repository is `git clone` and nothing else — no
compose edit, no new tunnel, no restart.

Keep the clones one level deep. `LOCAL_MCP_ROOT` is the whole tree, so a
checkout nested three directories down works but stops being obvious, and
anything else you leave in `Developer/` is readable too.

The trade is that one token now reaches every repository under it. Where that
matters — a client's code beside your own, say — give that one its own directory
elsewhere, with its own `compose.yaml`, hostname, tunnel and token. Nothing
binds a host port, so two of these run side by side with no coordination at all;
only the hostname and the token have to differ.

## Connecting ChatGPT

1. Bring it up behind the Cloudflare Tunnel so it has an HTTPS URL. Do not open
   a port to the internet directly.
2. Set `LOCAL_MCP_PUBLIC_URL` to that URL. ChatGPT offers OAuth and nothing else
   whichever way you add the server, so without it there is no way to hand the
   token over.
3. In ChatGPT, add a custom connector pointing at `https://<your-host>/local`.
4. The consent screen asks for `LOCAL_MCP_TOKEN`. Enter it once; the connector
   holds an access token from then on.

### As a plugin

The same server can be added as a plugin instead of as a connector. Nothing
changes on this side — same URL, same OAuth flow, same token — and it is then
available in the browser and in the apps.

Go to [chatgpt.com/plugins](https://chatgpt.com/plugins), pick **New Plugin**,
and fill in two things that matter:

| Field | What to put |
| --- | --- |
| **Connection** | `Server URL`, not `Tunnel`. |
| **MCP Server URL** | The tunnel's hostname with `/local` on the end: `https://<your-host>/local`. |
| **Authentication** | `OAuth`. |

`Server URL` versus `Tunnel` is the one place this gets genuinely confusing,
because you got here by setting up a tunnel. They are unrelated. Cloudflare has
already done the tunnelling and handed you an ordinary HTTPS address; as far as
ChatGPT is concerned this is just a server on the internet, so it connects by
URL like any other.

The URL field is prefilled with a placeholder ending in `/sse`. That is a
different transport; this server answers on `/local` and nothing is listening at
`/sse`.

**Advanced OAuth settings** is worth opening once, because it says it will
review the *discovered* settings — it fetches the metadata from the URL you
typed. Seeing them appear means `LOCAL_MCP_PUBLIC_URL` is set and the tunnel
resolves. Seeing nothing means one of those two is wrong, and finding out here
is cheaper than finding out from a connector that just fails.

`Name` and `Description` are your own labels; the icon is optional, PNG, and
capped at 10 KB.

Then ChatGPT asks you to confirm that you understand the risk — that it has not
reviewed this server, and that a custom MCP server can be used to steal data or
talk the model into destroying some. That warning is accurate. This one hands
out a shell by design, which is exactly why the container is the boundary and
why the token is the only thing standing in front of it. The
[security model](#security-model) is the part that decides whether clicking
through is reasonable.

### Verify by hand first

```sh
curl -sS https://<your-host>/local \
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
