# QueryMT harness adapter experiment

Trains, evaluates and publishes a LoRA adapter for the QueryMT harness with TRL
AsyncGRPO, Harbor tasks and OpenEnv. The harness is `qmt-headless`: one session, one
instruction, the result written to a session database and exported as ATIF. The OpenSpec
change `add-querymt-harness-adapter-experiment` holds the design and requirements.

Run everything from this directory with `uv run ...`. If `RUST_LOG` is set in your
shell, unset it (`env -u RUST_LOG uv run ...`) to keep test output readable.

## Runbook

### Prerequisites and costs

| Needed | For | Checked by preflight |
| --- | --- | --- |
| 2 GPUs (the reference is one `h200x2` HF Jobs flavor): vLLM on GPU 0, the trainer on GPU 1 | training, evaluation | `nvidia-smi --list-gpus` count >= `preflight.min_gpus` |
| A sandbox provider credential: `E2B_API_KEY` for `QMT_SANDBOX=e2b` (default), `MODAL_TOKEN_ID` and `MODAL_TOKEN_SECRET` for `modal`, `DAYTONA_API_KEY` for `daytona`, only `HF_TOKEN` for `hf-sandbox` (the reference run's sandboxes; they need a prebuilt `docker_image` in each task's `task.toml`), none for `docker` | every rollout | yes |
| `HF_TOKEN` (read the datasets, write the model repository) | data, publish | yes, and read access to each dataset revision |
| Pinned `qmt-headless` archives and the provider plugin | the sandbox install | yes: both are fetched and their SHA-256 checked |
| `preflight.min_free_disk_gb` (200) free on the run directory | the model, every checkpoint, trajectories | yes |

Costs are GPU hours on the two GPUs plus sandbox time: a rollout can run up to 30 minutes in
training and 1 to 2 hours in evaluation, and the baseline plus every evaluated checkpoint
runs all of SWE-bench Pro hard51 and Terminal-Bench 2.1. The reference run stopped at step
191; the plan targets 1,000 steps, with every checkpoint kept. See `hf jobs hardware` for the
current price of a flavor, and use `--override budget.max_steps=N` to bound a run.

### Configuration

`configs/reference.toml` is the only place recipe values live; every module reads it. Copy it
and point `QMT_HARNESS_CONFIG` (or `--config`) at the copy to change a pin, and use
`--override section.key=JSON` (repeatable) for a single value, for example
`--override optimizer.learning_rate=1e-5`. A resumed run refuses a changed configuration
(`training_config.json` records the one it started with). `configs/local.toml` (not committed,
from `scripts/package-headless.sh`) pins locally built `qmt-headless` archives.

### Running

```
python -m querymt_harbor.pipeline run --run-dir runs/my-run          # all stages
python -m querymt_harbor.pipeline run --run-dir runs/my-run --stage preflight
python -m querymt_harbor.pipeline status --run-dir runs/my-run
python -m querymt_harbor.pipeline run --stage publish --repo-id <org>/<model>
```

On HF Jobs the same command runs with a bucket mounted at `/data`
(`launcher.hf_jobs_command` builds it; `scripts/hf_job.py` checks out `QMT_REPO_REV`, runs
`uv sync --extra gpu` and then the pipeline):

```
hf jobs uv run --flavor h200x2 --timeout 48h --detach --env QMT_REPO_REV=<commit> \
  --secrets HF_TOKEN --secrets E2B_API_KEY --volume hf://buckets/<org>/<bucket>:/data \
  scripts/hf_job.py run --run-dir /data/runs/qmt-qwen38-27b
```

### Stages

Each finished stage writes `stages/<name>.done.json` (outputs and time) in the run directory.
A stage that fails writes none. Services (vLLM, the Harbor server, the guard) are started by a
stage and stopped when it ends; their logs are in `logs/<name>.log`.

| Stage | Inputs | Outputs | Fails with |
| --- | --- | --- | --- |
| `preflight` | the config, the environment, the machine | the marker: GPUs, credential names (never values), pinned revisions | `preflight failed:` and one line per problem, all at once. Nothing is started. |
| `qualify` | the training split (built under `data/split`, `dataset`) | `qualify.json` | `qualification failed: ...`: no reward, no turns, `roots != 1` (history rewritten), no trainable tokens (behind safeagents: set `guard.enabled=false`, design D6), or `atif != match` |
| `baseline` | the base model, hard51 and Terminal-Bench 2.1 | `selection_set.json` (declared first), `eval/base.json` | a service that does not come up, with the last lines of its log |
| `train` | a passing `qualify.json` | `training_config.json`, `checkpoints/checkpoint-N/`, `guard_blocked.log` | `no qualify.json` / `qualification did not pass`; `the trainer exited with code N` with its log; `the Harbor server stopped answering` when the tunnel or capture proxy dies |
| `evaluate` | complete checkpoints from step 80, every 20 | `eval/checkpoint-N.json`, `eval/infra_reruns.jsonl` | as `baseline` |
| `select` | `eval/*.json`, `selection_set.json` | `selection.json`, `eval_report.json` | `run the baseline stage first` / `run the evaluate stage first` |
| `publish` | `selection.json`, the checkpoint, the profile | the bundle in `bundle/`, uploaded to `publish.repo_id` | `no Hub repository`; `ProfileDrift` naming both hashes when the profile or a prompt changed since training |

