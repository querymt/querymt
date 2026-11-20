"""Run the real `qmt-headless` binary (`run`, then `export`) and collect everything it wrote."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass
class HeadlessResult:
    run_returncode: int
    export_returncode: int
    stdout: str
    stderr: str
    export_stderr: str
    summary: dict[str, Any] | None
    atif: dict[str, Any] | None
    db_path: Path
    atif_path: Path
    summary_path: Path


def qmt_env(qmt_home: Path, base_url: str, model: str = "m", key: str = "k") -> dict[str, str]:
    return {
        "QMT_HOME": str(qmt_home),
        "QMT_CONFIG_DIR": str(qmt_home),
        "QMT_BASE_URL": base_url,
        "QMT_MODEL": model,
        "QMT_API_KEY": key,
    }


def _load(path: Path) -> dict[str, Any] | None:
    return json.loads(path.read_text()) if path.is_file() else None


def run_args(
    headless_bin: Path, profile: Path, work: Path, instruction_file: Path, db: Path
) -> list[str]:
    return [
        str(headless_bin),
        "run",
        str(profile),
        "--cwd",
        str(work),
        "--instruction-file",
        str(instruction_file),
        "--db",
        str(db),
    ]


def export_args(headless_bin: Path, db: Path, atif: Path, summary: Path) -> list[str]:
    return [
        str(headless_bin),
        "export",
        "--db",
        str(db),
        "--atif",
        str(atif),
        "--summary",
        str(summary),
    ]


def run_headless(
    headless_bin: Path,
    profile: Path,
    out_dir: Path,
    *,
    env: dict[str, str],
    instruction: str = "do the task",
    cwd: Path | None = None,
    timeout: float = 180,
) -> HeadlessResult:
    """`qmt-headless run` with `--db` in `out_dir`, then `export`, as the agent does."""
    out_dir.mkdir(parents=True, exist_ok=True)
    work = cwd or out_dir / "work"
    work.mkdir(parents=True, exist_ok=True)
    instruction_file = out_dir / "instruction.txt"
    instruction_file.write_text(instruction)
    db = out_dir / "sessions.db"
    atif_path = out_dir / "trajectory.json"
    summary_path = out_dir / "summary.json"
    full_env = {**os.environ, **env}

    run = subprocess.run(
        run_args(headless_bin, profile, work, instruction_file, db),
        env=full_env,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    export = subprocess.run(
        export_args(headless_bin, db, atif_path, summary_path),
        env=full_env,
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    return HeadlessResult(
        run_returncode=run.returncode,
        export_returncode=export.returncode,
        stdout=run.stdout,
        stderr=run.stderr,
        export_stderr=export.stderr,
        summary=_load(summary_path),
        atif=_load(atif_path),
        db_path=db,
        atif_path=atif_path,
        summary_path=summary_path,
    )


def profile_with_max_steps(profile: Path, root: Path, max_steps: int) -> Path:
    """A copy of the profile with another `max_steps`, keeping relative prompt paths valid."""
    (root / "confs").mkdir(parents=True)
    shutil.copytree(profile.parent.parent / "prompts", root / "prompts")
    text = profile.read_text()
    assert "max_steps = 50" in text
    copy = root / "confs" / profile.name
    copy.write_text(text.replace("max_steps = 50", f"max_steps = {max_steps}"))
    return copy
