"""The launcher: exact commands from the reference config, and process handling with stand-ins."""

from __future__ import annotations

import copy
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

import querymt_harbor.launcher as launcher
from querymt_harbor.commands import (
    Command,
    flatten_overrides,
    safeagents_command,
    server_command,
    trainer_command,
    vllm_command,
)
from querymt_harbor.config import load_reference
from querymt_harbor.launcher import (
    HealthSupervisor,
    LaunchError,
    LocalServices,
    ProcessGroup,
    default_spawn,
    eval_stack,
    hf_jobs_command,
    running,
    training_stack,
    wait_for_job,
)
from querymt_harbor.select import SWEBENCH, TERMINAL_BENCH

REFERENCE = load_reference()
RAW = REFERENCE.raw
RUN = Path("/data/runs/qmt-qwen38-27b")
BASE = "Qwen/Qwen3.8-27B"
VLLM = "http://127.0.0.1:8000"
SERVER = "http://127.0.0.1:8001"
GUARD = "http://127.0.0.1:8787"

VLLM_TRAINING_ARGV = (
    "vllm", "serve", BASE, "--port", "8000",
    "--enable-lora", "--max-lora-rank", "32", "--max-loras", "18",
    "--enable-auto-tool-choice", "--tool-call-parser", "qwen3_xml",
    "--reasoning-parser", "qwen3",
    "--default-chat-template-kwargs", '{"enable_thinking": false}',
    "--language-model-only", "--enable-prefix-caching", "--mamba-cache-mode", "align",
    "--max-model-len", "131072",
    "--logprobs-mode", "processed_logprobs", "--return-tokens-as-token-ids",
    "--weight-transfer-config", '{"backend":"nccl"}',
)  # fmt: skip


def test_launcher_builds_reference_commands():
    vllm = vllm_command(RAW, trainer=True)
    assert vllm.argv == VLLM_TRAINING_ARGV
    assert vllm.env == {
        "CUDA_VISIBLE_DEVICES": "0",
        "VLLM_ALLOW_RUNTIME_LORA_UPDATING": "1",
        "VLLM_SERVER_DEV_MODE": "1",
    }

    guard = safeagents_command(RAW, RUN)
    assert guard.argv == (
        sys.executable, "-m", "safeagents.cli",
        "--journal", str(RUN / "safeagents_calls.jsonl"),
        VLLM, "8787",
    )  # fmt: skip

    server = server_command(RAW, llm_url=VLLM, datasets=["/data/runs/qmt-qwen38-27b/data/split"])
    assert server.argv == (
        sys.executable, "-m", "querymt_harbor.serve",
        "--llm-url", VLLM, "--model", BASE,
        "--port", "8001", "--capture-port", "8100", "--expose", "gradio",
        "--dataset", "/data/runs/qmt-qwen38-27b/data/split",
    )  # fmt: skip
    # OpenEnv's default of 4 would refuse the fifth concurrent rollout.
    assert server.env == {"MAX_CONCURRENT_ENVS": "64"}

    trainer = trainer_command(RAW, RUN, split="/data/split")
    assert trainer.argv == (
        sys.executable, "-m", "querymt_harbor.train",
        "--run-dir", str(RUN), "--server", SERVER,
        "--vllm-url", VLLM, "--rollout-llm-url", GUARD, "--split", "/data/split",
    )  # fmt: skip
    assert trainer.env == {"CUDA_VISIBLE_DEVICES": "1"}


def test_the_training_stack_starts_vllm_server_guard_then_trainer():
    members = training_stack(RAW, RUN, split="/data/split")
    assert [m.name for m in members] == ["vllm", "harbor-server", "safeagents", "trainer"]
    assert members[0].command.argv == VLLM_TRAINING_ARGV
    assert members[-1].ready is None  # the trainer is the job, not a service


def test_without_the_guard_the_trainer_rolls_out_against_vllm_directly():
    raw = copy.deepcopy(RAW)
    raw["guard"]["enabled"] = False
    members = training_stack(raw, RUN, split="/data/split")
    assert [m.name for m in members] == ["vllm", "harbor-server", "trainer"]
    argv = members[-1].command.argv
    assert argv[argv.index("--rollout-llm-url") + 1] == VLLM


def test_pipeline_overrides_reach_the_trainer():
    overrides = {"optimizer": {"learning_rate": 1e-5}, "budget": {"max_steps": 20}}
    argv = trainer_command(
        RAW, RUN, split="s", config_path="/cfg/reference.toml", overrides=overrides
    ).argv
    assert argv[argv.index("--config") + 1] == "/cfg/reference.toml"
    assert [argv[i + 1] for i, a in enumerate(argv) if a == "--override"] == [
        "optimizer.learning_rate=1e-05",
        "budget.max_steps=20",
    ]
    assert flatten_overrides({"a": {"b": "x"}}) == ['a.b="x"']