### Resume

Run the same command again with the same run directory. Completed stages are skipped and the
first incomplete one starts. `train` resumes from the latest complete checkpoint (it never
repeats completed steps), and `evaluate` keeps every `eval/checkpoint-N.json` that exists and
evaluates only the rest. Evaluation of a model repeats a task up to twice when the attempt
failed for infrastructure reasons (zero turns plus a connection, sandbox-start or install
error); only the last attempt is reported and a task still failing is ungraded, not zero.
`--stage NAME --force` reruns one finished stage.

### Smoke check (no GPU, no vLLM)

The same live test as task 3.8 runs the real harness in a Docker sandbox against a local
llama-server. It needs Docker and `llama-server` with a small tool-capable GGUF; it checks the
reward, `turns > 0`, one root and `atif=match`, and the agent-timeout path.

```
scripts/package-headless.sh                                   # configs/local.toml
llama-server -hf unsloth/Qwen3-4B-Instruct-2507-GGUF:Q6_K_XL --jinja -c 32768 --alias qwen3-4b --port 8080 &
env -u RUST_LOG QMT_RUN_LIVE_HARBOR=1 QMT_HARNESS_CONFIG=configs/local.toml \
  QMT_LIVE_LLM_URL=http://127.0.0.1:8080 QMT_LIVE_MODEL=qwen3-4b \
  QMT_LIVE_DATASET=harborframework/terminal-bench-2.1 uv run pytest tests/test_harbor_live.py
```

`QMT_LIVE_TIER=vllm` (with a vLLM endpoint) also requires trainable tokens; that is the
qualification of task 7.2.

### Known limitations

- **Token capture behind the guard.** If safeagents hides token ids or logprobs from the
  capture proxy, qualification fails with "no trainable tokens"; the fallback is
  `guard.enabled=false`, which trains without the block-and-zero rule.
