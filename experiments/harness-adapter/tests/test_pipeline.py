"""The pipeline command: preflight, stage markers, resume, and the stage logic on fake services."""

from __future__ import annotations

import contextlib
import json
import stat
from pathlib import Path

import pytest

from querymt_harbor.config import load_reference
from querymt_harbor.evaluate import EvalTask, eval_path
from querymt_harbor.pipeline import (
    STAGES,
    Context,
    PipelineError,
    PreflightFailed,
    Probes,
    completed_stages,
    is_done,
    main,
    marker_path,
    preflight,
    read_marker,
    run,
    write_marker,
)
from querymt_harbor.qualify import QualificationFailed
from querymt_harbor.select import SWEBENCH, TERMINAL_BENCH

REFERENCE = load_reference()


def passing_probes(**overrides) -> Probes:
    probes = Probes(
        dataset_access=lambda repo, revision: None,
        headless=lambda ref: {"linux-x86_64": "a" * 64},
        plugin=lambda ref: REFERENCE.plugin.blob_sha256,
        free_gb=lambda path: 1000.0,
    )
    for name, value in overrides.items():
        setattr(probes, name, value)
    return probes


class NoServices:
    """Any use of a service is a failure: preflight must stop a run before it starts any."""

    def __getattr__(self, name):
        raise AssertionError(f"a service was used: {name}")


def fake_nvidia_smi(directory: Path, gpus: int) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    script = directory / "nvidia-smi"
    lines = "\n".join(f'echo "GPU {i}: NVIDIA H200 (UUID: GPU-{i})"' for i in range(gpus))
    script.write_text(f"#!/bin/sh\n{lines}\n")
    script.chmod(script.stat().st_mode | stat.S_IEXEC)
    return directory


def context(tmp_path, env, **kwargs) -> Context:
    return Context(
        raw=REFERENCE.raw,
        reference=REFERENCE,
        run_dir=tmp_path / "run",
        env=env,
        **kwargs,
    )


def test_preflight_reports_missing_prerequisites(tmp_path, monkeypatch):
    # One GPU, a Hugging Face token but no sandbox credential.
    monkeypatch.setenv("PATH", str(fake_nvidia_smi(tmp_path / "bin", gpus=1)))
    ctx = context(tmp_path, {"HF_TOKEN": "hf_x"}, services=NoServices(), probes=passing_probes())

    with pytest.raises(PreflightFailed) as failure:
        run(ctx, log=lambda _m: None)

    # Both problems are reported together, and nothing started or was marked done.
    message = str(failure.value)
    assert "1 GPU visible" in message
    assert "at least 2" in message
    assert "E2B_API_KEY is not set" in message
    assert "HF_TOKEN" not in message
    assert completed_stages(ctx.run_dir) == []


def test_preflight_lists_everything_that_is_wrong(tmp_path):
    def unreadable(repo, revision):
        raise PermissionError("gated")

    probes = passing_probes(
        gpus=list,
        dataset_access=unreadable,
        headless=lambda ref: (_ for _ in ()).throw(RuntimeError("sha256 mismatch")),
        free_gb=lambda path: 3.0,
    )
    with pytest.raises(PreflightFailed) as failure:
        preflight(REFERENCE.raw, REFERENCE, tmp_path / "run", {}, probes)
    problems = failure.value.problems
    assert any("0 GPUs visible" in p for p in problems)
    assert any(p.startswith("E2B_API_KEY") for p in problems)
    assert any(p.startswith("HF_TOKEN") for p in problems)
    assert any("harness-hub/scale800-harbor@main is not readable: gated" in p for p in problems)
    assert any("qmt-headless archive: sha256 mismatch" in p for p in problems)
    assert any("3 GB free" in p for p in problems)


def test_the_sandbox_credential_follows_the_provider(tmp_path):
    probes = passing_probes(gpus=lambda: ["GPU 0", "GPU 1"])
    # Docker needs no credential; Modal needs its token pair.
    preflight(
        REFERENCE.raw, REFERENCE, tmp_path, {"QMT_SANDBOX": "docker", "HF_TOKEN": "x"}, probes
    )
    with pytest.raises(PreflightFailed, match="MODAL_TOKEN_SECRET"):
        preflight(
            REFERENCE.raw,
            REFERENCE,
            tmp_path,
            {"QMT_SANDBOX": "modal", "MODAL_TOKEN_ID": "i", "HF_TOKEN": "x"},
            probes,
        )


