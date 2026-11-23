"""Run an `openenv harbor <command>` CLI invocation after registering the querymt seam."""

from __future__ import annotations

import sys

from . import seam


def delegate(command: str, argv: list[str] | None = None) -> None:
    seam.register()
    from openenv.cli.__main__ import main

    args = sys.argv[1:] if argv is None else argv
    sys.argv = ["openenv", "harbor", command, *args]
    main()
