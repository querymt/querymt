"""The training split (harness-adapter-training / Training task mix)."""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

from querymt_harbor.dataset import (
    OverlapError,
    Source,
    build_from_config,
    build_split,
    instruction_sha1,
    interleave,
    list_tasks,
    normalise_instruction,
)


def make_source(root: Path, prefix: str, n: int, *, instruction=None) -> Path:
    """A Harbor-style dataset: `tasks/<name>/{task.toml,instruction.md,tests/test.sh}`."""
    for i in range(n):
        task = root / "tasks" / f"{prefix}-{i:03d}"
        (task / "tests").mkdir(parents=True)
        (task / "task.toml").write_text("version = '1.0'\n")
        text = instruction(i) if instruction else f"{prefix}: solve problem number {i}"
        (task / "instruction.md").write_text(text)
        (task / "tests" / "test.sh").write_text("#!/bin/sh\nexit 0\n")
    return root


@pytest.fixture
def sources(tmp_path):
    a = make_source(tmp_path / "src-a", "alpha", 20)
    b = make_source(tmp_path / "src-b", "beta", 12)
    return [
        Source("scale800", a, "org/scale800", "rev-a", 10),
        Source("termgrade", b, "org/tg", "rev-b", 4),
    ]


@pytest.fixture
def benchmarks(tmp_path):
    bench = make_source(tmp_path / "bench", "bench", 5)
    return [Source("terminal-bench-2.1", bench, "org/tb", "rev-t")]


def test_task_order_is_reproducible(tmp_path, sources, benchmarks):
    first = build_split(sources, benchmarks, tmp_path / "out1", seed=7)
    second = build_split(sources, benchmarks, tmp_path / "out2", seed=7)

    ids = [t["id"] for t in first["tasks"]]
    assert ids == [t["id"] for t in second["tasks"]]
    assert len(ids) == 14
    # Sorted directory order (how OpenEnv indexes a dataset) is training order.
    on_disk = sorted(p.name for p in (tmp_path / "out1" / "tasks").iterdir())
    assert on_disk == ids == sorted(ids)

    other_seed = build_split(sources, benchmarks, tmp_path / "out3", seed=8)
    assert [t["task"] for t in other_seed["tasks"]] != [t["task"] for t in first["tasks"]]


def test_manifest_records_provenance(tmp_path, sources, benchmarks):
    build_split(sources, benchmarks, tmp_path / "out", seed=3)
    manifest = json.loads((tmp_path / "out" / "manifest.json").read_text())

    assert manifest["seed"] == 3
    assert [(s["name"], s["revision"], s["sampled"]) for s in manifest["sources"]] == [
        ("scale800", "rev-a", 10),
        ("termgrade", "rev-b", 4),
    ]
    assert manifest["benchmarks"][0]["repo"] == "org/tb"
    row = manifest["tasks"][0]
    assert set(row) == {"index", "id", "task", "source", "revision", "instruction_sha1"}
    assert row["id"].startswith("0000__")


def test_sources_are_interleaved_in_their_ratio(tmp_path, sources, benchmarks):
    manifest = build_split(sources, benchmarks, tmp_path / "out", seed=1)
    order = [t["source"] for t in manifest["tasks"]]
    # 10:4 is 5:2: every prefix stays within one task of the ratio, and the totals are exact.
    assert order.count("scale800") == 10 and order.count("termgrade") == 4
    for n in range(1, len(order) + 1):
        expected = n * 10 / 14
        assert abs(order[:n].count("scale800") - expected) <= 1


def test_interleave_is_deterministic_and_complete():
    merged = interleave([list("AAAAA"), list("BB")])
    assert [item for _, item in merged].count("A") == 5
    # Each prefix is within one task of the 5:2 ratio; ties go to the earlier group.
    assert "".join(item for _, item in merged) == "ABAAABA"
    assert interleave([[], list("xy")]) == [(1, "x"), (1, "y")]


