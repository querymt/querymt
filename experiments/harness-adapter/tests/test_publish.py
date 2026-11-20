"""Publishing (harness-adapter-training / Published adapter bundle, Adapter served through the
trained profile)."""

from __future__ import annotations

import dataclasses
import json
import shutil
import subprocess
from pathlib import Path

import pytest

from headless_run import qmt_env, run_headless
from querymt_harbor.config import DEFAULT_CONFIG, load_reference
from querymt_harbor.publish import (
    CANARY,
    ProfileDrift,
    ScrubError,
    build_bundle,
    check_harness_unchanged,
    gguf_argv,
    hub_uploader,
    publish,
    publish_trajectories,
)
from querymt_harbor.select import declare_selection_set, select_and_report
from querymt_harbor.train import resolve, write_config

REPO_ID = "qmt/qwen-qmt-adapter"
FIFTEEN_TOOLS = {
    "edit",
    "read_tool",
    "write_file",
    "multiedit",
    "replace_symbol",
    "glob",
    "search_text",
    "ls",
    "index",
    "get_symbol",
    "get_function",
    "find_symbol_references",
    "shell",
    "todowrite",
    "todoread",
}


@pytest.fixture
def reference():
    return load_reference(DEFAULT_CONFIG)


@pytest.fixture
def run_dir(tmp_path, reference):
    """A finished run: training config, selection set, selection and report."""
    run = tmp_path / "run"
    write_config(run, resolve(reference, run))
    swe = [f"swe-{i}" for i in range(10)]
    tb = [f"tb-{i}" for i in range(30)]
    selection = declare_selection_set(run, swe, tb, seed=0, subset_size=18)
    base = dict.fromkeys(selection.task_ids + selection.held_out, 0.0)
    adapter = {**base, **dict.fromkeys(selection.task_ids[:10] + selection.held_out[:3], 1.0)}
    select_and_report(run, base, {"checkpoint-160": adapter})
    return run


@pytest.fixture
def checkpoint(tmp_path):
    path = tmp_path / "checkpoint-160"
    path.mkdir()
    (path / "adapter_model.safetensors").write_bytes(b"lora-weights")
    (path / "adapter_config.json").write_text('{"r": 32}')
    return path


@pytest.fixture
def tokenizer_dir(tmp_path):
    path = tmp_path / "tokenizer"
    path.mkdir()
    (path / "tokenizer.json").write_text("{}")
    (path / "tokenizer_config.json").write_text("{}")
    return path


def build(tmp_path, run_dir, checkpoint, reference, tokenizer_dir, **kwargs):
    return build_bundle(
        tmp_path / "bundle",
        run_dir=run_dir,
        checkpoint=checkpoint,
        reference=reference,
        repo_id=REPO_ID,
        tokenizer_dir=tokenizer_dir,
        **kwargs,
    )


def test_bundle_layout(tmp_path, run_dir, checkpoint, reference, tokenizer_dir):
    bundle = build(tmp_path, run_dir, checkpoint, reference, tokenizer_dir)

    files = {str(p.relative_to(bundle)) for p in bundle.rglob("*") if p.is_file()}
    expected = {
        "adapter_model.safetensors",
        "adapter_config.json",
        "tokenizer.json",
        "tokenizer_config.json",
        "querymt/harness_adapter.toml",
        "training_config.json",
        "selection.json",
        "eval_report.json",
        "README.md",
    } | {f"querymt/prompts/{p.name}" for p in reference.prompts}
    assert files == expected


def test_bundled_profile_differs_only_in_prompt_paths(
    tmp_path, run_dir, checkpoint, reference, tokenizer_dir
):
    bundle = build(tmp_path, run_dir, checkpoint, reference, tokenizer_dir)
    original = reference.profile.read_text()
    published = (bundle / "querymt/harness_adapter.toml").read_text()
    assert published == original.replace('"../prompts/', '"prompts/')
    assert "../prompts/" not in published


