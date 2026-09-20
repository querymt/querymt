import asyncio

import querymt


async def main() -> None:
    registry = await querymt.Registry.default()
    provider = await registry.provider("openai", model="gpt-4o-mini")
    response = await provider.chat([
        querymt.user_message([querymt.text_part("Say hello in one short sentence.")])
    ])
    print(response.text)


if __name__ == "__main__":
    asyncio.run(main())
