import asyncio

import querymt


TOOLS = [
    querymt.function_tool(
        name="lookup_weather",
        description="Look up the current weather for a city.",
        parameters={
            "type": "object",
            "properties": {
                "city": {
                    "type": "string",
                    "description": "City name",
                }
            },
            "required": ["city"],
        },
    )
]


async def main() -> None:
    registry = await querymt.Registry.default()
    provider = await registry.provider("openai", model="gpt-4o-mini")

    if not provider.supports_streaming():
        raise RuntimeError("provider does not support streaming")

    messages = [
        querymt.user_message(
            [querymt.text_block("What is the weather in Paris? Stream the reasoning.")]
        )
    ]

    stream = await provider.chat_stream_with_tools(messages, TOOLS)
    async for chunk in stream:
        print(chunk.kind, chunk.data)


if __name__ == "__main__":
    asyncio.run(main())
