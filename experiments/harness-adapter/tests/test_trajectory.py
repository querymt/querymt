"""ATIF and summary exported by the real qmt-headless against a scripted endpoint."""

from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import time
from pathlib import Path

from harbor.models.trajectories import Trajectory

from headless_run import (
    export_args,
    profile_with_max_steps,
    qmt_env,
    run_args,
    run_headless,
)

LS = {"name": "ls", "arguments": {"path": "."}}


def agent_steps(atif: dict) -> list[dict]:
    return [s for s in atif["steps"] if s["source"] == "agent"]


def run(headless_bin, profile, qmt_home, server, tmp_path, **kw):
    return run_headless(
        headless_bin, profile, tmp_path / "run", env=qmt_env(qmt_home, server.base_url), **kw
    )


def test_trajectory_matches_model_calls(
    headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path
):
    usages = [(101, 11), (202, 22), (303, 33)]
    fake_chat_server.script(
        [
            {"tool_calls": [LS], "usage": dict(prompt_tokens=101, completion_tokens=11)},
            {"tool_calls": [LS], "usage": dict(prompt_tokens=202, completion_tokens=22)},
            {"text": "done", "usage": dict(prompt_tokens=303, completion_tokens=33)},
        ]
    )
    result = run(headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path)

    assert result.run_returncode == 0, result.stderr[-1500:]
    assert result.export_returncode == 0, result.export_stderr[-1500:]
    assert len(fake_chat_server.requests) == 3
    assert result.atif is not None, result.export_stderr[-1500:]
    steps = agent_steps(result.atif)
    assert [(s["metrics"]["prompt_tokens"], s["metrics"]["completion_tokens"]) for s in steps] == (
        usages
    )
    assert result.summary["model_calls"] == 3
    assert result.summary["outcome"] == "end_turn"
    assert result.summary["session_id"] == result.atif["session_id"]
    Trajectory.model_validate(result.atif)


def test_trajectory_survives_the_budget_stop(
    headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path
):
    copy = profile_with_max_steps(profile_path, tmp_path / "profile", 3)
    fake_chat_server.script([{"tool_calls": [LS]}])
    result = run(headless_bin, copy, qmt_home, fake_chat_server, tmp_path)

    assert result.run_returncode == 0, result.stderr[-1500:]
    assert result.export_returncode == 0, result.export_stderr[-1500:]
    assert result.atif is not None
    assert len(agent_steps(result.atif)) == 3
    assert result.summary["outcome"] == "stopped"
    assert result.summary["stop_type"] == "step_limit"
    Trajectory.model_validate(result.atif)


def test_trajectory_survives_a_killed_run(
    headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path
):
    """Two answered calls, then the endpoint stalls; the run is SIGKILLed mid-request."""
    fake_chat_server.script(
        [
            {"tool_calls": [LS]},
            {"tool_calls": [LS]},
            {"text": "never delivered", "stall": 120},
        ]
    )
    out = tmp_path / "run"
    work = out / "work"
    work.mkdir(parents=True)
    instruction = out / "instruction.txt"
    instruction.write_text("do the task")
    db = out / "sessions.db"
    env = {**os.environ, **qmt_env(qmt_home, fake_chat_server.base_url)}

    proc = subprocess.Popen(
        run_args(headless_bin, profile_path, work, instruction, db),
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        deadline = time.monotonic() + 90
        while len(fake_chat_server.requests) < 3:
            assert proc.poll() is None, "run exited before the third request"
            assert time.monotonic() < deadline, "third request never arrived"
            time.sleep(0.05)
        proc.kill()
    finally:
        proc.kill()
        proc.wait(timeout=30)

    atif_path = out / "trajectory.json"
    summary_path = out / "summary.json"
    export = subprocess.run(
        export_args(headless_bin, db, atif_path, summary_path),
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    assert export.returncode == 0, export.stderr[-1500:]
    atif = json.loads(atif_path.read_text())
    summary = json.loads(summary_path.read_text())
    # The third call was started but never finished: only the two completed calls are steps.
    assert len(agent_steps(atif)) == 2, [s["source"] for s in atif["steps"]]
    assert summary["outcome"] == "incomplete", summary
    assert summary["model_calls"] == 2
    Trajectory.model_validate(atif)


def _counts(db: Path) -> tuple[int, int, int]:
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        sessions = conn.execute("SELECT COUNT(*) FROM sessions").fetchone()[0]
        events, max_seq = conn.execute(
            "SELECT COUNT(*), MAX(stream_seq) FROM event_journal"
        ).fetchone()
        return sessions, events, max_seq
    finally:
        conn.close()


def test_export_only_reads(headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path):
    fake_chat_server.script([{"tool_calls": [LS]}, {"text": "done"}])
    result = run(headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path)
    assert result.run_returncode == 0, result.stderr[-1500:]

    before = _counts(result.db_path)
    assert before[0] == 1 and before[1] > 0
    env = {**os.environ, **qmt_env(qmt_home, fake_chat_server.base_url)}
    for attempt in range(2):
        export = subprocess.run(
            export_args(
                headless_bin,
                result.db_path,
                tmp_path / f"again{attempt}.json",
                tmp_path / f"again{attempt}-summary.json",
            ),
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        assert export.returncode == 0, export.stderr[-1500:]
    assert _counts(result.db_path) == before


def test_session_data_stays_in_the_log_directory(
    headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path
):
    home = tmp_path / "home"
    work = tmp_path / "work"
    home.mkdir()
    work.mkdir()
    fake_chat_server.script([{"tool_calls": [LS]}, {"text": "done"}])
    result = run_headless(
        headless_bin,
        profile_path,
        tmp_path / "run",
        env={**qmt_env(qmt_home, fake_chat_server.base_url), "HOME": str(home)},
        cwd=work,
    )

    assert result.run_returncode == 0, result.stderr[-1500:]
    assert result.atif is not None and result.summary is not None
    assert result.db_path.is_file()
    for root in (home, qmt_home, work):
        leftovers = [
            p for p in root.rglob("*") if p.suffix == ".db" or p.name.endswith(".run-leases")
        ]
        assert not leftovers, leftovers
