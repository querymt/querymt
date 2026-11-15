#!/usr/bin/env python3
"""Split one Codecov JSON report into per-package reports.

CI instruments the whole agent stack in a single build, so `cargo llvm-cov report`
produces one report covering every crate. Codecov flags are per package (see
codecov.yml), and only the packages that were affected by a change are uploaded;
the rest carry forward. This script writes one file per requested package that
contains only the files under that package's directory.

Usage: split-codecov.py REPORT OUT_DIR NAME=DIR_PREFIX [NAME=DIR_PREFIX ...]

Example: split-codecov.py coverage/codecov.json coverage agent-core=crates/agent-core/
writes coverage/codecov-agent-core.json.
"""

import json
import sys
from pathlib import Path


def main(argv: list[str]) -> int:
    if len(argv) < 4:
        print(__doc__, file=sys.stderr)
        return 2

    report = json.loads(Path(argv[1]).read_text())
    out_dir = Path(argv[2])
    out_dir.mkdir(parents=True, exist_ok=True)
    files = report.get("coverage", {})

    for spec in argv[3:]:
        name, _, prefix = spec.partition("=")
        if not name or not prefix:
            print(f"bad spec {spec!r}, expected NAME=DIR_PREFIX", file=sys.stderr)
            return 2
        # The prefix ends in "/" so that crates/agent/ does not match crates/agent-core/.
        selected = {path: lines for path, lines in files.items() if prefix in path}
        target = out_dir / f"codecov-{name}.json"
        target.write_text(json.dumps({**report, "coverage": selected}))
        print(f"{target}: {len(selected)} files")

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
