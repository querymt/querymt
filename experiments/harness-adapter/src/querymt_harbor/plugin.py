"""Fetch the pinned provider plugin and render the sandbox's provider config.

`qmt-headless` resolves the `openai` provider from a `providers.toml`. By default that
points at `oci://ghcr.io/querymt/openai:latest`, pulled over the network on first
use: unreliable under restricted sandbox networks and not frozen. Here the wasm
is downloaded once on the host by SHA-256 and uploaded as a local file.
"""

from __future__ import annotations

import hashlib
import json
import os
import tempfile
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlsplit

import httpx

WASM_MEDIA_TYPE = "application/vnd.wasm.v1.layer+wasm"
MANIFEST_ACCEPT = "application/vnd.oci.image.manifest.v1+json"

# An empty models.dev registry. Written fresh at install time it is "fresh" for 24 h,
# so the harness never fetches https://models.dev during a rollout, and no models.dev
# defaults (output caps, pricing) can vary the requests between runs.
EMPTY_MODELS_REGISTRY = '{"providers":{}}\n'

HttpGet = Callable[[str, dict[str, str]], bytes]


class PluginError(RuntimeError):
    """The plugin could not be fetched or did not match its pin."""


@dataclass(frozen=True)
class PluginPin:
    registry: str
    repository: str
    manifest_digest: str  # "sha256:<hex>"
    blob_sha256: str  # hex, the sha256 of the wasm file itself

    @classmethod
    def from_config(cls, table: dict[str, str]) -> PluginPin:
        pin = cls(
            registry=table["registry"],
            repository=table["repository"],
            manifest_digest=table["manifest_digest"],
            blob_sha256=table["blob_sha256"].removeprefix("sha256:"),
        )
        if not pin.manifest_digest.startswith("sha256:") or len(pin.blob_sha256) != 64:
            raise PluginError("plugin pin needs sha256 digests (manifest_digest, blob_sha256)")
        return pin

    @property
    def filename(self) -> str:
        return f"{self.blob_sha256}.wasm"


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def httpx_get(url: str, headers: dict[str, str]) -> bytes:
    # httpx drops Authorization when a redirect leaves the origin (ghcr -> CDN).
    response = httpx.get(url, headers=headers, follow_redirects=True, timeout=120)
    if response.status_code != 200:
        raise PluginError(f"GET {url} -> HTTP {response.status_code}")
    return response.content


def fetch_plugin(pin: PluginPin, cache_dir: str | Path, http_get: HttpGet = httpx_get) -> Path:
    """Return a local path to the pinned wasm, downloading it only if not cached.

    The file is named by its SHA-256 and re-hashed on every call, so a corrupted or
    swapped cache entry is never used.
    """
    cache = Path(cache_dir)
    cache.mkdir(parents=True, exist_ok=True)
    target = cache / pin.filename
    if target.is_file() and sha256_hex(target.read_bytes()) == pin.blob_sha256:
        return target

    base = f"https://{pin.registry}/v2/{pin.repository}"
    token_url = f"https://{pin.registry}/token?scope=repository:{pin.repository}:pull"
    try:
        token = json.loads(http_get(token_url, {}))["token"]
    except (KeyError, ValueError) as error:
        raise PluginError(f"no pull token from {pin.registry}: {error}") from error
    auth = {"Authorization": f"Bearer {token}"}

    manifest_bytes = http_get(
        f"{base}/manifests/{pin.manifest_digest}", {**auth, "Accept": MANIFEST_ACCEPT}
    )
    if f"sha256:{sha256_hex(manifest_bytes)}" != pin.manifest_digest:
        raise PluginError("manifest does not match the pinned manifest_digest")
    layers = [
        layer
        for layer in json.loads(manifest_bytes).get("layers", [])
        if layer.get("mediaType") == WASM_MEDIA_TYPE
    ]
    if len(layers) != 1 or layers[0]["digest"] != f"sha256:{pin.blob_sha256}":
        raise PluginError("manifest wasm layer does not match the pinned blob_sha256")

    blob = http_get(f"{base}/blobs/sha256:{pin.blob_sha256}", auth)
    actual = sha256_hex(blob)
    if actual != pin.blob_sha256:
        raise PluginError(f"plugin sha256 mismatch: expected {pin.blob_sha256}, got {actual}")

    # Write atomically so a concurrent fetch never sees a partial file.
    fd, tmp = tempfile.mkstemp(dir=cache, suffix=".part")
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(blob)
        os.replace(tmp, target)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)
    return target


def endpoint_host(base_url: str) -> str:
    host = urlsplit(base_url).hostname
    if not host:
        raise PluginError(f"cannot derive a host from endpoint URL {base_url!r}")
    return host


def providers_toml(plugin_path: str, endpoint_url: str) -> str:
    """providers.toml with the openai provider as a local file allowed to reach the endpoint.

    The plugin only allows its default host (api.openai.com); the training proxy and
    the serving endpoint are other hosts, so they must be listed explicitly.
    """
    return (
        "[[providers]]\n"
        'name = "openai"\n'
        f"path = {json.dumps(plugin_path)}\n"
        "[providers.config]\n"
        f"allowed_hosts = [{json.dumps(endpoint_host(endpoint_url))}]\n"
    )