def test_profile_drift_blocks_publishing(tmp_path, run_dir, checkpoint, reference, tokenizer_dir):
    edited = tmp_path / "edited"
    shutil.copytree(reference.profile.parent.parent, edited, dirs_exist_ok=True)
    profile = edited / "confs" / reference.profile.name
    profile.write_text(profile.read_text() + "\n# edited after training\n")
    drifted = dataclasses.replace(
        reference,
        profile=profile,
        prompts=tuple(edited / "prompts" / p.name for p in reference.prompts),
    )
    recorded = json.loads((run_dir / "training_config.json").read_text())

    with pytest.raises(ProfileDrift) as excinfo:
        check_harness_unchanged(recorded, drifted)

    message = str(excinfo.value)
    assert recorded["provenance"]["profile_sha256"] in message
    assert reference.profile.name in message
    with pytest.raises(ProfileDrift):
        build(tmp_path, run_dir, checkpoint, drifted, tokenizer_dir)
    assert not (tmp_path / "bundle").exists(), "nothing is built for a drifted harness"


def test_a_changed_prompt_blocks_publishing(tmp_path, run_dir, reference):
    edited = tmp_path / "edited"
    shutil.copytree(reference.profile.parent.parent, edited, dirs_exist_ok=True)
    prompt = edited / "prompts" / reference.prompts[0].name
    prompt.write_text(prompt.read_text() + "changed")
    drifted = dataclasses.replace(
        reference,
        profile=edited / "confs" / reference.profile.name,
        prompts=tuple(edited / "prompts" / p.name for p in reference.prompts),
    )
    recorded = json.loads((run_dir / "training_config.json").read_text())
    with pytest.raises(ProfileDrift, match=f"prompt {prompt.name}"):
        check_harness_unchanged(recorded, drifted)


def test_unchanged_harness_passes(run_dir, reference):
    check_harness_unchanged(json.loads((run_dir / "training_config.json").read_text()), reference)


def test_missing_adapter_or_tokenizer_is_reported(tmp_path, run_dir, reference, checkpoint):
    with pytest.raises(FileNotFoundError, match="no tokenizer"):
        build(tmp_path, run_dir, checkpoint, reference, None)
    (checkpoint / "adapter_model.safetensors").unlink()
    with pytest.raises(FileNotFoundError, match="not a PEFT adapter"):
        build(tmp_path, run_dir, checkpoint, reference, None)


def test_model_card_serving_commands(tmp_path, run_dir, checkpoint, reference, tokenizer_dir):
    bundle = build(tmp_path, run_dir, checkpoint, reference, tokenizer_dir)
    card = (bundle / "README.md").read_text()
    config = json.loads((run_dir / "training_config.json").read_text())

    # vLLM (design D7): the inference subset of the training flags, and nothing trainer-only.
    for flag in (
        "--enable-lora",
        f"--max-lora-rank {config['lora']['r']}",
        f"--lora-modules {config['model']['served_name']}={REPO_ID}",
        "--tool-call-parser qwen3_xml",
        "--reasoning-parser qwen3",
        "--language-model-only",
        "--enable-prefix-caching",
        "--mamba-cache-mode align",
        "--max-model-len 131072",
    ):
        assert flag in card, flag
    assert '{"enable_thinking": false}' in card
    for trainer_only in (
        "--weight-transfer-config",
        "--return-tokens-as-token-ids",
        "--logprobs-mode",
        "VLLM_SERVER_DEV_MODE",
    ):
        assert trainer_only not in card, trainer_only

    # llama.cpp: llama-server, with the converted adapter.
    assert "llama-server" in card and "--lora qmt-adapter.gguf" in card
    assert "llama serve" not in card

    # The harness is qmt-headless; nothing points at another binary.
    assert "qmt-headless run querymt/harness_adapter.toml" in card
    assert "qmtcode" not in card
    for variable in ("QMT_BASE_URL", "QMT_MODEL=qmt-adapter", "QMT_API_KEY"):
        assert variable in card
    assert config["provenance"]["profile_sha256"] in card
    assert config["provenance"]["headless"]["version"] in card


