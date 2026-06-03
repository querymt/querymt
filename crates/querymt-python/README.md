# querymt

Python bindings for QueryMT.

## Install

From the repository root:

```bash
python -m venv .venv
source .venv/bin/activate
pip install maturin
maturin develop --manifest-path crates/querymt-python/Cargo.toml
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

- `crates/querymt-python/examples/chat.py`
- `crates/querymt-python/examples/stream_chat.py`
- `crates/querymt-python/examples/tools_chat.py`
- `crates/querymt-python/examples/tools_stream_chat.py`
- `crates/querymt-python/examples/share_provider.py`
- `crates/querymt-python/examples/remote_chat.py`

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

Use them with `chat_with_tools(...)` or `chat_stream_with_tools(...)`.

## Helper Builders

The module also exposes small helper builders:

- `querymt.user_message(...)`
- `querymt.assistant_message(...)`
- `querymt.text_block(...)`
- `querymt.image_block(...)`
- `querymt.image_url_block(...)`
- `querymt.pdf_block(...)`
- `querymt.audio_block(...)`
- `querymt.thinking_block(...)`
- `querymt.tool_use_block(...)`
- `querymt.tool_result_block(...)`
- `querymt.resource_link_block(...)`
- `querymt.function_tool(...)`
