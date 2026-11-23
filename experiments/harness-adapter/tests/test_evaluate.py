"""Evaluation: infrastructure failures are rerun, reports, shell share, multi-LoRA serving."""

from __future__ import annotations

import contextlib
import copy
import json

import pytest
from openenv.harbor.models import HarborRolloutResult, HarborTurn

from querymt_harbor.commands import vllm_command
from querymt_harbor.config import load_reference
from querymt_harbor.evaluate import (
    CAPACITY,
    CONNECTION,
    INFRA_LOG_FILE,
    INSTALL,
    SANDBOX,
    EvalTask,
    batches,
    classify_infrastructure,
    eval_concurrency,
    eval_path,
    evaluate_checkpoints,
    evaluate_model,
    evaluate_task,
    evaluation_steps,
    load_scores,
    shell_share,
    tool_counts,
)
from querymt_harbor.select import SWEBENCH, TERMINAL_BENCH

RAW = load_reference().raw

SWE_TASK = EvalTask(SWEBENCH, "swe-001", "swe-split", 0)
TB_TASK = EvalTask(TERMINAL_BENCH, "tb-001", "tb-split", 4)

OUTAGE = {
    "ok": False,
    "error": "ConnectError: connection refused to the capture endpoint",
    "n_turns": 0,
    "reward": None,
}


def graded(reward: float, *calls: str) -> dict:
    return {
        "ok": True,
        "reward": reward,
        "n_turns": 1,
        "turns": [{"tool_calls": [{"name": c} for c in calls]}],
    }


class Script:
    """A fake rollout runner returning scripted results per task, recording every request."""

    def __init__(self, *results):
        self.results = list(results)
        self.requests = []

    def __call__(self, request):
        self.requests.append(request)
        result = self.results.pop(0)
        if isinstance(result, BaseException):
            raise result
        return result


def test_tunnel_outage_is_rerun(tmp_path):
    runner = Script(OUTAGE, graded(1.0, "shell"))
    report = evaluate_model(
        runner, [TB_TASK], name="base", model="Qwen/Qwen3.8-27B", raw=RAW, run_dir=tmp_path
    )

    assert [r.attempt for r in runner.requests] == [1, 2]
    task = report["tasks"][TB_TASK.key]
    assert [a["infrastructure_failure"] for a in task["attempts"]] == [CONNECTION, None]
    # Only the rerun's grade is reported.
    assert report["scores"] == {TB_TASK.key: 1.0}
    assert load_scores(eval_path(tmp_path, "base")) == {TB_TASK.key: 1.0}
    # The failed attempt is in the report and in the rerun log, not in the scores.
    assert len(report["infrastructure_failures"]) == 1
    logged = [json.loads(line) for line in (tmp_path / "eval" / INFRA_LOG_FILE).open()]
    assert [(e["model"], e["task"], e["attempt"]) for e in logged] == [("base", TB_TASK.key, 1)]


def test_an_attempt_still_failing_after_the_reruns_is_ungraded_not_zero():
    runner = Script(OUTAGE, OUTAGE, OUTAGE, graded(1.0))
    outcome = evaluate_task(runner, TB_TASK, model="m", raw=RAW)
    assert len(runner.requests) == 3  # one attempt plus `infra_reruns = 2`
    assert outcome.score is None
    assert outcome.infrastructure_failed


def test_the_rerun_limit_comes_from_the_config():
    raw = copy.deepcopy(RAW)
    raw["eval"]["infra_reruns"] = 0
    runner = Script(OUTAGE, graded(1.0))
    assert evaluate_task(runner, TB_TASK, model="m", raw=raw).score is None
    assert len(runner.requests) == 1


def test_a_runner_that_raises_a_connection_error_is_rerun():
    runner = Script(ConnectionError("connection refused"), graded(0.0))
    outcome = evaluate_task(runner, TB_TASK, model="m", raw=RAW)
    assert outcome.score == 0.0
    assert [a.infrastructure for a in outcome.attempts] == [CONNECTION, None]