def test_model_card_flags_exist_in_llama_server():
    """The llama.cpp flags the card uses are real, when a llama-server is installed."""
    llama = shutil.which("llama-server")
    if llama is None:
        pytest.skip("llama-server is not installed")
    help_text = subprocess.run([llama, "--help"], capture_output=True, text=True).stdout
    for flag in ("--lora", "--jinja", "--alias"):
        assert flag in help_text, flag


def test_card_reports_the_recorded_results(tmp_path, run_dir, checkpoint, reference, tokenizer_dir):
    card = (
        build(tmp_path, run_dir, checkpoint, reference, tokenizer_dir) / "README.md"
    ).read_text()
    report = json.loads((run_dir / "eval_report.json").read_text())
    assert "checkpoint-160" in card
    assert f"{report['held_out_terminal_bench_vs_base']['mean_diff_points']:+.1f}" in card
    assert "held-out Terminal-Bench 2.1" in card


# --- 5.5: serving through the trained profile -------------------------------------------------


def test_adapter_served_through_the_trained_profile(
    tmp_path,
    run_dir,
    checkpoint,
    reference,
    tokenizer_dir,
    headless_bin,
    qmt_home,
    fake_chat_server,
):
    """`qmt-headless run` on the bundle's own profile: model name, tools, no thinking."""
    bundle = build(tmp_path, run_dir, checkpoint, reference, tokenizer_dir)
    fake_chat_server.script([{"text": "done"}])

    result = run_headless(
        headless_bin,
        bundle / "querymt" / "harness_adapter.toml",
        tmp_path / "serve",
        env=qmt_env(qmt_home, fake_chat_server.base_url, model="qmt-adapter", key="k"),
    )

    assert result.run_returncode == 0, result.stderr[-1500:]
    assert result.summary["outcome"] == "end_turn", result.summary
    body = fake_chat_server.requests[0].body
    assert body["model"] == "qmt-adapter"
    assert {t["function"]["name"] for t in body["tools"]} == FIFTEEN_TOOLS
    flat = json.dumps(body).lower()
    for thinking in ("enable_thinking", "chat_template_kwargs", "reasoning_effort", '"thinking"'):
        assert thinking not in flat, thinking
    # The bundled prompts were found next to the profile (the system prompt reached the wire).
    system = next(m for m in body["messages"] if m["role"] == "system")
    content = system["content"]
    text = content if isinstance(content, str) else " ".join(p.get("text", "") for p in content)
    assert text.strip()


# --- trajectories ---------------------------------------------------------------------------


SECRET = "sk-live-0123456789abcdef0123456789abcdef"


def trajectory_dir(tmp_path: Path) -> Path:
    source = tmp_path / "trajectories-in"
    source.mkdir()
    (source / "t1.json").write_text(
        json.dumps(
            {
                "steps": [
                    {"message": f"export OPENAI_API_KEY={SECRET} and cd /Users/alice/project"},
                    {"message": "mail alice.smith@acme-corp.io"},
                ]
            }
        )
    )
    return source


def test_trajectories_are_scrubbed_and_marked_as_evaluation_data(tmp_path):
    source = trajectory_dir(tmp_path)
    original = (source / "t1.json").read_text()

    published = publish_trajectories(
        [source], tmp_path / "bundle" / "trajectories", redact=[SECRET]
    )

    assert [p.name for p in published] == ["t1.json"]
    text = published[0].read_text()
    assert (
        SECRET not in text and "alice.smith@acme-corp.io" not in text and "/Users/alice" not in text
    )
    data = json.loads(text)
    assert data["canary"] == CANARY and "DO NOT USE FOR TRAINING" in data["canary"]
    assert (source / "t1.json").read_text() == original, "the originals are untouched"
    assert not list((tmp_path / "bundle").glob(".scrub-*")), "staging is cleaned up"


