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

## Not covered

No real OAuth-protected HTTP server was exercised; OAuth is covered by mock
authorization/MCP servers in the unit and integration tests.
