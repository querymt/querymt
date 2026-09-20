import asyncio

import querymt


async def main() -> None:
    runtime = await querymt.MeshRuntime.lan()
    provider = await runtime.find_provider(
        provider="openai",
        model="gpt-4o-mini",
    )
    response = await provider.chat([
        querymt.user_message([querymt.text_part("Say hello from the mesh.")])
    ])
    print(response.text)


if __name__ == "__main__":
    asyncio.run(main())
