"""The training split (design D4).

Two Harbor task sources are sampled with a seed, interleaved in their ratio, checked against
the evaluation benchmarks and written as one local Harbor dataset directory:

    <out>/tasks/0000__scale800__<task>/   task.toml, instruction.md, environment/, tests/ ...
    <out>/manifest.json                   seed, revisions and one row per task

OpenEnv lists a dataset directory sorted by directory name and a task's index is its identity
downstream, so the interleaved order is encoded in the `NNNN__` prefix: sorted order is
training order. Task files are real files (hard links where possible, never symlinks): Harbor
tars a task's `tests/` into the sandbox and tar would preserve a dangling symlink.
"""

from __future__ import annotations

import hashlib
import json
import os
import random
import re
import shutil
import unicodedata
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any


class OverlapError(ValueError):
    """A training task has the same instruction as an evaluation-benchmark task."""


@dataclass(frozen=True)
class TaskRef:
    name: str
    path: Path
    sha1: str


@dataclass(frozen=True)
class Source:
    """One Harbor task source: a local root (containing `tasks/`), its provenance, a sample size."""

    name: str
    root: Path
    repo: str = ""
    revision: str = ""
    count: int | None = None
    """Tasks to sample; `None` takes all of them."""


def normalise_instruction(text: str) -> str:
    """NFKC, case-folded, whitespace collapsed: formatting must not hide a duplicate."""
    return re.sub(r"\s+", " ", unicodedata.normalize("NFKC", text)).strip().casefold()


def instruction_sha1(text: str) -> str:
    return hashlib.sha1(normalise_instruction(text).encode("utf-8")).hexdigest()  # noqa: S324


def list_tasks(root: Path) -> list[TaskRef]:
    """Task directories under `root` (or `root/tasks`), sorted by name, with instruction hashes."""
    base = root / "tasks" if (root / "tasks").is_dir() else root
    refs = []
    for path in sorted(p for p in base.iterdir() if p.is_dir() and not p.name.startswith(".")):
        instruction = path / "instruction.md"
        if not instruction.is_file():
            raise ValueError(f"{path} has no instruction.md; is it a Harbor task directory?")
        refs.append(TaskRef(path.name, path, instruction_sha1(instruction.read_text("utf-8"))))
    if not refs:
        raise ValueError(f"no Harbor tasks under {root}")
    return refs


def sample_tasks(
    tasks: Sequence[TaskRef], count: int | None, seed: int, label: str
) -> list[TaskRef]:
    """`count` tasks, chosen with a seed that is independent per source and stable across runs."""
    if count is None:
        return list(tasks)
    if count > len(tasks):
        raise ValueError(f"{label}: asked for {count} tasks but the source has {len(tasks)}")
    # A str seed is hashed with SHA-512 by `random`, so it does not depend on PYTHONHASHSEED.
    chosen = random.Random(f"{seed}/{label}").sample(list(tasks), count)
    return sorted(chosen, key=lambda t: t.name)


def interleave(groups: Sequence[Sequence[Any]]) -> list[tuple[int, Any]]:
    """Merge `groups` keeping every prefix as close to the groups' ratio as possible.

    At each position the group furthest below its share goes next; ties go to the earlier
    group. Deterministic, and with 750 and 300 it repeats a 5:2 pattern.
    """
    total = sum(len(g) for g in groups)
    emitted = [0] * len(groups)
    out: list[tuple[int, Any]] = []
    for position in range(total):
        best, best_deficit = -1, float("-inf")
        for i, group in enumerate(groups):
            if emitted[i] >= len(group):
                continue
            deficit = len(group) * (position + 1) / total - emitted[i]
            if deficit > best_deficit:
                best, best_deficit = i, deficit
        out.append((best, groups[best][emitted[best]]))
        emitted[best] += 1
    return out


def _link_or_copy(src: str, dst: str) -> None:
    try:
        os.link(src, dst)
    except OSError:  # another filesystem, or links not supported
        shutil.copy2(src, dst)


