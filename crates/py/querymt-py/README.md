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
