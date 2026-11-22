"""Isolate the provider and workspace defaults used by the public Python API."""

import pytest


@pytest.fixture(scope="session", autouse=True)
def test_home(tmp_path_factory):
    root = tmp_path_factory.mktemp("querymt-home")
    with pytest.MonkeyPatch.context() as env:
        for name in (
            "HOME",
            "QMT_HOME",
            "QMT_CONFIG_DIR",
            "QMT_PROVIDER_CACHE_DIR",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
        ):
            path = root / name.lower()
            path.mkdir()
            env.setenv(name, str(path))
        env.delenv("QMT_SESSIONS_DB", raising=False)
        yield