def build_split(
    sources: Sequence[Source],
    benchmarks: Sequence[Source],
    out_dir: Path,
    seed: int,
) -> dict[str, Any]:
    """Write the training dataset to `out_dir` and return (and save) its manifest.

    Raises `OverlapError`, naming both tasks, when a sampled task's instruction hash equals a
    benchmark task's. Nothing is written in that case.
    """
    benchmark_hashes: dict[str, tuple[str, str]] = {}
    for bench in benchmarks:
        for task in list_tasks(bench.root):
            benchmark_hashes.setdefault(task.sha1, (bench.name, task.name))

    groups = [sample_tasks(list_tasks(src.root), src.count, seed, src.name) for src in sources]
    for src, group in zip(sources, groups, strict=True):
        for task in group:
            if task.sha1 in benchmark_hashes:
                bench_name, bench_task = benchmark_hashes[task.sha1]
                raise OverlapError(
                    f"training task {src.name}/{task.name} has the same instruction as "
                    f"{bench_name}/{bench_task} (sha1 {task.sha1})"
                )

    ordered = interleave(groups)
    if (out_dir / "tasks").exists():
        raise FileExistsError(f"{out_dir / 'tasks'} already exists; refusing to overwrite it")
    rows = []
    for index, (source_index, task) in enumerate(ordered):
        src = sources[source_index]
        dir_name = f"{index:04d}__{src.name}__{task.name}"
        shutil.copytree(
            task.path, out_dir / "tasks" / dir_name, symlinks=False, copy_function=_link_or_copy
        )
        rows.append(
            {
                "index": index,
                "id": dir_name,
                "task": task.name,
                "source": src.name,
                "revision": src.revision,
                "instruction_sha1": task.sha1,
            }
        )

    manifest = {
        "seed": seed,
        "sources": [
            {"name": s.name, "repo": s.repo, "revision": s.revision, "sampled": len(g)}
            for s, g in zip(sources, groups, strict=True)
        ],
        "benchmarks": [
            {"name": b.name, "repo": b.repo, "revision": b.revision} for b in benchmarks
        ],
        "tasks": rows,
    }
    (out_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", "utf-8")
    return manifest


Downloader = Callable[[str, str], Path]
"""`(repo, revision) -> local root containing tasks/`."""


def hub_download(cache: Path) -> Downloader:
    """Download a dataset's `tasks/` from the Hub as real files into `cache`."""

    def download(repo: str, revision: str) -> Path:
        from huggingface_hub import snapshot_download

        # One directory per (repo, revision): a directory reused across revisions would keep the
        # files a later revision deleted, and so list tasks the pinned revision does not have.
        label = re.sub(r"[^A-Za-z0-9._-]+", "_", revision or "default")
        target = cache / f"{repo.replace('/', '__')}@{label}"
        snapshot_download(
            repo,
            repo_type="dataset",
            revision=revision or None,
            allow_patterns=["tasks/**"],
            local_dir=str(target),
        )
        return target

    return download


def build_from_config(
    raw: dict[str, Any], out_dir: Path, download: Downloader, *, seed: int | None = None
) -> dict[str, Any]:
    """Build the split from the `[data]` table of `reference.toml`."""
    data = raw["data"]
    seed = raw["run"]["seed"] if seed is None else seed
    sources = [
        Source(
            name,
            download(spec["repo"], spec["revision"]),
            spec["repo"],
            spec["revision"],
            spec["tasks"],
        )
        for name, spec in ((k, v) for k, v in data.items() if k != "benchmarks")
    ]
    benchmarks = [
        Source(
            spec["repo"].split("/")[-1],
            download(spec["repo"], spec["revision"]),
            spec["repo"],
            spec["revision"],
        )
        for spec in data["benchmarks"]
    ]
    return build_split(sources, benchmarks, out_dir, seed)
