# querymt-agent

Python bindings for QueryMT Agent.

## Install

From the repository root:

```bash
python -m venv .venv
source .venv/bin/activate
pip install maturin
maturin develop --manifest-path crates/py/querymt-py/Cargo.toml
maturin develop --manifest-path crates/py/querymt-agent-py/Cargo.toml
```

## Quick Start

```python
import asyncio
from querymt_agent import Agent


async def main() -> None:
    agent = await Agent.single(
        provider="openai",
        model="gpt-4o-mini",
        tools=["read_tool", "glob", "search_text"],
    )
    print(await agent.chat("Say hello briefly."))


asyncio.run(main())
```
