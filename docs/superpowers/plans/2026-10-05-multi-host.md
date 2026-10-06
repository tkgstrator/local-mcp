# Multi-host LocalMCP Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans inline. Track verification per task.

**Goal:** Route a stable MCP tool set to explicitly selected filesystem hosts.
**Architecture:** Preserve standalone operation; central mode intercepts dispatch and forwards through persistent Rust MCP clients. Child mode retains existing filesystem and job handlers.
**Tech Stack:** Rust, rmcp, reqwest, axum, Docker Compose.
**Spec:** ../specs/2026-10-05-multi-host-design.md

## Global Constraints
- Explicit hostname/IP aliases, never arbitrary caller URLs or write fallback.
- No tokens in tool results, config/token files outside root.
- Keep existing standalone calls and image compatible.
- No live GPU deployment or registry publishing.

## Review Focus
- Omitted/unknown selectors must never access local files.
- Duplicate aliases and symlinked secret paths must fail startup.
- One failed child must not hide healthy children in discovery.
- Background job IDs must be used on their original connection.
- Redirects/transport failures must not retry submitted commands or leak tokens.

### Task 1: Configuration and dispatch contract
Files: src/config.rs, src/server.rs, src/connections.rs, src/main.rs, Cargo.toml/Cargo.lock.
- [x] Add failing tests for `connections`, selector schema, unknown selectors and no local fallback.
- [x] Add Mode and validated static connection registry; retain default standalone behavior.
- [x] Add optional selector to fixed tool schemas, central routing and child connection metadata.
- [x] Run targeted tests.

### Task 2: Persistent upstream transport and real routing
Files: src/connections.rs, tests/multi_host.py.
- [x] Add two real child servers with separate roots and central server.
- [x] Assert hostname/IP aliases route reads, writes and edits to the correct root; missing/unknown selector never writes.
- [x] Assert jobs/exec disable, unreachable child and secret-free discovery.
- [x] Implement bounded handshakes and SDK forwarding, with no tool retries/fallback.
- [x] Run full cargo test plus integration script.

### Task 3: Docker roles and operating instructions
Files: Dockerfile.server, Dockerfile.client, compose.server.yaml, compose.client.yaml, README.md, .github/workflows/*.yaml.
- [x] Add non-root lightweight central image and retain existing full child image.
- [x] Add Compose examples with explicit configurable port binds and persistent state.
- [x] Wire image CI for both roles while preserving standalone tags.
- [x] Document connection configuration, file roots, same-connection jobs, LocalGPT integration boundary.
- [x] Run cargo fmt, clippy, integration and build checks; record actual results.

## Verification record

- Initial schema/discovery test failed before implementation; green afterwards.
- Identity-mismatch routing regression failed before the hostname check; green afterwards.
- Rust suite: 43 passed, 0 failed. Formatter, clippy with -D warnings and git diff --check pass.
- Two real child processes: discovery, alias routing, reads/writes/edits/search/list, concurrent calls, jobs, dead host, explicit selector/identity rejection, central exec-disable and standalone compatibility pass.
- HTTP failure fixtures: dropped response after write, session-expired 404 and redirects produce one submission, no initialization replay, no redirected requests, no local fallback.
- Both role images built locally for arm64 as localmcp-server:0.6.0 and localmcp-client:0.6.0. Compose config validates. Docker runtime smoke passed: both images run as UID 1000; a central container discovers two child containers and reads/writes isolated roots. All test containers/network/config volume removed.
- Read-only reviewer found no concrete bug; recommended fault-injection tests were added and passed.
- Production deployment and registry publication are separate steps. LocalGPT gateway allowlist/session target propagation remain follow-up integration.