def test_the_guard_command_really_starts_safeagents():
    # The argv test above cannot catch a module that is not runnable (`python -m safeagents`
    # fails: the package has no __main__), so run the command.
    argv = [*safeagents_command(RAW, RUN).argv[:3], "--help"]
    proc = subprocess.run(argv, capture_output=True, text=True, timeout=60)
    assert proc.returncode == 0, proc.stderr
    assert "--journal" in proc.stdout


def test_the_eval_stack_serves_candidates_and_both_benchmarks_without_the_guard():
    roots = ["/run/downloads/swe@main", "/run/downloads/tb@main"]
    members = eval_stack(RAW, {"checkpoint-80": "/run/checkpoints/checkpoint-80"}, roots)
    assert [m.name for m in members] == ["vllm", "harbor-server"]
    vllm = list(members[0].command.argv)
    assert "--lora-modules" in vllm
    assert "--weight-transfer-config" not in vllm
    server = members[1].command.argv
    datasets = [server[i + 1] for i, a in enumerate(server) if a == "--dataset"]
    # Local directories, never Hub ids: the server would download those itself, unpinned.
    assert datasets == roots
    assert members[1].command.env == {"MAX_CONCURRENT_ENVS": "64"}


def test_hf_jobs_command_mounts_the_bucket_at_data():
    argv = hf_jobs_command(
        RAW, bucket="org/qmt-runs", script="scripts/hf_job.py", revision="abc123"
    )
    assert argv == [
        "hf", "jobs", "uv", "run", "--flavor", "h200x2", "--timeout", "48h", "--detach",
        "--env", "QMT_REPO_REV=abc123",
        "--secrets", "HF_TOKEN", "--secrets", "E2B_API_KEY",
        "--volume", "hf://buckets/org/qmt-runs:/data",
        "scripts/hf_job.py", "run", "--run-dir", "/data/runs/qmt-qwen38-27b",
    ]  # fmt: skip


# --- process handling ---------------------------------------------------------------------------


def sh(script: str, **env: str) -> Command:
    return Command(("/bin/sh", "-c", script), env)


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def wait_for_file(path: Path, timeout: float = 5.0) -> str:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.is_file() and path.read_text().strip():
            return path.read_text().strip()
        time.sleep(0.02)
    raise AssertionError(f"{path} was not written")


def test_stopping_a_group_kills_the_whole_process_group(tmp_path):
    pidfile = tmp_path / "child.pid"
    with ProcessGroup(tmp_path / "logs") as group:
        group.start("vllm", sh(f"sleep 300 & echo $! > {pidfile}; wait"))
        child = int(wait_for_file(pidfile))
        assert alive(child)
    # vLLM forks workers: they must not outlive the launcher.
    time.sleep(0.2)
    assert not alive(child)


def test_a_process_that_ignores_sigterm_is_killed(tmp_path):
    marker = tmp_path / "started"
    group = ProcessGroup(tmp_path / "logs")
    managed = group.start(
        "stubborn", sh(f"trap '' TERM; touch {marker}; while :; do sleep 1; done")
    )
    wait_for_file_exists = time.monotonic() + 5
    while not marker.exists() and time.monotonic() < wait_for_file_exists:
        time.sleep(0.02)
    started = time.monotonic()
    group.stop_all(grace=0.3)
    assert managed.process.poll() is not None
    assert time.monotonic() - started < 5


def test_a_process_that_dies_before_it_is_healthy_reports_its_log(tmp_path):
    group = ProcessGroup(tmp_path / "logs")
    managed = group.start("vllm", sh("echo 'CUDA out of memory'; exit 3"))
    with pytest.raises(LaunchError, match=r"(?s)vllm exited with code 3.*CUDA out of memory") as e:
        group.wait_healthy(managed, lambda: False, timeout=10, interval=0.05)
    assert "vllm.log" in str(e.value)
    group.stop_all()


def test_a_process_that_never_gets_healthy_times_out(tmp_path):
    ticks = iter(range(100))
    with ProcessGroup(tmp_path / "logs") as group:
        managed = group.start("server", sh("sleep 300"))
        with pytest.raises(LaunchError, match="server was not healthy after 5 s"):
            group.wait_healthy(
                managed,
                lambda: False,
                timeout=5,
                sleep=lambda _s: None,
                clock=lambda: float(next(ticks)),
            )