def test_preflight_marker_lists_gpus_credentials_and_revisions(tmp_path):
    env = {"E2B_API_KEY": "secret-e2b", "HF_TOKEN": "secret-hf"}
    ctx = context(tmp_path, env, probes=passing_probes(gpus=lambda: ["GPU 0: H200", "GPU 1: H200"]))
    run(ctx, only="preflight", log=lambda _m: None)

    marker = read_marker(ctx.run_dir, "preflight")
    outputs = marker["outputs"]
    assert outputs["gpus"] == ["GPU 0: H200", "GPU 1: H200"]
    assert outputs["credentials_present"] == ["E2B_API_KEY", "HF_TOKEN"]
    assert outputs["revisions"]["datasets"]["harness-hub/scale800-harbor"] == "main"
    assert outputs["revisions"]["plugin_blob_sha256"] == REFERENCE.plugin.blob_sha256
    assert "profile_sha256" in outputs["revisions"]["harness"]
    # Names only: a credential value is never written to the run directory.
    text = marker_path(ctx.run_dir, "preflight").read_text()
    assert "secret-e2b" not in text
    assert "secret-hf" not in text


def recording_stages(calls, fail_at=None):
    def make(name):
        def stage(ctx):
            calls.append(name)
            if name == fail_at:
                raise RuntimeError(f"{name} broke")
            return {"stage": name}

        return stage

    return {name: make(name) for name in STAGES}


def test_rerun_resumes_after_the_last_completed_stage(tmp_path):
    ctx = context(tmp_path, {})
    for stage in ("preflight", "qualify", "baseline"):
        write_marker(ctx.run_dir, stage, {"stage": stage})
    calls: list[str] = []

    ran = run(ctx, stages=recording_stages(calls), log=lambda _m: None)

    assert calls == ["train", "evaluate", "select", "publish"]
    assert [r.stage for r in ran if r.skipped] == ["preflight", "qualify", "baseline"]
    assert completed_stages(ctx.run_dir) == list(STAGES)


def test_a_failing_stage_has_no_marker_and_the_rerun_starts_there(tmp_path):
    ctx = context(tmp_path, {})
    calls: list[str] = []
    with pytest.raises(RuntimeError, match="train broke"):
        run(ctx, stages=recording_stages(calls, fail_at="train"), log=lambda _m: None)
    assert completed_stages(ctx.run_dir) == ["preflight", "qualify", "baseline"]

    calls.clear()
    run(ctx, stages=recording_stages(calls), log=lambda _m: None)
    assert calls == ["train", "evaluate", "select", "publish"]


def test_a_stage_can_run_alone_and_be_forced(tmp_path):
    ctx = context(tmp_path, {})
    calls: list[str] = []
    stages = recording_stages(calls)

    run(ctx, only="select", stages=stages, log=lambda _m: None)
    assert calls == ["select"]
    assert completed_stages(ctx.run_dir) == ["select"]  # no other stage ran or was marked

    run(ctx, only="select", stages=stages, log=lambda _m: None)  # done: skipped
    assert calls == ["select"]
    run(ctx, only="select", force=True, stages=stages, log=lambda _m: None)
    assert calls == ["select", "select"]

    with pytest.raises(PipelineError, match="unknown stage"):
        run(ctx, only="deploy", stages=stages)


def test_the_marker_is_written_whole_or_not_at_all(tmp_path):
    path = write_marker(tmp_path, "qualify", {"passed": True})
    assert json.loads(path.read_text())["outputs"] == {"passed": True}
    assert not list(path.parent.glob("*.tmp"))
    assert is_done(tmp_path, "qualify")


# --- the stage logic, on fake services ---------------------------------------------------------

SWE = [f"swe-{i}" for i in range(3)]
TB = [f"tb-{i:02d}" for i in range(20)]