- **Long rollouts do not train.** `micro_batch_max_tokens` (65,536, TRL's `token_budget`) drops
  a longer sample with a warning, while vLLM's `max_model_len` is 131,072.
- **The guard sees only the model's output.** A rollout is zeroed when a tool call was blocked,
  never because of a tool result.
- **Evaluation runs without the guard**, as the published adapter is served without it.
- **Evaluation tasks are numbered from the local download the server serves.** Each benchmark
  is downloaded once at its `[data].benchmarks` revision under `downloads/`, the server is
  started over that directory (not a Hub id, which it would download unpinned), and after it
  starts the task counts are compared with the ones the scores are filed under. Pin the
  revisions to commit SHAs for a reproducible run: `main` is a moving target.
- **The tunnel supervisor checks the Harbor server and its capture port locally**; it cannot
  see the public tunnel itself, only the failures it causes.
- **The efficiency bonus can shift the tool mix** toward large `shell` calls. The evaluation
  reports the share of `shell` among tool calls beside the solve rate.
- **Template skew.** The GGUF/llama.cpp path is not re-evaluated; the adapter is trained and
  scored through vLLM.
- **The harness is frozen.** The profile and prompts are hashed at training time and
  publishing refuses a changed one.
- **Only reruns of infrastructure failures are automatic.** A verifier that crashes leaves the
  task ungraded and is not rerun.

## Modules

| Step | Module |
| --- | --- |
| Stages, markers, preflight | `pipeline` |
| Processes, health, HF Jobs | `launcher`, `commands` |
| Training split | `dataset` |
| Qualification gate | `qualify` |
| Training, reward, guard | `train`, `reward`, `guard` |
| Evaluation and statistics | `evaluate`, `stats` |
| Selection | `select` |
| Publishing | `publish` |

## The Harbor agent

`querymt_harbor.agent.QueryMTAgent` is a Harbor `BaseInstalledAgent` that runs the
pinned `qmt-headless` binary once per task and exports its ATIF trajectory. It does not
drive ACP and installs nothing from a package index.

### Install layout (inside the sandbox)

Everything is downloaded once on the host, verified against a SHA-256 pin in
`configs/reference.toml`, and uploaded. A rollout fetches nothing.

| Path | Content |
| --- | --- |
| `/opt/qmt/bin/qmt-headless` | pinned static musl binary (`[headless_binary]`, linux x86_64 / aarch64) |
| `/installed-agent/qmt/confs/harness_adapter.toml` | the frozen profile (only `max_steps` may change) |
| `/installed-agent/qmt/prompts/` | the profile's prompt files |
| `/installed-agent/qmt/plugins/<sha256>.wasm` | the pinned openai provider plugin (`[plugin]`) |
| `/installed-agent/qmt/providers.toml` | local-file provider, `allowed_hosts` = the endpoint host |
| `/installed-agent/qmt/models.dev.json` | empty models registry, so the harness never calls models.dev |
| `<agent log dir>/qmt/` | `sessions.db`, `run.pid`, `summary.json`; the trajectory is `<agent log dir>/trajectory.json` |

A hash mismatch (archive or plugin), an unpinned platform or a missing endpoint fails
`install()` before anything is uploaded, so no model call can follow.

`run()` writes the instruction to a file, runs `qmt-headless run` once with its PID
recorded, and in a `finally` kills it if it is still alive (TERM, then KILL) and runs
`qmt-headless export`. A rollout that hits Harbor's agent timeout therefore keeps its
trajectory and leaves no process behind (`summary.json` has `outcome=incomplete`).

### Environment variables

The launcher exports `QMT_HOME`, `QMT_CONFIG_DIR` and `QMT_PROVIDER_CACHE_DIR`, all
pointing at `/installed-agent/qmt`. The `querymt` seam supplies, to agent-phase commands
only: `QMT_BASE_URL` (`<capture proxy>/v1`), `QMT_API_KEY` (the session id) and
`QMT_MODEL`. The verifier's environment (including `OPENAI_API_KEY`) is never touched.

Agent kwargs: `step_limit` (integer >= 1; rewrites `max_steps` only), `reference` (path to
an alternative config; also `QMT_HARNESS_CONFIG`), `cache_dir` (download cache; also
`QMT_HARNESS_CACHE`, default `~/.cache/querymt-harbor`).

Opt-in environment variables of the wrappers below (unchanged behaviour when unset):

- `QMT_DIRECT_ADVERTISE_HOST`: the host a Docker sandbox reaches the capture proxy on with
  `--expose direct` (`host.docker.internal`). OpenEnv advertises `127.0.0.1`, which inside a
  container is the container itself.
- `QMT_AGENT_TIMEOUT_SEC`: overrides the task's agent timeout, which the OpenEnv CLI cannot.

### The `querymt` harness

`querymt_harbor.seam` adds a `querymt` entry to OpenEnv's seam table (idempotently). Use
the wrappers instead of the `openenv` CLI; they take the same arguments:

```
python -m querymt_harbor.serve   ...   # openenv harbor serve ...
python -m querymt_harbor.rollout ...   # openenv harbor rollout ...
```

Training and evaluation `import querymt_harbor.seam` before creating sessions. Other
harnesses behave as without the wrapper.

### Building and pinning `qmt-headless`

Until CI publishes `qmt-headless-<ver>-<target>.tar.gz` assets (nightly), build and pin local
archives:

```
cargo zigbuild --release -p querymt-agent --example qmt-headless --target aarch64-unknown-linux-musl
cargo zigbuild --release -p querymt-agent --example qmt-headless --target x86_64-unknown-linux-musl
scripts/package-headless.sh                       # writes configs/local.toml (not committed)
QMT_HARNESS_CONFIG=configs/local.toml uv run ...
```

## Training

```
python -m querymt_harbor.train --server http://host:8200 --vllm-url http://host:8000 \
    --split <dataset the server serves> [--override optimizer.learning_rate=1e-5]
```

`train.py` is a thin fork of TRL's `examples/async_grpo_harbor/async_grpo_harbor.py` at the
pinned commit. It differs in exactly these ways: the reward function, rollouts through the
safeagents proxy while weights sync to vLLM, every checkpoint kept and resumed from, all
defaults read from `reference.toml`, and the resolved configuration (with the SHA-256 of the
profile, prompts, plugin and binary) written to `training_config.json` before the first step.

- **Qualification gate.** `qualify` checks one rollout under the configured guard: a reward,
  at least one turn, exactly one root, trainable tokens above zero and `atif=match`. Training
  refuses to start without a passing `qualify.json`.
- **Reward.** Correctness (`correctness`, else `reward`), plus `bonus_weight * clip(1 -
  tool_calls / tool_budget, 0, 1)` only when it is 1.0. An ungraded rollout is unscored (not 0).
  A timeout keeps the verifier's score.
- **Guard.** safeagents replaces a flagged tool call with a `[safeagents] blocked call to ...`
  notice before the proxy records the response. A rollout with any blocked call scores 0, and
  `guard_blocked_rollouts` is logged per step. `guard.enabled = false` skips the proxy.
- **Resume.** A restart resumes from the latest complete `checkpoint-N` (one with
  `trainer_state.json` and the adapter weights); a changed configuration is refused.
- **Training split.** `dataset` samples scale800 and termgrade with a seed, interleaves them
  in their ratio, rejects any instruction that matches an evaluation benchmark (SHA-1) and
  writes real files, never symlinks.

### Requirements of the trainer host

- A GPU host, vLLM with LoRA enabled (`--enable-lora --max-lora-rank >= lora.r`,
  `VLLM_ALLOW_RUNTIME_LORA_UPDATING=1`) and a Harbor server (`openenv harbor serve`).
- `harbor_env.harness` (the session factory) is not in the OpenEnv wheel, so
  `querymt_harbor/harbor_session.py` is a vendored, unmodified copy of it from the `v0.7.0` tag
  (excluded from ruff). Nothing needs checking out or putting on `PYTHONPATH`. When `openenv`
  is bumped in `pyproject.toml`, copy the file again from the new tag.
- The Harbor server refuses (it does not queue) a rollout beyond `MAX_CONCURRENT_ENVS`, whose
  OpenEnv default is 4. The launcher sets it from `serving.max_concurrent_rollouts` (64); keep
  that at least as large as the trainer's rollouts in flight and `eval.concurrency` (16, checked
  before any evaluation starts). A refused rollout is classified as an infrastructure failure
  and rerun.
