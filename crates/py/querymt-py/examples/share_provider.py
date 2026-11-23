import asyncio
import os

import querymt


async def main() -> None:
    registry = await querymt.Registry.default()
    runtime = await querymt.MeshRuntime.lan(
        node_name="python-share",
        peer_access_store=os.environ.get("QMT_PEER_ACCESS_STORE"),
    )
    share = await runtime.share_provider(
        registry=registry,
        provider="openai",
        allowed_models=["gpt-4o-mini"],
    )

    print(f"sharing openai on peer_id={runtime.peer_id}")
    await share.wait()


if __name__ == "__main__":
    asyncio.run(main())
