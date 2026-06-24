#!/usr/bin/env python3
from __future__ import annotations

import argparse
import pathlib
import re
import sys

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - fallback for local Python < 3.11
    import tomli as tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
CARGO_TOML = ROOT / "Cargo.toml"
QUERYMT_PYPROJECT = ROOT / "crates/py/querymt-py/pyproject.toml"
QUERYMT_AGENT_PYPROJECT = ROOT / "crates/py/querymt-agent-py/pyproject.toml"

PIN_PATTERNS = {
    QUERYMT_PYPROJECT: (
        re.compile(r'^agent = \["querymt-agent==(?P<version>[^"]+)"\]$', re.MULTILINE),
        'agent = ["querymt-agent=={version}"]',
    ),
    QUERYMT_AGENT_PYPROJECT: (
        re.compile(r'^dependencies = \["querymt==(?P<version>[^"]+)"\]$', re.MULTILINE),
        'dependencies = ["querymt=={version}"]',
    ),
}


def workspace_version() -> str:
    cargo = tomllib.loads(CARGO_TOML.read_text())
    return cargo["workspace"]["package"]["version"]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Sync Python package dependency pins from the workspace Cargo version."
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="fail if files are out of sync")
    mode.add_argument("--write", action="store_true", help="rewrite files in place")
    parser.add_argument(
        "--tag",
        help="optional release tag like v0.5.0; when set, ensure it matches the workspace version",
    )
    return parser.parse_args()


def validate_dynamic_version(pyproject_path: pathlib.Path, data: dict[str, object]) -> list[str]:
    project = data.get("project", {})
    if not isinstance(project, dict):
        return [f"{pyproject_path}: missing [project] table"]

    dynamic = project.get("dynamic", [])
    if not isinstance(dynamic, list) or "version" not in dynamic:
        return [
            f"{pyproject_path}: expected [project].dynamic to contain 'version', got {dynamic!r}"
        ]
    return []


def current_pin(pyproject_path: pathlib.Path) -> str | None:
    pattern, _ = PIN_PATTERNS[pyproject_path]
    match = pattern.search(pyproject_path.read_text())
    if not match:
        return None
    return match.group("version")


def sync_file(pyproject_path: pathlib.Path, version: str, write: bool) -> tuple[bool, str]:
    pattern, replacement_template = PIN_PATTERNS[pyproject_path]
    content = pyproject_path.read_text()
    match = pattern.search(content)
    if not match:
        return False, f"{pyproject_path}: could not find expected dependency pin to update"

    replacement = replacement_template.format(version=version)
    updated = pattern.sub(replacement, content, count=1)
    changed = updated != content
    if write and changed:
        pyproject_path.write_text(updated)
    return changed, f"{pyproject_path}: {'updated' if changed else 'already in sync'}"


def main() -> int:
    args = parse_args()
    version = workspace_version()
    errors: list[str] = []

    if args.tag is not None:
        if not args.tag.startswith("v"):
            errors.append(f"release tag must start with 'v', got {args.tag}")
        elif args.tag[1:] != version:
            errors.append(f"workspace version {version} does not match tag {args.tag[1:]}")

    for pyproject_path in (QUERYMT_PYPROJECT, QUERYMT_AGENT_PYPROJECT):
        data = tomllib.loads(pyproject_path.read_text())
        errors.extend(validate_dynamic_version(pyproject_path, data))

    desired_pins = {
        QUERYMT_PYPROJECT: f"querymt-agent=={version}",
        QUERYMT_AGENT_PYPROJECT: f"querymt=={version}",
    }

    out_of_sync: list[str] = []
    for pyproject_path, expected in desired_pins.items():
        current = current_pin(pyproject_path)
        if current is None:
            errors.append(f"{pyproject_path}: could not read current dependency pin")
            continue
        actual = expected.split("==", 1)[1]
        if current != actual:
            out_of_sync.append(
                f"{pyproject_path}: dependency pin is {current}, expected {actual}"
            )

    if args.write:
        for pyproject_path in desired_pins:
            _, message = sync_file(pyproject_path, version, write=True)
            print(message)
    else:
        for pyproject_path, expected in desired_pins.items():
            print(f"{pyproject_path}: expected dependency pin {expected}")

    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1

    if args.check and out_of_sync:
        print("\n".join(out_of_sync), file=sys.stderr)
        return 1

    if args.write:
        print(f"Python package dependency pins synced to workspace version {version}")
    else:
        print(f"Workspace version: {version}")
        if out_of_sync:
            print("\n".join(out_of_sync), file=sys.stderr)
            return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
