# Manual smoke test (task 9.3)

Paste into the PR description.

Server: `@modelcontextprotocol/server-everything` 2.2.1 (node v26.13.0), driven
through the real `build_mcp_state` / `execute_tool_call` path by the ignored
test `mcp_tool_tests::smoke_real_mcp_server`:

```
# stdio
cargo nextest run -p querymt-agent --all-features --run-ignored only --no-capture \
  -E 'test(smoke_real_mcp_server)'
# Streamable HTTP
PORT=3001 npx -y @modelcontextprotocol/server-everything streamableHttp &
QMT_SMOKE_MCP=http://127.0.0.1:3001/mcp cargo nextest run ... (same filter)
```

Both transports gave identical results.

## Progress (`trigger-long-running-operation`, duration 2, steps 4)

`ToolCallProgress` events observed, in order, before `ToolCallEnd`:

| progress | total | message |
|---------:|------:|---------|
| 1 | 4 | - |
| 2 | 4 | - |
| 3 | 4 | - |
| 4 | 4 | - |

Tool result: `Long running operation completed. Duration: 2 seconds, Steps: 4.`
(`is_error = false`). No progress event followed the tool call's end.

## `isError` result

Invalid arguments (`duration: "not-a-number"`) sent to the adapter. The server
answered `isError: true`; the adapter returned
`Err(McpToolError::Reported { parts })` with the server's text:

```
Input validation error: Invalid arguments for tool trigger-long-running-operation:
duration: Invalid input: expected number, received string,
steps: Invalid input: expected number, received string
```

(In the agent this becomes a failed tool call with this text verbatim; covered
by `is_error_result_is_a_failed_tool_call_with_server_text`.)

## OAuth against a real server (Linear)

Server: `https://mcp.linear.app/mcp`, configured with
`auth = { type = "oauth", scopes = ["read"] }`, run manually (by hand, after the
review follow-ups in tasks 10.1-10.9 landed). The login only works with the
RFC 9207 `iss` handling (task 10.9): Linear advertises
`authorization_response_iss_parameter_supported`, and the token exchange is
refused without the redirect's `iss`.

Observed: the OAuth login for provider `mcp:linear` completed against the real
Linear authorization server (discovery, dynamic client registration, browser
login), and the agent then connected with the stored credentials and listed
Linear issues through the MCP tools.

Not exercised against a real server (covered by mock authorization/MCP servers
in the unit and integration tests only): the skipped-server notification and
`_meta.querymt.skippedMcpServers` before login, token refresh and rejected
refresh, logout, a changed `redirect_port`, an unreachable authorization
server, and private-address discovery (needs an internal identity provider).
