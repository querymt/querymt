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

    messages = [
        querymt.user_message(
            [querymt.text_block("What is the weather in Paris? Use the tool if needed.")]
        )
    ]

    response = await provider.chat_with_tools(messages, TOOLS)
    print("text:", response.text)
    print("tool_calls:", [call.name for call in response.tool_calls])
    for block in response.content:
        print(block.kind, block.data)


if __name__ == "__main__":
    asyncio.run(main())
