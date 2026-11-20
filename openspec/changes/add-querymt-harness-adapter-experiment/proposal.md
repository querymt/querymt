# Proposal

## Why

harness-hub showed that an RL-trained LoRA adapter for one specific harness (Qwen3.8-27B under pi) solves far more tasks with fewer turns (SWE-bench Pro hard51 39.2% → 64.7%, Terminal-Bench 2.1 39.5% → 55.8%). The recipe is public (TRL AsyncGRPO + Harbor tasks served by OpenEnv) but only works for harnesses OpenEnv can drive. QueryMT has no seam there, so nobody can reproduce the experiment for the querymt-agent harness. This change adds everything needed to run it end to end and publish a QueryMT adapter.

## What Changes

- Add a frozen training/deployment profile for the QueryMT harness (`qmt-headless`). It takes the endpoint, model and key from the environment and drops the interactive and network tools. It keeps conversation history unrewritten (no pruning or compaction) and bounds model calls with the `limits` middleware. A matching system-prompt template omits the provider/model line.
- Add `qmt-headless`, a dedicated headless binary (`crates/agent/examples/qmt-headless.rs`, based on `from_config.rs`) with two subcommands. `run` loads the profile and runs the task instruction as one prompt in one session, writing to a given session database. `export` reads that database and writes the session as ATIF with QueryMT's own exporter plus a small summary JSON, however the run ended. It needs no feature flags (no dashboard, mesh or OAuth), and is built as static musl archives for x86_64 and aarch64 in the nightly and release workflows.
- Add a Python experiment package under `experiments/harness-adapter/`. It contains:
  - A pinned provider plugin: the `openai` wasm is fetched by SHA-256 on the host and installed into each sandbox as a local file with an endpoint-allowing `providers.toml` and an empty models cache, so rollouts make no plugin or metadata download.
  - A Harbor agent class that installs the SHA-256-pinned `qmt-headless`, uploads the profile and runs one headless session per rollout, with credentials scoped to the agent phase. The trajectory Harbor and OpenEnv read (`agent/trajectory.json`) is QueryMT's native ATIF export. The agent exports it after the run process ends, so a timeout or kill still yields the completed steps. Nothing is installed into the sandbox from a package index.
  - An OpenEnv seam named `querymt`, registered at server start by a wrapper around `openenv harbor serve`, so OpenEnv itself is not forked.
  - A training script derived from TRL's `async_grpo_harbor.py`. It follows the harness-hub card's recipe: scale800 + TermGrade interleaved, the `correctness` reward plus a 0.1 tool-efficiency bonus, LoRA r=32/alpha=64 on q,k,v,o,gate,up,down, 8 rollouts at temperature 0.8, a 50-step and 1,800 s budget, and up to 16 stale policy versions. An optional safeagents guard sets reward 0 for blocked rollouts.
  - A launcher that starts the Harbor server, vLLM (LoRA hot-swap, token-id/logprob capture) and the trainer on one 2-GPU host, locally or on Hugging Face Jobs. It runs a qualification smoke rollout before training is allowed to start.
  - An evaluation tool that runs base model and checkpoints under the same profile on SWE-bench Pro hard51 and Terminal-Bench 2.1, produces a paired report (difference, 95% interval, newly solved/lost, exact sign test) and picks the checkpoint on a declared selection subset.
  - A publishing tool that pushes the adapter together with the exact profile, prompts, training config, evaluation report and a model card. It optionally adds a GGUF LoRA, and uploads scrubbed trajectories.
- Add a runbook (`experiments/harness-adapter/README.md`) that covers the whole sequence: prerequisites → smoke → baseline → train → evaluate → select → publish → serve with QueryMT. Add a pointer to it from the agent examples docs.

Two small runtime changes, both found while testing against the real binary:

- **Fix:** a profile's `[agent] api_key` was dropped at session creation (documented, validated, but never reached the provider). It is now applied as the key override for the profile's own provider and model only, and is still never persisted.
- **Fix:** HTTP clients panicked on Linux systems without CA certificates (common in sandbox images), even for plain-http endpoints and for tools the profile does not offer. `querymt` gains `outbound::build_client`, which falls back to bundled Mozilla roots when the system store is empty. The provider HTTP clients and the `browse` and `web_fetch` tools use it.

The spec's stable tool order relies on `ToolRegistry` being ordered by name (`BTreeMap`), which comes from the prompt-caching branch (`reduce-agent-harness-overhead`); this change does not touch `tools/registry.rs`.

No public Rust API change beyond the additive `querymt::outbound::build_client`, and no TOML schema, ACP, hook, middleware, provider wire, Extism, session DB, Python binding or typeshare change. Nothing is **BREAKING**. The per-step run-objective reminder also forks the prefix; that is handled by `run_objective = false` on a separate branch, not here.

## Non-goals

- LoRA loading in the built-in `llama-cpp` provider. Local serving goes through `llama serve --lora` or vLLM behind the `openai` provider.
- Driving qmtcode over ACP inside the sandbox. Harbor's ACP runner needs a package install in the sandbox and converts ACP events to ATIF lossily; the headless binary avoids both.
- Upstreaming the seam or agent class to OpenEnv/Harbor, which is a later follow-up.
- SFT on exported QueryMT sessions, reward redesign, multi-harness adapters, or thinking-mode training.
- Shipping the adapter by default in any QueryMT profile or the desktop app.

## Capabilities

### New Capabilities

- `harness-adapter-training`: The QueryMT training/deployment profile contract, the Harbor/OpenEnv integration of the querymt harness, the training reward and data contract, qualification, paired evaluation, checkpoint selection, and the published adapter bundle.

### Modified Capabilities

None.

## Impact

- Cargo packages:
  - `querymt-agent-runtime` applies the profile `api_key` (`session/provider.rs`).
  - `querymt` adds `outbound::build_client` and a direct `webpki-root-certs` dependency under `http-client`, used by its two HTTP clients (`outbound.rs`, the extism host) and by the `browse` and `web_fetch` tools in `querymt-agent-services`. Every crate above `querymt` rebuilds.
  - `querymt-agent` gains a new example profile `crates/agent/examples/confs/harness_adapter.toml`, a prompt `crates/agent/examples/prompts/harness_adapter_meta.jinja2`, the `qmt-headless` example binary (default features only; its CLI uses the crate's existing `clap` dev-dependency), and integration tests for the profile and the binary's CLI. No feature-gated code changes. The tests run with querymt-agent's CI feature set (`dashboard,oauth,zbus-secret-service,systemd-credentials,remote`).
- CI: `.github/workflows/build.yml` gains `qmt-headless` entries for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, and its example build/copy steps handle more than `qmtcode`. Nightly and stable releases gain `qmt-headless-<ver>-<target>.tar.gz` assets.
- New non-Cargo directory `experiments/harness-adapter/`. It is a uv-managed Python 3.12 project pinning `openenv[harbor]==0.7.0`, Harbor, TRL (git revision), PEFT, vLLM, datasets, trackio and optionally safeagents. It is not part of the workspace build, wheels or CI test matrix.
- Docs: a new runbook and a pointer in `docs/docs/agent/examples.md`.
- External resources needed to run (not to build or test):
  - A host with 2 NVIDIA GPUs that can serve and train a 27B model (B200 or H200, or the HF Jobs `h200x2` flavor).
  - A sandbox backend credential (for example `E2B_API_KEY`) and `HF_TOKEN`.
  - Access to the four Harbor datasets.
- querymt-desktop: no UI follow-up required.
