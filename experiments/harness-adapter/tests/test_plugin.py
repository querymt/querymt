import json
import os

import pytest

from headless_run import qmt_env, run_headless
from querymt_harbor.plugin import (
    EMPTY_MODELS_REGISTRY,
    PluginError,
    PluginPin,
    endpoint_host,
    fetch_plugin,
    providers_toml,
    sha256_hex,
)

WASM = b"\x00asm fake plugin bytes"
REGISTRY = "registry.test"
REPO = "org/openai"


def make_registry(served: bytes = WASM, *, layer_digest: str | None = None):
    """A fake OCI registry: returns an http_get callable and the pin that matches it.

    The manifest always advertises the pinned WASM digest; `served` is what the blob
    endpoint actually returns, so `served != WASM` models a tampered blob.
    """
    blob_hex = sha256_hex(WASM)
    manifest = json.dumps(
        {
            "layers": [
                {
                    "mediaType": "application/vnd.wasm.v1.layer+wasm",
                    "digest": layer_digest or f"sha256:{blob_hex}",
                }
            ]
        }
    ).encode()
    calls: list[str] = []

    def http_get(url: str, headers: dict[str, str]) -> bytes:
        calls.append(url)
        if "/token" in url:
            return b'{"token": "t"}'
        if "/manifests/" in url:
            assert headers["Authorization"] == "Bearer t"
            return manifest
        if "/blobs/" in url:
            return served
        raise AssertionError(url)

    pin = PluginPin(REGISTRY, REPO, f"sha256:{sha256_hex(manifest)}", sha256_hex(WASM))
    return http_get, pin, calls


def test_fetch_downloads_verifies_and_caches(tmp_path):
    http_get, pin, calls = make_registry()

    path = fetch_plugin(pin, tmp_path, http_get)
    assert path.read_bytes() == WASM
    assert path.name == f"{pin.blob_sha256}.wasm"
    first = len(calls)

    assert fetch_plugin(pin, tmp_path, http_get) == path
    assert len(calls) == first, "second fetch is served from the verified cache"


def test_plugin_hash_mismatch_fails_fetch(tmp_path):
    # The registry serves different bytes than the pinned blob_sha256.
    http_get, pin, _ = make_registry(served=b"tampered")
    with pytest.raises(PluginError, match="sha256 mismatch"):
        fetch_plugin(pin, tmp_path, http_get)
    assert not list(tmp_path.glob("*.wasm")), "nothing is installed after a mismatch"


def test_manifest_digest_mismatch_fails_fetch(tmp_path):
    http_get, pin, _ = make_registry()
    bad = PluginPin(pin.registry, pin.repository, "sha256:" + "0" * 64, pin.blob_sha256)
    with pytest.raises(PluginError, match="manifest"):
        fetch_plugin(bad, tmp_path, http_get)


def test_manifest_layer_must_match_pinned_blob(tmp_path):
    http_get, pin, _ = make_registry(layer_digest="sha256:" + "1" * 64)
    with pytest.raises(PluginError, match="layer"):
        fetch_plugin(pin, tmp_path, http_get)


def test_corrupted_cache_entry_is_not_used(tmp_path):
    http_get, pin, calls = make_registry()
    path = fetch_plugin(pin, tmp_path, http_get)
    path.write_bytes(b"corrupt")

    assert fetch_plugin(pin, tmp_path, http_get).read_bytes() == WASM
    assert any("/blobs/" in c for c in calls[3:]), "re-downloaded after the cache failed its hash"


def test_providers_toml_points_at_local_file_and_allows_endpoint_host():
    text = providers_toml("/opt/qmt/plugins/openai.wasm", "http://proxy.internal:8787/v1")
    assert 'path = "/opt/qmt/plugins/openai.wasm"' in text
    assert 'allowed_hosts = ["proxy.internal"]' in text
    assert "oci://" not in text
    assert endpoint_host("https://api.example.com/v1") == "api.example.com"
    with pytest.raises(PluginError):
        endpoint_host("not a url")


def test_pin_requires_sha256_digests():
    with pytest.raises(PluginError):
        PluginPin.from_config(
            {"registry": "r", "repository": "x", "manifest_digest": "abc", "blob_sha256": "def"}
        )


def test_empty_models_registry_is_valid_json():
    assert json.loads(EMPTY_MODELS_REGISTRY) == {"providers": {}}


@pytest.mark.skipif(not os.environ.get("QMT_LIVE_PLUGIN"), reason="set QMT_LIVE_PLUGIN=1 (network)")
def test_live_pinned_plugin_runs_without_any_network_fetch(
    headless_bin, profile_path, fake_chat_server, tmp_path
):
    """Real ghcr.io pin -> real qmt-headless, with providers.toml and models cache pre-seeded."""
    import tomllib
    from pathlib import Path

    ref = tomllib.loads((Path(__file__).parents[1] / "configs/reference.toml").read_text())
    pin = PluginPin.from_config(ref["plugin"])
    wasm = fetch_plugin(pin, tmp_path / "cache")
    assert sha256_hex(wasm.read_bytes()) == pin.blob_sha256

    home = tmp_path / "home"
    home.mkdir()
    (home / "providers.toml").write_text(providers_toml(str(wasm), fake_chat_server.base_url))
    (home / "models.dev.json").write_text(EMPTY_MODELS_REGISTRY)

    fake_chat_server.script([{"text": "ok"}])
    result = run_headless(
        headless_bin,
        profile_path,
        tmp_path / "run",
        instruction="hi",
        env={
            **qmt_env(home, fake_chat_server.base_url, "m", "key-q"),
            # Unreachable proxies: any attempt to fetch plugins/metadata would hang or fail.
            "HTTPS_PROXY": "http://127.0.0.1:9",
            "HTTP_PROXY": "http://127.0.0.1:9",
            "NO_PROXY": "127.0.0.1",
            "XDG_CACHE_HOME": str(tmp_path / "empty-cache"),
            "HOME": str(tmp_path / "empty-home"),
        },
        timeout=60,
    )
    assert result.run_returncode == 0, result.stderr[-1500:]
    assert result.export_returncode == 0, result.export_stderr[-1500:]
    assert result.summary["outcome"] == "end_turn", result.summary
    req = fake_chat_server.requests[0]
    assert req.headers["authorization"] == "Bearer key-q"
