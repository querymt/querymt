# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Entry point of the pipeline on HF Jobs (`launcher.hf_jobs_command` builds the command).

    hf jobs uv run --flavor h200x2 --secrets HF_TOKEN --secrets E2B_API_KEY \\
        --volume hf://buckets/<org>/<bucket>:/data \\
        scripts/hf_job.py run --run-dir /data/runs/<name>

The job container only has `uv`, so this script fetches the experiment at a pinned revision,
installs it with the GPU extra, and runs `python -m querymt_harbor.pipeline` with the
arguments it was given. The run directory is on the mounted bucket: checkpoints and stage
markers survive the job, and running the same command again resumes where the markers say.

    QMT_REPO_URL   git URL of the repository (default: the public QueryMT repository)
    QMT_REPO_REV   commit or tag to run; required, so a job is always reproducible
    QMT_SRC_DIR    where to clone (default /tmp/querymt)
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

REPO_URL = "https://github.com/querymt/querymt.git"


def run(args: list[str], cwd: Path | None = None) -> None:
    print("+", " ".join(args), flush=True)
    subprocess.run(args, cwd=cwd, check=True)


def main(argv: list[str]) -> int:
    revision = os.environ.get("QMT_REPO_REV")
    if not revision:
        print("QMT_REPO_REV is required: pass it with --env QMT_REPO_REV=<commit or tag>")
        return 2
    src = Path(os.environ.get("QMT_SRC_DIR", "/tmp/querymt"))
    if not (src / ".git").is_dir():
        run(["git", "clone", os.environ.get("QMT_REPO_URL", REPO_URL), str(src)])
    run(["git", "fetch", "--depth", "1", "origin", revision], cwd=src)
    run(["git", "checkout", "--detach", "FETCH_HEAD"], cwd=src)

    package = src / "experiments" / "harness-adapter"
    run(["uv", "sync", "--extra", "gpu", "--frozen"], cwd=package)
    os.chdir(package)
    os.execvp(
        "uv",
        ["uv", "run", "--frozen", "python", "-m", "querymt_harbor.pipeline", *argv],
    )
    return 0  # unreachable: execvp replaces this process


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