@pytest.mark.parametrize(
    ("error", "expected"),
    [
        ("sandbox failed to start: image build error", SANDBOX),
        ("failed to create the sandbox container", SANDBOX),
        ("agent install failed: upload of qmt-headless", INSTALL),
        ("Name or service not known", CONNECTION),
    ],
)
def test_infrastructure_failure_kinds(error, expected):
    assert classify_infrastructure({"ok": False, "error": error, "n_turns": 0}) == expected


def test_a_server_at_capacity_is_an_infrastructure_failure_and_is_rerun():
    # OpenEnv refuses (it does not queue) a rollout beyond MAX_CONCURRENT_ENVS.
    refused = {
        "ok": False,
        "error": "Server at capacity: 4/4 sessions active. Cannot accept new connections.",
        "n_turns": 0,
    }
    assert classify_infrastructure(refused) == CAPACITY
    assert classify_infrastructure({**refused, "error": "CAPACITY_REACHED"}) == CAPACITY
    runner = Script(refused, graded(1.0))
    outcome = evaluate_task(runner, TB_TASK, model="m", raw=RAW)
    assert outcome.score == 1.0
    assert [a.infrastructure for a in outcome.attempts] == [CAPACITY, None]


def test_evaluation_concurrency_defaults_to_the_config_and_cannot_exceed_the_server():
    assert eval_concurrency(RAW) == RAW["eval"]["concurrency"] == 16
    assert eval_concurrency(RAW, 4) == 4
    assert RAW["eval"]["concurrency"] <= RAW["serving"]["max_concurrent_rollouts"]
    with pytest.raises(ValueError, match="exceeds serving.max_concurrent_rollouts 64"):
        eval_concurrency(RAW, 65)
    with pytest.raises(ValueError, match="at least 1"):
        eval_concurrency(RAW, 0)


def test_checkpoint_evaluation_rejects_too_much_concurrency_before_serving(tmp_path):
    served = []

    @contextlib.contextmanager
    def serve(adapters):
        served.append(adapters)
        yield

    with pytest.raises(ValueError, match="exceeds"):
        evaluate_checkpoints(
            Script(),
            [TB_TASK],
            raw=RAW,
            run_dir=tmp_path,
            checkpoints_dir=tmp_path / "checkpoints",
            serve=serve,
            concurrency=500,
        )
    assert served == []


def test_a_model_failure_is_not_an_infrastructure_failure():
    # Turns were captured: the model ran, and its own commands may fail to connect all day.
    ran = {"ok": False, "error": "connection refused", "n_turns": 7, "reward": 0.0}
    assert classify_infrastructure(ran) is None
    # No endpoint, sandbox or install problem: the verifier crashed. Scored as ungraded, not rerun.
    runner = Script({"ok": False, "error": "verifier crashed", "n_turns": 3, "reward": None})
    outcome = evaluate_task(runner, TB_TASK, model="m", raw=RAW)
    assert len(runner.requests) == 1
    assert outcome.score is None
    assert not outcome.infrastructure_failed
    assert classify_infrastructure(graded(1.0)) is None


def test_requests_carry_the_evaluation_policy():
    runner = Script(graded(0.0), graded(0.0))
    evaluate_task(runner, SWE_TASK, model="qmt-adapter", raw=RAW)
    evaluate_task(runner, TB_TASK, model="qmt-adapter", raw=RAW)
    swe, tb = runner.requests
    assert (swe.step_limit, swe.timeout_sec) == (100, 7200)
    assert (tb.step_limit, tb.timeout_sec) == (100, 3600)
    assert dict(swe.sampling) == {"temperature": 0.8, "top_p": 1.0}
    assert (swe.model, swe.task.split, tb.task.index) == ("qmt-adapter", "swe-split", 4)


def test_shell_share_counts_only_the_reported_attempt():
    # The outage attempt's calls (none) do not count; the rerun made 3 shell and 1 edit calls.
    runner = Script(OUTAGE, graded(1.0, "shell", "shell", "edit", "shell"))
    report = evaluate_model(runner, [TB_TASK], name="base", model="m", raw=RAW)
    assert report["tool_calls"] == {"shell": 3, "edit": 1}
    assert report["shell_share"] == 0.75


def test_shell_share_over_all_tasks_and_without_tool_calls():
    runner = Script(graded(1.0, "shell"), graded(0.0, "read_tool", "read_tool", "shell"))
    report = evaluate_model(runner, [SWE_TASK, TB_TASK], name="base", model="m", raw=RAW)
    assert report["shell_share"] == pytest.approx(2 / 4)
    assert shell_share({}) is None