class FakeServices:
    def __init__(self, run_dir: Path, qualifying=None):
        self.run_dir = run_dir
        self.served: list[list[str]] = []
        self.requests: list = []
        self.trained = False
        self.qualifying = qualifying or {
            "reward": 1.0,
            "n_turns": 5,
            "n_roots": 1,
            "n_trainable_tokens": 900,
            "atif": "match",
        }

    def qualification_rollout(self, run_dir):
        return self.qualifying

    def eval_tasks(self):
        return {
            SWEBENCH: [EvalTask(SWEBENCH, t, "swe", i) for i, t in enumerate(SWE)],
            TERMINAL_BENCH: [EvalTask(TERMINAL_BENCH, t, "tb", i) for i, t in enumerate(TB)],
        }

    def eval_runner(self):
        def runner(request):
            # The selection set must be on disk before the first evaluation rollout.
            assert (self.run_dir / "selection_set.json").is_file()
            self.requests.append(request)
            solved = request.model == "checkpoint-100"
            return {"ok": True, "reward": 1.0 if solved else 0.0, "n_turns": 2}

        return runner

    @contextlib.contextmanager
    def serve_eval(self, adapters):
        self.served.append(sorted(adapters))
        yield

    def train(self, config):
        self.trained = True
        for step in (80, 100):
            path = self.run_dir / "checkpoints" / f"checkpoint-{step}"
            path.mkdir(parents=True)
            (path / "trainer_state.json").write_text("{}")
            (path / "adapter_model.safetensors").write_text("x")


def test_stages_run_end_to_end_on_fake_services(tmp_path):
    ctx = context(tmp_path, {}, services=None, concurrency=1)
    ctx.services = FakeServices(ctx.run_dir)

    ran = run(ctx, stages={**_default(), "preflight": lambda c: {}}, log=lambda _m: None)

    assert [r.stage for r in ran] == list(STAGES)
    assert not any(r.skipped for r in ran)
    assert ctx.services.trained
    assert eval_path(ctx.run_dir, "base").is_file()
    assert eval_path(ctx.run_dir, "checkpoint-80").is_file()
    assert eval_path(ctx.run_dir, "checkpoint-100").is_file()
    # Base is served alone; the two checkpoints are served together.
    assert ctx.services.served == [[], ["checkpoint-100", "checkpoint-80"]]
    selection = json.loads((ctx.run_dir / "selection.json").read_text())
    assert selection["selected"] == "checkpoint-100"
    assert (ctx.run_dir / "eval_report.json").is_file()


def _default():
    from querymt_harbor.pipeline import DEFAULT_STAGES

    # Publishing needs a Hub repo; it is covered by tests/test_publish.py.
    return {**DEFAULT_STAGES, "publish": lambda c: {}}


def test_training_does_not_start_without_a_passing_qualification(tmp_path):
    ctx = context(tmp_path, {}, concurrency=1)
    ctx.services = FakeServices(
        ctx.run_dir,
        qualifying={
            "reward": 1.0,
            "n_turns": 5,
            "n_roots": 1,
            "n_trainable_tokens": 0,
            "atif": "match",
        },
    )
    with pytest.raises(QualificationFailed, match="no trainable tokens"):
        run(ctx, stages={**_default(), "preflight": lambda c: {}}, log=lambda _m: None)
    assert not ctx.services.trained
    assert completed_stages(ctx.run_dir) == ["preflight"]
    assert json.loads((ctx.run_dir / "qualify.json").read_text())["passed"] is False


def test_select_and_publish_say_what_is_missing(tmp_path):
    ctx = context(tmp_path, {}, concurrency=1)
    with pytest.raises(PipelineError, match="run the baseline stage first"):
        run(ctx, only="select", log=lambda _m: None)
    with pytest.raises(PipelineError, match="no Hub repository"):
        run(ctx, only="publish", log=lambda _m: None)


def test_status_lists_completed_stages(tmp_path, capsys):
    write_marker(tmp_path / "run", "preflight", {})
    assert main(["status", "--run-dir", str(tmp_path / "run")]) == 0
    lines = capsys.readouterr().out.splitlines()
    assert lines[0].split() == ["preflight", "done"]
    assert lines[1].split() == ["qualify", "pending"]