def test_a_failing_member_stops_the_ones_started_before_it(tmp_path):
    pidfile = tmp_path / "vllm.pid"
    members = [
        launcher.Member("vllm", sh(f"echo $$ > {pidfile}; sleep 300"), lambda: True),
        launcher.Member("harbor-server", sh("echo no port; exit 1"), lambda: False, 10),
    ]
    with pytest.raises(LaunchError, match="harbor-server exited with code 1"):
        with running(members, tmp_path / "logs"):
            pytest.fail("the stack must not come up")
    time.sleep(0.2)
    assert not alive(int(pidfile.read_text()))


def test_the_supervisor_trips_after_consecutive_failures():
    with HealthSupervisor(lambda: False, interval=0.01, max_failures=3) as supervisor:
        assert supervisor.failed.wait(timeout=5)
    assert "stopped answering" in supervisor.message()


def test_the_supervisor_forgives_isolated_failures():
    answers = iter([False, True] * 50)
    with HealthSupervisor(lambda: next(answers, True), interval=0.005, max_failures=2) as s:
        time.sleep(0.2)
        assert not s.failed.is_set()


def test_wait_for_job_returns_the_exit_code_and_notices_a_dead_service(tmp_path):
    with ProcessGroup(tmp_path / "logs") as group:
        group.start("vllm", sh("sleep 300"))
        group.start("trainer", sh("exit 4"))
        assert wait_for_job(group, "trainer", sleep=lambda _s: None) == 4

    with ProcessGroup(tmp_path / "logs2") as group:
        group.start("vllm", sh("echo oom; exit 9"))
        group.start("trainer", sh("sleep 300"))
        with pytest.raises(LaunchError, match="vllm exited with code 9 while trainer"):
            wait_for_job(group, "trainer", interval=0.05)


def test_wait_for_job_stops_the_run_when_the_tunnel_is_down(tmp_path):
    with ProcessGroup(tmp_path / "logs") as group:
        group.start("trainer", sh("sleep 300"))
        with HealthSupervisor(lambda: False, interval=0.01, max_failures=2) as supervisor:
            with pytest.raises(LaunchError, match="stopped answering"):
                wait_for_job(group, "trainer", supervisor, interval=0.05)


# --- LocalServices ------------------------------------------------------------------------------


BENCHMARK_TASKS = {
    launcher.BENCHMARK_REPOS[SWEBENCH]: ["b-task", "a-task"],
    launcher.BENCHMARK_REPOS[TERMINAL_BENCH]: ["z", "m", "c"],
}


def make_benchmarks(base: Path) -> dict[str, Path]:
    """Two benchmark roots laid out as downloaded (`tasks/<name>/task.toml`), by repo id."""
    roots = {}
    for repo, names in BENCHMARK_TASKS.items():
        root = base / repo.replace("/", "__")
        for name in names:
            (root / "tasks" / name).mkdir(parents=True)
            (root / "tasks" / name / "task.toml").write_text("")
            (root / "tasks" / name / "instruction.md").write_text(f"do {name}")
        roots[repo] = root
    return roots


class CountClient:
    """A Harbor client that answers `num_tasks` from a table, by split."""

    def __init__(self, counts: dict[str, int]):
        self.counts = counts

    def num_tasks(self, split):
        return self.counts[split]

    def close(self):
        pass


@pytest.fixture
def local(tmp_path, monkeypatch):
    """LocalServices whose stack members are stand-in processes that are always healthy."""
    monkeypatch.setattr(launcher, "http_ok", lambda *_a, **_k: True)
    monkeypatch.setattr(launcher, "tcp_open", lambda *_a, **_k: True)
    started: list[str] = []
    scripts = {"trainer": "exit 0"}
    roots = make_benchmarks(tmp_path / "benchmarks")

    def spawn(name, command, log):
        started.append(name)
        standin = sh(scripts.get(name, "sleep 300"), **command.env)
        return default_spawn(name, standin, log)

    services = LocalServices(
        raw=RAW,
        reference=REFERENCE,
        run_dir=tmp_path / "run",
        spawn=spawn,
        sleep=lambda _s: time.sleep(0.02),
        downloader=lambda repo, revision: roots[repo],
    )
    services.ensure_split = lambda: tmp_path / "split"  # type: ignore[method-assign]
    services.client_factory = lambda url: CountClient(
        {str(roots[repo]): len(names) for repo, names in BENCHMARK_TASKS.items()}
    )
    return services, started, scripts