def test_tool_calls_are_read_from_the_real_result_model():
    result = HarborRolloutResult(
        reward=1.0,
        n_turns=2,
        turns=[
            HarborTurn(turn=0, tool_calls=[{"name": "shell", "arguments": "{}"}]),
            HarborTurn(
                turn=1,
                tool_calls=[
                    {"name": "shell", "arguments": "{}"},
                    {"name": "todowrite", "arguments": "{}"},
                ],
            ),
        ],
    )
    assert tool_counts(result) == {"shell": 2, "todowrite": 1}
    assert classify_infrastructure(result) is None


def test_checkpoints_are_those_at_the_configured_interval():
    available = range(10, 201, 10)
    assert evaluation_steps(available, first=80, every=20) == [80, 100, 120, 140, 160, 180, 200]
    assert evaluation_steps([70, 90, 100], first=80, every=20) == [100]
    assert batches([1, 2, 3, 4, 5], 2) == [[1, 2], [3, 4], [5]]


def make_checkpoint(directory, step):
    path = directory / f"checkpoint-{step}"
    path.mkdir(parents=True)
    (path / "trainer_state.json").write_text("{}")
    (path / "adapter_model.safetensors").write_text("x")
    return path


def test_checkpoints_are_served_in_batches_and_finished_ones_are_kept(tmp_path):
    raw = copy.deepcopy(RAW)
    raw["serving"]["max_loras"] = 2
    checkpoints = tmp_path / "checkpoints"
    for step in (60, 80, 100, 120, 140):
        make_checkpoint(checkpoints, step)
    (checkpoints / "checkpoint-160").mkdir()  # interrupted save: no weights, never evaluated
    eval_path(tmp_path, "checkpoint-80").parent.mkdir(parents=True)
    eval_path(tmp_path, "checkpoint-80").write_text(json.dumps({"scores": {}}))

    served = []

    @contextlib.contextmanager
    def serve(adapters):
        served.append(sorted(adapters))
        yield

    runner = Script(*[graded(1.0)] * 3)
    reports = evaluate_checkpoints(
        runner,
        [TB_TASK],
        raw=raw,
        run_dir=tmp_path,
        checkpoints_dir=checkpoints,
        serve=serve,
        concurrency=1,
    )

    assert served == [["checkpoint-100", "checkpoint-120"], ["checkpoint-140"]]
    assert sorted(reports) == ["checkpoint-100", "checkpoint-120", "checkpoint-140"]
    assert [r.model for r in runner.requests] == [
        "checkpoint-100",
        "checkpoint-120",
        "checkpoint-140",
    ]
    assert not eval_path(tmp_path, "checkpoint-60").exists()  # before the first checkpoint


def test_multi_lora_vllm_command_serves_all_candidates():
    adapters = {"checkpoint-100": "/run/checkpoints/checkpoint-100", "checkpoint-120": "/x/120"}
    command = vllm_command(RAW, trainer=False, adapters=adapters)
    argv = list(command.argv)
    assert argv[:3] == ["vllm", "serve", "Qwen/Qwen3.8-27B"]
    i = argv.index("--lora-modules")
    assert argv[i + 1 : i + 3] == [
        "checkpoint-100=/run/checkpoints/checkpoint-100",
        "checkpoint-120=/x/120",
    ]
    assert argv[argv.index("--max-lora-rank") + 1] == "32"
    assert argv[argv.index("--max-loras") + 1] == "18"
    # Evaluated as published: thinking off, no trainer-only machinery.
    assert argv[argv.index("--default-chat-template-kwargs") + 1] == '{"enable_thinking": false}'
    for trainer_only in ("--weight-transfer-config", "--return-tokens-as-token-ids"):
        assert trainer_only not in argv
    assert "VLLM_SERVER_DEV_MODE" not in command.env


def test_more_adapters_than_max_loras_are_refused():
    adapters = {f"checkpoint-{i}": f"/x/{i}" for i in range(19)}
    with pytest.raises(ValueError, match="max-loras 18"):
        vllm_command(RAW, trainer=False, adapters=adapters)
