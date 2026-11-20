"""Load `configs/reference.toml`, the single source of experiment defaults."""

from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .plugin import PluginPin, sha256_hex
from .release import HeadlessPin

DEFAULT_CONFIG = Path(__file__).resolve().parents[2] / "configs" / "reference.toml"


@dataclass(frozen=True)
class Reference:
    """The parts of the reference config the sandbox install needs, resolved to paths."""

    profile: Path
    prompts: tuple[Path, ...]
    plugin: PluginPin
    headless: HeadlessPin
    raw: dict[str, Any] = field(default_factory=dict, compare=False)


def harness_digests(reference: Reference) -> dict[str, Any]:
    """SHA-256 of the profile and each prompt file: the harness an adapter is trained for.

    Recorded in `training_config.json` and compared again before publishing, so an adapter
    is never shipped with a profile it was not trained under.
    """
    return {
        "profile_sha256": sha256_hex(reference.profile.read_bytes()),
        "prompts_sha256": {p.name: sha256_hex(p.read_bytes()) for p in reference.prompts},
    }


def load_reference(path: str | Path | None = None) -> Reference:
    """Read the config at `path`, `$QMT_HARNESS_CONFIG`, or the repository default."""
    config_path = Path(path or os.environ.get("QMT_HARNESS_CONFIG") or DEFAULT_CONFIG)
    raw = tomllib.loads(config_path.read_text())
    base = config_path.resolve().parent
    return Reference(
        profile=(base / raw["profile"]["path"]).resolve(),
        prompts=tuple((base / p).resolve() for p in raw["profile"]["prompts"]),
        plugin=PluginPin.from_config(raw["plugin"]),
        headless=HeadlessPin.from_config(raw["headless_binary"], base),
        raw=raw,
    )