def test_train_runs_the_stack_and_waits_for_the_trainer(local):
    services, started, _scripts = local
    services.train({"run": {"dir": str(services.run_dir)}})
    assert started == ["vllm", "harbor-server", "safeagents", "trainer"]


def test_a_failing_trainer_is_reported_with_its_log(local):
    services, _started, scripts = local
    scripts["trainer"] = "echo 'Traceback: reward blew up'; exit 3"
    with pytest.raises(LaunchError, match=r"(?s)trainer exited with code 3.*reward blew up"):
        services.train({"run": {"dir": str(services.run_dir)}})


class FakeClient:
    def __init__(self):
        self.calls = []
        self.closed = False

    def run_rollout(self, **kwargs):
        self.calls.append(kwargs)
        return {"reward": 1.0, "n_turns": 3}

    def close(self):
        self.closed = True


def test_the_qualification_rollout_goes_through_the_guard(local):
    services, started, _scripts = local
    client = FakeClient()
    services.client_factory = lambda url: (setattr(client, "url", url), client)[1]

    result = services.qualification_rollout(services.run_dir)

    assert result == {"reward": 1.0, "n_turns": 3}
    assert started == ["vllm", "harbor-server", "safeagents"]  # no trainer
    (call,) = client.calls
    assert client.url == SERVER and client.closed
    assert call["llm_url"] == GUARD
    assert call["harness"] == "querymt"
    assert (call["agent_step_limit"], call["agent_timeout_sec"]) == (50, 1800.0)
    assert call["sampling"] == {"temperature": 0.8, "top_p": 1.0, "top_k": -1}
    assert call["reward_key"] == "correctness,reward"


def test_serve_eval_starts_and_stops_the_eval_stack(local):
    services, started, _scripts = local
    with services.serve_eval({"checkpoint-80": Path("/x/80")}) as group:
        assert [m.name for m in group.processes] == ["vllm", "harbor-server"]
        procs = [m.process for m in group.processes]
        # The server was started over the local, pinned downloads and not over Hub ids.
        argv = group.processes[1].command.argv
        served = [argv[i + 1] for i, a in enumerate(argv) if a == "--dataset"]
        assert served == [str(p) for p in services.eval_roots().values()]
    assert started == ["vllm", "harbor-server"]
    assert all(p.poll() is not None for p in procs)


def test_a_server_listing_other_tasks_than_the_scores_use_stops_the_evaluation(local):
    services, started, _scripts = local
    services.client_factory = lambda url: CountClient(
        {str(root): 5 for root in services.eval_roots().values()}
    )
    with pytest.raises(LaunchError, match="serves 5 tasks .* but 2 were expected"):
        with services.serve_eval({}):
            pytest.fail("no rollout may run against a mismatched server")


def test_the_benchmarks_are_downloaded_once_at_their_pinned_revisions(tmp_path):
    roots = make_benchmarks(tmp_path / "benchmarks")
    seen = []

    def downloader(repo, revision):
        seen.append((repo, revision))
        return roots[repo]

    services = LocalServices(
        raw=RAW, reference=REFERENCE, run_dir=tmp_path / "run", downloader=downloader
    )
    services.eval_tasks()
    services.eval_tasks()
    services.eval_roots()
    pins = {b["repo"]: b["revision"] for b in RAW["data"]["benchmarks"]}
    assert sorted(seen) == sorted(pins.items())  # each once, at its configured revision


def test_eval_tasks_are_indexed_in_the_order_the_server_lists_them(tmp_path):
    roots = make_benchmarks(tmp_path / "benchmarks")

    services = LocalServices(
        raw=RAW,
        reference=REFERENCE,
        run_dir=tmp_path / "run",
        downloader=lambda repo, revision: roots[repo],
    )
    tasks = services.eval_tasks()
    assert [(t.task_id, t.index) for t in tasks[SWEBENCH]] == [("a-task", 0), ("b-task", 1)]
    assert [(t.task_id, t.index) for t in tasks[TERMINAL_BENCH]] == [("c", 0), ("m", 1), ("z", 2)]
    # The split is the dataset spec the server is started with: the local root.
    tb_root = roots[launcher.BENCHMARK_REPOS[TERMINAL_BENCH]]
    assert tasks[TERMINAL_BENCH][0].split == str(tb_root)


def test_constructing_services_starts_nothing(tmp_path):
    def spawn(name, command, log):
        raise AssertionError(f"{name} was started")

    services = LocalServices(raw=RAW, reference=REFERENCE, run_dir=tmp_path, spawn=spawn)
    services.eval_runner()  # building a runner is not starting a service either