- **Known limitation:** `micro_batch_max_tokens` (65,536) maps to TRL's `token_budget`. A
  rollout sample longer than it is dropped with a warning, and vLLM's `max_model_len` is
  131,072, so very long rollouts do not train. Raise it if memory allows.

## Evaluation and selection

- `stats.paired_report` compares two score tables on the tasks graded for both: mean
  difference in points, 95% interval, newly solved and lost, an exact sign test, and the
  excluded tasks.
- `select.declare_selection_set` fixes the selection set (SWE-bench Pro hard51 plus an
  18-task Terminal-Bench 2.1 subset by seed) in `selection_set.json` before any evaluation, and
  refuses to change it afterwards. `select_and_report` picks the checkpoint with the highest
  solve rate on that set only (ties go to the earlier step) and reports the remaining
  Terminal-Bench tasks as the held-out paired comparison with the base model.

## Publishing

`publish.build_bundle` assembles the bundle locally: adapter weights and config, tokenizer,
the profile and prompts under `querymt/`, `training_config.json`, `selection.json`,
`eval_report.json` and a generated model card with the vLLM and llama.cpp serving commands and
the `qmt-headless run` command. It refuses to build if the profile or a prompt no longer
matches the SHA-256 recorded at training time. Trajectories go through `safeagents scrub`
(re-scrubbed and checked) and carry a canary string marking them as evaluation data. Upload
happens only after the bundle is built.

## Upgrading pins

- **OpenEnv / Harbor** (`pyproject.toml`): re-run `uv lock`, then `uv run pytest`.
  `tests/test_agent.py` and `tests/test_rollout.py` exercise Harbor's agent internals;
  `tests/test_seam.py` covers `seams.SEAMS` and `build_trial_config`. Fix those first if they break.
- **TRL** (`pyproject.toml`): the `AsyncGRPOConfig` field mapping in `reference.toml` and
  `tests/test_train.py` is checked against the pinned commit; re-run it after a bump.
- **`qmt-headless`**: edit `[headless_binary]` in `reference.toml` (URL and SHA-256 per
  platform). The profile must match that build.
- **Plugin**: edit `[plugin]` (manifest digest and wasm SHA-256). Changing it changes the
  trained-against artifact, so re-train or record the change.
- **safeagents**: `tests/test_guard.py` runs the pinned package and fails if its block notice
  changes.

## Tests

```
env -u RUST_LOG uv run pytest                 # offline; builds qmt-headless and the wasm once
env -u RUST_LOG QMT_LIVE_PLUGIN=1 uv run pytest    # also downloads the pinned plugin
QMT_RUN_DOCKER=1 uv run pytest                # also the no-CA-certificates container test
QMT_RUN_LIVE_HARBOR=1 QMT_LIVE_LLM_URL=... QMT_LIVE_MODEL=... QMT_LIVE_DATASET=... \
  QMT_HARNESS_CONFIG=configs/local.toml uv run pytest tests/test_harbor_live.py
```

The live tests need Docker and an OpenAI-compatible endpoint. `QMT_LIVE_TIER=vllm` also asserts
trainable tokens; the default `local` tier (llama-server) captures text only.