def test_a_single_file_source_is_published_flat(tmp_path):
    source = trajectory_dir(tmp_path) / "t1.json"

    (published,) = publish_trajectories([source], tmp_path / "out", redact=[SECRET])

    assert published == tmp_path / "out" / "t1.json"
    assert SECRET not in published.read_text()


def test_a_value_that_survives_scrubbing_is_refused(tmp_path, monkeypatch):
    source = trajectory_dir(tmp_path)
    # A scrubber that leaves everything alone must be caught by the verification pass.
    monkeypatch.setattr(
        "querymt_harbor.publish._scrub",
        lambda args: (
            subprocess.CompletedProcess(
                args, 0, stdout="0 strings to redact in 1 files\n", stderr=""
            )
            if "--out" not in args
            else _copy_unredacted(args)
        ),
    )
    with pytest.raises(ScrubError, match="still contains a value"):
        publish_trajectories([source], tmp_path / "out", redact=[SECRET])
    assert not (tmp_path / "out").exists(), "nothing unredacted is published"


def _copy_unredacted(args):
    # Like the real tool: the copy lands under the source directory's own name.
    src = Path(args[-1])
    out = Path(args[args.index("--out") + 1]) / src.name
    out.mkdir(parents=True)
    for path in src.iterdir():
        shutil.copy2(path, out / path.name)
    return subprocess.CompletedProcess(args, 1, stdout="", stderr="")


def test_bundle_includes_scrubbed_trajectories(
    tmp_path, run_dir, checkpoint, reference, tokenizer_dir
):
    bundle = build(
        tmp_path,
        run_dir,
        checkpoint,
        reference,
        tokenizer_dir,
        trajectories=[trajectory_dir(tmp_path)],
        redact=[SECRET],
    )
    published = (bundle / "trajectories" / "t1.json").read_text()
    assert SECRET not in published and CANARY in published


# --- upload ---------------------------------------------------------------------------------


def test_upload_goes_through_the_injected_uploader(
    tmp_path, run_dir, checkpoint, reference, tokenizer_dir
):
    bundle = build(tmp_path, run_dir, checkpoint, reference, tokenizer_dir)
    calls = []

    publish(bundle, REPO_ID, lambda path, repo: calls.append((path, repo)) or "ok")

    assert calls == [(bundle, REPO_ID)]


def test_hub_uploader_creates_the_repo_then_uploads_the_folder(tmp_path, monkeypatch):
    calls = []

    class FakeApi:
        def __init__(self, token=None):
            calls.append(("token", token))

        def create_repo(self, repo_id, **kwargs):
            calls.append(("create_repo", repo_id, kwargs))

        def upload_folder(self, **kwargs):
            calls.append(("upload_folder", kwargs))
            return "commit"

    monkeypatch.setattr("huggingface_hub.HfApi", FakeApi)

    assert hub_uploader("tok", private=True)(tmp_path, REPO_ID) == "commit"

    assert calls[0] == ("token", "tok")
    assert calls[1] == (
        "create_repo",
        REPO_ID,
        {"repo_type": "model", "private": True, "exist_ok": True},
    )
    assert calls[2][1]["folder_path"] == str(tmp_path) and calls[2][1]["repo_id"] == REPO_ID


def test_gguf_conversion_command(tmp_path):
    argv = gguf_argv(
        tmp_path / "convert_lora_to_gguf.py",
        tmp_path / "adapter",
        tmp_path / "base",
        tmp_path / "a.gguf",
    )
    assert argv[1:] == [
        str(tmp_path / "convert_lora_to_gguf.py"),
        "--base",
        str(tmp_path / "base"),
        "--outfile",
        str(tmp_path / "a.gguf"),
        str(tmp_path / "adapter"),
    ]
