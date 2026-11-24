# querymt

Python bindings for QueryMT.

## Install

From the repository root:

```bash
python -m venv .venv
source .venv/bin/activate
pip install maturin
maturin develop --manifest-path crates/py/querymt-py/Cargo.toml
```

Then verify the module imports:

```bash
python -c "import querymt; print(querymt.__all__)"
```

## Quick Start

```python
import asyncio
import querymt


async def main() -> None:
    registry = await querymt.Registry.default()
    provider = await registry.provider("openai", model="gpt-4o-mini")
    response = await provider.chat([
        {"role": "user", "content": "Say hello briefly."}
    ])
    print(response.text)


asyncio.run(main())
```

## Examples

- `crates/py/querymt-py/examples/chat.py`
- `crates/py/querymt-py/examples/stream_chat.py`
- `crates/py/querymt-py/examples/tools_chat.py`
- `crates/py/querymt-py/examples/tools_stream_chat.py`
- `crates/py/querymt-py/examples/share_provider.py`
- `crates/py/querymt-py/examples/remote_chat.py`

## LAN Provider Sharing

`MeshRuntime.lan()` without a peer access store lets any connected peer use
models listed in `share_provider(..., allowed_models=[...])`. It does not apply
directional peer grants or per-peer quotas. Use this mode only on a mesh whose
connected peers you trust to consume those models.

To apply the same provider grants and quotas as an agent host, pass the path to
a host-managed mesh state file:

```python
runtime = await querymt.MeshRuntime.lan(peer_access_store="/path/to/mesh_state.json")
```

This option does not change the node's identity. Grants in the file must be for
this host's `runtime.peer_id` and the authenticated calling peer. A new store
starts with no grants; a store that cannot be opened fails startup. Peer grants
can narrow but never expand `allowed_models`. To use a Custom grant, a Python
client must also start with its own store so it can negotiate the grant-aware
protocol. A client without a store uses the legacy protocol and needs a Full
grant on a guarded host.

On a trusted network, `lan_access="full"` gives peers connected from a
private-network address (RFC 1918, loopback, link-local or IPv6 unique-local)
Full access unless the store holds a grant for them:

```python
runtime = await querymt.MeshRuntime.lan(
    peer_access_store="/path/to/mesh_state.json", lan_access="full"
)
```

The default is `"none"`. A saved grant, including No access, always wins.

Use the Rust `GrantStore` and `QuotaStore` APIs to provision the host file while
it is stopped. Do not run another process that writes the same store or uses
the same mesh identity while this runtime is active. This API hosts providers,
not agent sessions.

## Tool Calling

You can pass tool definitions as plain Python dictionaries matching QueryMT's `Tool` schema:

```python
TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "lookup_weather",
            "description": "Look up the current weather for a city.",
            "parameters": {
                "type": "object",
                "properties": {
                    "city": {
                        "type": "string",
                        "description": "City name"
                    }
                },
                "required": ["city"],
            },
        },
    }
]
```

Use them with `chat_with_tools(...)` or `chat_stream_with_tools(...)` (thin Python wrappers over a Rust `ChatCall`).

## Helper Builders

The module exposes canonical helper builders:

- `querymt.user_message(input_parts)` creates `{role: "user", input: [...]}`.
- `querymt.assistant_message(output)` creates `{role: "assistant", output: {...}}`.
- `querymt.text_part(...)`
- `querymt.inline_attachment(...)`
- `querymt.url_attachment(...)`
- `querymt.tool_result(...)`
- `querymt.function_tool(...)`

Generated reasoning and function calls are represented only in canonical assistant
`output`; there are no generated-content input builders. Attachments use one
canonical shape instead of separate image, image-URL, PDF, audio, and
resource-link block variants.
