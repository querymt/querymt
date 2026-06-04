import asyncio

import querymt


async def main() -> None:
    registry = await querymt.Registry.default()
    provider = await registry.provider("openai", model="gpt-4o-mini")

    if not provider.supports_streaming():
        raise RuntimeError("provider does not support streaming")

    stream = await provider.chat_stream(
        [{"role": "user", "content": "Explain streaming in one short paragraph."}]
    )

    async for chunk in stream:
        print(chunk.kind, chunk.data)


if __name__ == "__main__":
    asyncio.run(main())
