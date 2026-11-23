import asyncio

import querymt


async def main() -> None:
    registry = await querymt.Registry.default()
    runtime = await querymt.MeshRuntime.lan(node_name="python-share")
    share = await runtime.share_provider(
        registry=registry,
        provider="openai",
        allowed_models=["gpt-4o-mini"],
    )

    print(f"sharing openai on peer_id={runtime.peer_id}")
    await share.wait()


if __name__ == "__main__":
    asyncio.run(main())
