# qmcp

MCP server that lets an MCP client control a running QueryMT agent.
Built with Rust and [qmacp](../qmacp), connecting to
QueryMT over ACP WebSockets. Supports **stdio** (default) and **Streamable HTTP**.

## Install

Download the `qmcp` archive for your platform from the QueryMT
[releases](https://github.com/querymt/querymt/releases), or install it with Nix or Cargo:

```sh
nix profile install github:querymt/querymt#qmcp
cargo install --locked --git https://github.com/querymt/querymt qmcp
```

From a QueryMT checkout:

```sh
cargo install --locked --path crates/qmcp
```

## Connect


See [How to connect](docs/how-to-connect.md) for local stdio, Bearer over HTTPS,
private tunnels, and OAuth/ChatGPT setup. The guide includes client examples,
authentication requirements, and remote-access precautions.

## Tools

The MCP client discovers the tools and their argument schemas automatically.
Start with `caps`, `profiles`, and `models` to discover available capabilities.

| Tools | Purpose |
| --- | --- |
| `caps`, `profiles`, `models` | Discover protocol features, profiles, and models |
| `account` | Plan, usage limits and credits per provider (`querymt/auth/account`) |
| `new`, `sessions`, `find` | Create, list, and search sessions |
| `status`, `inspect`, `runtime` | Read configuration, history, and live runtime |
| `get_lifecycle`, `set_lifecycle` | Read or explicitly patch resolved/archived state |
| `pin`, `unpin` | Set session pin state |
| `set_mode`, `set_model`, `set_effort` | Configure the session |
| `prompt`, `follow`, `watch` | Submit work, collect events, or poll until idle |
| `steer`, `queue`, `discard_queued` | Control active and queued input |
| `cancel`, `close` | Request turn cancellation or explicitly close a session |
| `exec` | Run a batch sequentially on one ACP connection |

Use `prompt` with `new: true` to start a session, then its returned `sessionId`
as `session_id` to continue. A new session requires an absolute `cwd` on the
QueryMT host; existing sessions keep their stored `cwd`, and `prompt` rejects a
`cwd` when continuing one. `profiles`, `models`, `account`, lifecycle, and pin tools
return provider data under `result`, separate from bridge status and event metadata.

## Important Behavior

- Tools can execute work through QueryMT. The default permission policy is
  `allow-once`, with a fallback to the first offered option (possibly `allowAlways`).
  Use `--permission cancel` to deny all approval requests. Interactive elicitation
  is not supported.
- Timeouts and MCP cancellation do **not** stop remote work. Use `cancel`, then
  verify with `runtime`. Stateless HTTP ignores separate MCP cancellation
  notifications; disconnecting is not a guarantee that the operation stopped.
- Failed calls may include `recovery` details. Check them and current runtime
  before retrying: prior effects are not rolled back, including in `exec` batches.
- Queue/steer delivery skips session history. On `prompt`, `until_idle: false`
  returns after submission when either is selected; idle prompt delivery still waits.
- A receipt confirms acceptance, not task success. Check `observationState`;
  invalid receipts fail and keep submission recovery unconfirmed.
- Events and assistant text are returned with the result, not streamed live.
  Check `droppedEvents` and `textTruncated` for incomplete output.
- `find` scans at most 20 pages. Check `truncated` and `scanLimitReached` before
  treating its results as exhaustive.

## Development

From the QueryMT repository root (inside `nix develop` when Nix is available):

```sh
cargo fmt -p qmcp -- --check
cargo clippy --locked -p qmcp --all-targets -- -D warnings
cargo nextest run --locked -p qmcp
```

Tests use mock ACP servers; no running QueryMT instance or provider is needed.
Licensed under [MIT](LICENSE); qmacp's original MIT attribution is retained.