def test_benchmark_overlap_is_rejected(tmp_path, benchmarks):
    # Same instruction as bench-002, differing only in case and whitespace.
    leaky = make_source(
        tmp_path / "leaky",
        "leak",
        3,
        instruction=lambda i: "  BENCH: solve   problem number 2\n" if i == 1 else f"other {i}",
    )
    # `make_source` writes "<prefix>: ..." for benchmarks; align the text exactly.
    (tmp_path / "bench" / "tasks" / "bench-002" / "instruction.md").write_text(
        "bench: solve problem number 2"
    )
    out = tmp_path / "out"

    with pytest.raises(OverlapError) as excinfo:
        build_split([Source("scale800", leaky, count=3)], benchmarks, out, seed=0)

    message = str(excinfo.value)
    assert "scale800/leak-001" in message
    assert "terminal-bench-2.1/bench-002" in message
    assert not (out / "tasks").exists(), "nothing is written when the build fails"


def test_unsampled_overlap_does_not_fail_the_build(tmp_path, benchmarks):
    (tmp_path / "bench" / "tasks" / "bench-000" / "instruction.md").write_text("shared text")
    src = make_source(
        tmp_path / "src", "s", 6, instruction=lambda i: "shared text" if i == 5 else f"t{i}"
    )
    # The overlapping task is s-005; sample the other five only.
    manifest = None
    for seed in range(50):
        try:
            manifest = build_split(
                [Source("x", src, count=2)], benchmarks, tmp_path / f"o{seed}", seed
            )
            break
        except OverlapError:
            continue
    assert manifest is not None, "some seed must avoid the overlapping task"


def test_count_larger_than_the_source_is_an_error(tmp_path, benchmarks):
    src = make_source(tmp_path / "src", "s", 2)
    with pytest.raises(ValueError, match="asked for 5 tasks but the source has 2"):
        build_split([Source("x", src, count=5)], benchmarks, tmp_path / "out", seed=0)


def test_existing_output_is_not_overwritten(tmp_path, sources, benchmarks):
    build_split(sources, benchmarks, tmp_path / "out", seed=0)
    with pytest.raises(FileExistsError):
        build_split(sources, benchmarks, tmp_path / "out", seed=0)


def test_task_files_are_real_files_not_symlinks(tmp_path, sources, benchmarks):
    # An HF cache stores snapshot files as symlinks into blobs/; Harbor tars them verbatim.
    blob = tmp_path / "blobs" / "abc"
    blob.parent.mkdir()
    blob.write_text("#!/bin/sh\nexit 0\n")
    victim = sources[0].root / "tasks" / "alpha-000" / "tests" / "test.sh"
    victim.unlink()
    os.symlink(blob, victim)

    manifest = build_split(sources, benchmarks, tmp_path / "out", seed=5)

    for path in (tmp_path / "out" / "tasks").rglob("*"):
        assert not path.is_symlink(), path
    assert manifest["tasks"]


def test_normalisation_ignores_case_and_whitespace():
    assert normalise_instruction("  Fix\tthe   BUG\n") == "fix the bug"
    assert instruction_sha1("Fix the bug") == instruction_sha1("fix   the\nbug ")
    assert instruction_sha1("Fix the bug") != instruction_sha1("Fix the bugs")


def test_task_without_instruction_is_rejected(tmp_path):
    (tmp_path / "tasks" / "broken").mkdir(parents=True)
    with pytest.raises(ValueError, match="no instruction.md"):
        list_tasks(tmp_path)


def test_build_from_config_uses_the_data_table(tmp_path):
    roots = {
        "org/scale": make_source(tmp_path / "scale", "sc", 8),
        "org/term": make_source(tmp_path / "term", "te", 5),
        "org/bench": make_source(tmp_path / "bench", "be", 3),
    }
    calls = []

    def download(repo: str, revision: str) -> Path:
        calls.append((repo, revision))
        return roots[repo]

    raw = {
        "run": {"seed": 4},
        "data": {
            "scale800": {"repo": "org/scale", "revision": "r1", "tasks": 5},
            "termgrade": {"repo": "org/term", "revision": "r2", "tasks": 2},
            "benchmarks": [{"repo": "org/bench", "revision": "r3"}],
        },
    }
    manifest = build_from_config(raw, tmp_path / "out", download)

    assert manifest["seed"] == 4
    assert len(manifest["tasks"]) == 7
    assert calls == [("org/scale", "r1"), ("org/term", "r2"), ("org/bench", "r3")]
