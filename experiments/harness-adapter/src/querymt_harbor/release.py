"""The pinned qmt-headless archive, read once on the host and verified."""

from __future__ import annotations

import os
import re
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .plugin import HttpGet, httpx_get, sha256_hex

_SHA256 = re.compile(r"^[0-9a-f]{64}$")


class ReleaseError(RuntimeError):
    """The qmt-headless archive could not be fetched or did not match its pin."""


@dataclass(frozen=True)
class ArchivePin:
    """One archive: a `url` (downloaded on the host) or a local `path`, plus its SHA-256."""

    sha256: str
    url: str | None = None
    path: Path | None = None

    @property
    def source(self) -> str:
        return self.url if self.url is not None else str(self.path)


@dataclass(frozen=True)
class HeadlessPin:
    version: str
    platforms: dict[str, ArchivePin]

    @classmethod
    def from_config(cls, table: dict[str, Any], base: Path | None = None) -> HeadlessPin:
        platforms: dict[str, ArchivePin] = {}
        for name, entry in table.get("platforms", {}).items():
            sha256 = str(entry.get("sha256", "")).removeprefix("sha256:")
            url, path = entry.get("url"), entry.get("path")
            if (url is None) == (path is None):
                raise ReleaseError(f"headless_binary pin for {name} needs exactly one of url, path")
            if not _SHA256.match(sha256):
                raise ReleaseError(f"headless_binary pin for {name} needs a 64-hex sha256")
            if url is not None and not url.startswith("https://"):
                raise ReleaseError(f"headless_binary pin for {name} needs an https url")
            resolved = None
            if path is not None:
                resolved = Path(path)
                if not resolved.is_absolute() and base is not None:
                    resolved = (base / resolved).resolve()
            platforms[name] = ArchivePin(sha256=sha256, url=url, path=resolved)
        return cls(version=table["version"], platforms=platforms)

    def for_platform(self, platform_id: str) -> ArchivePin:
        try:
            return self.platforms[platform_id]
        except KeyError:
            raise ReleaseError(
                f"no pinned qmt-headless archive for {platform_id}; "
                f"pinned: {sorted(self.platforms)}"
            ) from None


def fetch_archive(
    pin: HeadlessPin,
    platform_id: str,
    cache_dir: str | Path,
    http_get: HttpGet = httpx_get,
) -> Path:
    """Return a local path to the verified archive, reading or downloading it only if not cached.

    The cache file is named by SHA-256 and re-hashed on every call.
    """
    asset = pin.for_platform(platform_id)
    cache = Path(cache_dir)
    cache.mkdir(parents=True, exist_ok=True)
    target = cache / f"{asset.sha256}.tar.gz"
    if target.is_file() and sha256_hex(target.read_bytes()) == asset.sha256:
        return target

    if asset.url is not None:
        data = http_get(asset.url, {})
    else:
        assert asset.path is not None
        if not asset.path.is_file():
            raise ReleaseError(f"pinned qmt-headless archive {asset.path} does not exist")
        data = asset.path.read_bytes()
    actual = sha256_hex(data)
    if actual != asset.sha256:
        raise ReleaseError(
            f"qmt-headless archive sha256 mismatch for {platform_id} ({asset.source}): "
            f"expected {asset.sha256}, got {actual}"
        )
    fd, tmp = tempfile.mkstemp(dir=cache, suffix=".part")
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
        os.replace(tmp, target)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)
    return target
