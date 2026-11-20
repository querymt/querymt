# Spec Delta

## Purpose

Defines how a QueryMT harness adapter (a LoRA trained with RL to run inside the QueryMT harness, `qmt-headless`) is trained, qualified, evaluated, selected and published, so that one documented pipeline produces a reproducible adapter and the exact harness profile it was trained for.

## ADDED Requirements

### Requirement: Harness profile endpoint from environment
The harness adapter profile SHALL take the OpenAI-compatible endpoint, served model name and API key only from `QMT_BASE_URL`, `QMT_MODEL` and `QMT_API_KEY`, using provider `openai` with chat completions. All three variables are required and have no defaults. A missing variable SHALL fail profile loading with an error that names it.

#### Scenario: Profile resolves endpoint from environment
- **WHEN** the QueryMT harness loads the harness adapter profile with `QMT_BASE_URL=http://proxy/v1`, `QMT_MODEL=m` and `QMT_API_KEY=k` set
- **THEN** the session's provider is `openai`, its model is `m`, its API key is `k` and its requests go to `http://proxy/v1/chat/completions`

#### Scenario: Missing endpoint variable fails profile load
- **WHEN** the QueryMT harness loads the harness adapter profile without `QMT_BASE_URL` set
- **THEN** loading fails before any session starts and the error names `QMT_BASE_URL`

### Requirement: Fixed harness tool set
The harness adapter profile SHALL expose exactly these tools: `edit`, `read_tool`, `write_file`, `multiedit`, `replace_symbol`, `glob`, `search_text`, `ls`, `index`, `get_symbol`, `get_function`, `find_symbol_references`, `shell`, `todowrite`, `todoread`. Tools that wait for a user (`question`), reach the web (`browse`), load skills (`skill`) or recover pruned context (`session_history`) SHALL NOT be exposed.

#### Scenario: Profile advertises the fixed tool set
- **WHEN** a session is created from the harness adapter profile
- **THEN** the tool declarations sent to the model are exactly the fifteen listed tools, in a stable order

#### Scenario: Interactive and web tools are absent
- **WHEN** a session is created from the harness adapter profile
- **THEN** no tool named `question`, `browse`, `skill` or `session_history` is declared to the model

### Requirement: Append-only model history
A session run from the harness adapter profile SHALL NOT rewrite earlier conversation content and SHALL NOT make auxiliary model calls. Pruning, automatic compaction and context-threshold compaction are disabled, so each model request's messages begin with the previous request's messages and the response to it.

#### Scenario: Consecutive requests share the prior prefix
- **WHEN** a session from the harness adapter profile makes two consecutive model requests in one prompt turn with large tool outputs in between
- **THEN** the second request's messages begin with every message of the first request followed by the first response, unchanged

#### Scenario: No auxiliary model calls
- **WHEN** a session from the harness adapter profile completes a prompt turn
- **THEN** every model request it made belongs to the agent loop, and no title, summary or compaction request was sent

### Requirement: Model-call budget
The harness adapter profile SHALL cap model calls per session at 50 by default through the `limits` middleware (`[[middleware]] type = "limits"`, `max_steps`, integer ≥ 1). The Harbor agent SHALL accept a different budget per run and apply it to the profile it uploads. A budget below 1 SHALL be rejected before the rollout starts.

#### Scenario: Session stops at the call budget
- **WHEN** a session from the harness adapter profile reaches 50 model calls without finishing
- **THEN** it makes no further model call and the run ends with stop type step limit

#### Scenario: Per-run budget overrides the default
- **WHEN** the Harbor agent is constructed with a step limit of 100
- **THEN** the profile uploaded to the sandbox has `max_steps = 100` and nothing else in it differs from the repository profile

#### Scenario: Invalid budget is rejected
- **WHEN** the Harbor agent is constructed with a step limit of 0
- **THEN** construction fails with an error naming the step limit, and no sandbox command is run

### Requirement: Endpoint-neutral system prompt
The harness adapter profile's system prompt SHALL NOT contain the provider name, model name, endpoint URL or API key. Otherwise it SHALL be the default QueryMT system prompt plus the environment block (git status, platform, date, directory tree).

#### Scenario: Prompt omits endpoint identity
- **WHEN** a session from the harness adapter profile is created with `QMT_MODEL=adapter-x` and `QMT_BASE_URL=http://proxy-y/v1`
- **THEN** the rendered system prompt contains neither `adapter-x`, `proxy-y` nor `openai`

### Requirement: Harbor rollout runs one headless QueryMT session
The querymt Harbor agent SHALL install a SHA-256-pinned `qmt-headless` binary and the profile into the sandbox without fetching anything from a package index or the network inside the sandbox. It SHALL then run `qmt-headless run` once, which creates one session whose working directory is the task workspace and sends the task instruction as the session's only prompt, with no step waiting for a user. Install failures SHALL fail the trial before any model call.

#### Scenario: Rollout runs one headless session
- **WHEN** Harbor runs the querymt agent on a task
- **THEN** `qmt-headless run` is executed once with the task workspace as its working directory and its session database in the agent log directory, the summary and `trajectory.json` name the same single session id with no subagent trajectories, and the first model request's user message carries the task instruction

#### Scenario: Checksum mismatch fails install
- **WHEN** the `qmt-headless` archive does not match its pinned SHA-256
- **THEN** the trial fails during agent install, and the capture proxy records no model call for its session

### Requirement: Rollout independent of image CA certificates
The querymt harness SHALL NOT depend on CA certificates in the task image. When the system store is empty, its HTTP clients SHALL fall back to roots bundled in the binary instead of failing.

#### Scenario: Rollout runs in an image without CA certificates
- **WHEN** `qmt-headless run` runs in an image with no system CA certificates against a plain-http endpoint
- **THEN** it starts, its model request reaches the endpoint, and it exits 0

### Requirement: Provider plugin pinned and installed locally
The querymt Harbor agent SHALL install the provider plugin into the sandbox as a local file matching the SHA-256 pinned in the experiment configuration. A rollout SHALL NOT download a plugin, a provider list or models metadata. The pinned hash SHALL be recorded in the training record and the published bundle.

#### Scenario: Plugin is installed from the pin
- **WHEN** the agent installs into a sandbox
- **THEN** the uploaded wasm has the pinned SHA-256, `providers.toml` points at that local path with no `oci://` reference, and its `allowed_hosts` contains the endpoint host

#### Scenario: Plugin hash mismatch fails install
- **WHEN** the plugin bytes do not match the pinned SHA-256
- **THEN** the fetch or install fails before the harness binary runs and the capture proxy records no model call for the session

#### Scenario: Install carries a models cache
- **WHEN** the agent installs into a sandbox
- **THEN** the sandbox has a models cache file in the directory named by `QMT_PROVIDER_CACHE_DIR`, so the harness makes no models-metadata request

#### Scenario: Rollout needs no network fetch
- **WHEN** `qmt-headless run` runs the pinned plugin with the installed `providers.toml` and models cache and no route to ghcr.io, repo.query.mt or models.dev
- **THEN** the prompt completes and its request carries the configured key

### Requirement: Credentials scoped to the agent phase
The querymt Harbor agent SHALL deliver the capture endpoint, session key and model name only to agent-phase commands. It SHALL NOT set `OPENAI_API_KEY`, `OPENAI_BASE_URL` or any other variable in the trainer or server process, or in the environment the task verifier runs in.

#### Scenario: Verifier keeps its own credentials
- **WHEN** a rollout of the querymt agent finishes and the verifier runs with `OPENAI_API_KEY=grader-key` in the server environment
- **THEN** the verifier sees `OPENAI_API_KEY=grader-key`, and `QMT_API_KEY` is not set in its environment

### Requirement: Native ATIF trajectory
Every rollout of the querymt Harbor agent SHALL write `agent/trajectory.json` produced by QueryMT's own ATIF exporter from the session database, with one agent step per completed model call carrying that call's prompt and completion token counts, and a run summary with session id, outcome, stop type, error and model call count.

#### Scenario: Trajectory matches model calls
- **WHEN** a rollout makes three model calls against an endpoint that reports usage for each
- **THEN** `trajectory.json` has three agent steps, and each step's `prompt_tokens` and `completion_tokens` equal the usage the endpoint reported for that call

### Requirement: Trajectory exported after the run ends
After the run process ends, however it ended (normal end, step limit, error, Harbor's agent timeout or a kill), the querymt Harbor agent SHALL stop any run process still alive and then export the trajectory and summary from the session database. The summary outcome SHALL be `end_turn`, `stopped`, `error` or `incomplete`; `incomplete` means the run recorded no normal end.

#### Scenario: Trajectory survives the budget stop
- **WHEN** a rollout reaches its step limit
- **THEN** `qmt-headless run` exits with status 0, and the exported `trajectory.json` has one agent step per model call and the summary records stop type step limit

#### Scenario: Trajectory survives a killed run
- **WHEN** the run process is killed after two completed model calls while the third is pending
- **THEN** the export writes `trajectory.json` with two agent steps and the summary records outcome `incomplete`

#### Scenario: Trajectory survives Harbor's agent timeout
- **WHEN** Harbor cancels the agent's run while `qmt-headless run` is still running
- **THEN** when the agent's run returns, no `qmt-headless run` process is left, and `trajectory.json` and the summary exist in the agent log directory

### Requirement: Session database confined to the agent log directory
`qmt-headless run` SHALL write session state only at the database path it is given, and the querymt Harbor agent SHALL place that path in the agent log directory. `qmt-headless export` SHALL only read the database.

#### Scenario: Export only reads
- **WHEN** `qmt-headless export` runs on a session database
- **THEN** the database's sessions and events are unchanged afterwards

#### Scenario: Session data stays in the log directory
- **WHEN** a rollout runs with `HOME`, `QMT_HOME` and the workspace in separate directories
- **THEN** no session database file or run-lease directory exists in any of them, and the session database exists at the given path

### Requirement: querymt harness registration
The experiment's server and rollout wrappers SHALL register a harness named `querymt` with the pinned OpenEnv version before delegating to OpenEnv's own CLI. That harness uses the chat-completions dialect and the served model name unchanged. Every other harness and every OpenEnv CLI option SHALL behave as without the wrapper.

#### Scenario: Wrapper exposes the querymt harness
- **WHEN** the experiment server wrapper starts and a client lists available harnesses
- **THEN** `querymt` is listed and a rollout requested with `harness=querymt` is built from the querymt Harbor agent

#### Scenario: Wrapper leaves other harnesses unchanged
- **WHEN** a rollout with `harness=pi` is requested through the experiment wrapper
- **THEN** its trial configuration equals the one built by the unwrapped OpenEnv CLI for the same inputs

### Requirement: Qualification gate before training
The pipeline SHALL NOT start training until a qualification rollout of the querymt harness under the configured guard has passed. It passes when the verifier produced a reward, at least one turn was captured, all turns form one root, trainable tokens are above zero, and the trace check is `match`. The rollout's result SHALL be recorded.

#### Scenario: Passing qualification unlocks training
- **WHEN** the qualification rollout reports reward present, `turns=7`, `roots=1`, `trainable_tokens=3120` and `atif=match`
- **THEN** the pipeline records the result and proceeds to the train stage

#### Scenario: Forked history blocks training
- **WHEN** the qualification rollout reports `roots=3`
- **THEN** the pipeline stops before training and reports that the harness history was rewritten

#### Scenario: Missing tokens block training
- **WHEN** the qualification rollout reports a reward but `trainable_tokens=0`
- **THEN** the pipeline stops before training and reports that the engine or proxy did not return token ids and logprobs

#### Scenario: Trace mismatch blocks training
- **WHEN** the qualification rollout reports `atif=MISMATCH` or `atif=none`
- **THEN** the pipeline stops before training and reports that the trajectory and the token capture disagree or the trajectory is missing

### Requirement: Rollout reward
The rollout reward SHALL be the verifier's `correctness`, or `reward` when `correctness` is absent. If correctness ≥ 1.0 it SHALL add `bonus_weight × clip(1 − tool_calls / tool_budget, 0, 1)`, defaults 0.1 and 15. A rollout whose verifier did not run SHALL be unscored, which is distinct from 0. A timeout SHALL keep the verifier's score.

#### Scenario: Solved rollout earns the efficiency bonus
- **WHEN** a rollout has `correctness=1.0` and 6 tool calls with default weights
- **THEN** its reward is `1.0 + 0.1 × 0.6 = 1.06`

#### Scenario: Unsolved rollout earns no bonus
- **WHEN** a rollout has `correctness=0.0` and 2 tool calls
- **THEN** its reward is `0.0`

#### Scenario: Reward key falls back
- **WHEN** a verifier returns only `{"reward": 1.0}`
- **THEN** the rollout's correctness is taken as `1.0`

#### Scenario: Ungraded rollout is excluded
- **WHEN** a rollout's verifier produced no reward
- **THEN** the rollout is unscored and does not enter its group's baseline

### Requirement: Guarded rollouts score zero
When the safety guard is enabled (default on), every model response in a rollout SHALL pass through safeagents before the harness sees it. A rollout in which any tool call was blocked SHALL receive reward 0 regardless of the verifier, and the block SHALL be counted in the training metrics.

#### Scenario: Blocked tool call zeroes reward
- **WHEN** a guarded rollout has one tool call replaced by a safeagents block notice and the verifier returns `correctness=1.0`
- **THEN** the rollout's reward is `0.0` and the step's `guard_blocked_rollouts` metric is incremented

#### Scenario: Clean guarded rollout is scored normally
- **WHEN** a guarded rollout has no blocked tool call and `correctness=1.0` with 15 tool calls
- **THEN** the rollout's reward is `1.0`

### Requirement: Training task mix
The training dataset SHALL interleave the scale800 and TermGrade training splits in a fixed ratio (default 750 and 300 tasks) in a deterministic order derived from a recorded seed. It SHALL contain no task whose instruction also appears in an evaluation benchmark.

#### Scenario: Task order is reproducible
- **WHEN** the dataset is built twice with the same seed and source revisions
- **THEN** both builds list the same task identifiers in the same order

#### Scenario: Benchmark overlap is rejected
- **WHEN** a training split contains a task whose instruction hash equals that of a Terminal-Bench 2.1 or SWE-bench Pro hard51 task
- **THEN** the build fails and names the overlapping task

### Requirement: Recorded training configuration
Each training run SHALL write its full resolved configuration to the run directory before the first step. This covers model and dataset revisions, `qmt-headless` version and checksum, plugin hash, profile hash, LoRA, optimiser, sampling, budget and reward settings, guard settings and seed. Unset values default to the harness-hub pi adapter recipe.

#### Scenario: Defaults reproduce the reference recipe
- **WHEN** a run starts with no overrides
- **THEN** the recorded configuration has LoRA r=32, alpha=64 on `q_proj,k_proj,v_proj,o_proj,gate_proj,up_proj,down_proj`, learning rate 2e-5, 8 generations at temperature 0.8 and top-p 1.0 with thinking off, 50 agent steps, 1800 s agent timeout, maximum staleness 16 and 1000 planned steps

#### Scenario: Configuration is written before training
- **WHEN** the first optimiser step begins
- **THEN** the run directory already contains the resolved configuration file including the profile SHA-256

### Requirement: Checkpoints and resume
A training run SHALL save the LoRA adapter at a fixed step interval (default every 10 steps) and keep every saved checkpoint. Restarting a run with the same run directory SHALL resume from the latest complete checkpoint and SHALL NOT repeat completed steps.

#### Scenario: Checkpoints are kept at the interval
- **WHEN** a run with interval 10 completes step 30
- **THEN** adapter checkpoints for steps 10, 20 and 30 exist in the run directory

#### Scenario: Interrupted run resumes
- **WHEN** a run is stopped after step 34 and restarted with the same run directory
- **THEN** training continues from the step-30 checkpoint and the next saved checkpoint is step 40

### Requirement: Paired evaluation
Evaluation SHALL run the base model and each candidate adapter under the same profile, one attempt per task, at temperature 0.8, top-p 1.0, thinking off, with 100 agent steps. The time limit SHALL be 2 h per SWE-bench Pro hard51 task and 1 h per Terminal-Bench 2.1 task. Comparisons SHALL use only tasks graded for both sides.

#### Scenario: Paired report statistics
- **WHEN** base and adapter are both graded on 51 tasks, the adapter newly solves 15 and loses 2
- **THEN** the report shows a difference of +25.5 points with a 95% normal interval of ±14.2 points on the paired per-task differences (variance with divisor n), `15 / 2` newly solved/lost, and an exact two-sided sign-test p-value of 0.0023

#### Scenario: Ungraded tasks are excluded from pairs
- **WHEN** the base model is ungraded on 3 Terminal-Bench 2.1 tasks that the adapter graded
- **THEN** the paired comparison uses the remaining tasks and lists the 3 excluded task identifiers

### Requirement: Infrastructure failures are rerun
An evaluation attempt that failed because the model endpoint was unreachable from the sandbox, the sandbox failed to start, or the agent failed to install SHALL be marked as an infrastructure failure. It SHALL NOT be scored, and SHALL be rerun up to the configured limit (default 2). Attempts still failing are reported as ungraded.

#### Scenario: Tunnel outage is rerun
- **WHEN** an evaluation attempt ends with zero captured turns and a connection error to the capture endpoint
- **THEN** the attempt is marked as an infrastructure failure, rerun, and only the rerun's grade appears in the report

### Requirement: Checkpoint selection on a declared subset
The released checkpoint SHALL be chosen by the highest combined solve rate on a selection set declared before evaluation starts. The default selection set is SWE-bench Pro hard51 plus a fixed 18-task subset of Terminal-Bench 2.1, recorded by task identifier. The report SHALL include the Terminal-Bench 2.1 result on the tasks outside the selection subset.

#### Scenario: Selection uses only the declared set
- **WHEN** checkpoints 140, 160 and 180 score 0.52, 0.58 and 0.55 on the selection set
- **THEN** step 160 is selected, and the decision record lists the selection task identifiers and each checkpoint's score

#### Scenario: Held-out result is reported
- **WHEN** the final report is produced for the selected checkpoint
- **THEN** it contains a paired Terminal-Bench 2.1 comparison over only the tasks outside the 18-task selection subset

### Requirement: Published adapter bundle
Publishing SHALL produce a model repository containing:
- The PEFT LoRA adapter and its tokenizer.
- The exact profile and prompt files under `querymt/`.
- The resolved training configuration.
- The evaluation report and selection record.
- A model card stating the base model, harness, recipe, results and serving instructions.

Publishing SHALL refuse to proceed when the profile hash differs from the one recorded in training.

#### Scenario: Bundle contains the trained harness profile
- **WHEN** the selected checkpoint is published
- **THEN** the repository contains `adapter_model.safetensors`, `adapter_config.json`, `querymt/harness_adapter.toml`, `querymt/prompts/`, `training_config.json`, `eval_report.json` and `README.md`

#### Scenario: Profile drift blocks publishing
- **WHEN** the repository profile was edited after training so that its SHA-256 differs from the recorded one
- **THEN** publishing fails and names both hashes

### Requirement: Scrubbed trajectories
Trajectories published with the adapter SHALL first be redacted with `safeagents scrub`. Every published file SHALL carry a canary string marking it as evaluation data not to be trained on. Unredacted trajectories SHALL never be uploaded.

#### Scenario: Secrets are redacted before upload
- **WHEN** an evaluation trajectory contains `QMT_API_KEY=sess-123` and the operator's home directory
- **THEN** the uploaded copy contains neither `sess-123` nor the home directory path, and contains the canary string

### Requirement: Serving the adapter with QueryMT
Running `qmt-headless run` with the harness adapter profile SHALL serve the adapter under the configuration it was trained in. The endpoint, set through the three environment variables, serves the base model with the LoRA, chat completions and thinking off. The published model card SHALL give this exact command sequence for vLLM and for llama.cpp, ending in the `qmt-headless run` command with the bundled profile.

#### Scenario: Adapter served through the trained profile
- **WHEN** vLLM serves the base model with the adapter under the name `qmt-adapter` and `qmt-headless run` starts with the harness adapter profile, `QMT_BASE_URL` pointing at it and `QMT_MODEL=qmt-adapter`
- **THEN** the instruction given to `qmt-headless run` produces chat-completion requests for model `qmt-adapter` with the fifteen profile tools and no thinking content

### Requirement: End-to-end pipeline command
The experiment SHALL be runnable as `pipeline run` with one configuration file. The stages are preflight, qualify, baseline, train, evaluate, select, publish. Each stage records completion in the run directory; rerunning skips completed stages and resumes the first incomplete one. Any stage can be run alone.

#### Scenario: Preflight reports missing prerequisites
- **WHEN** `pipeline run` starts with `E2B_API_KEY` unset and only one GPU visible
- **THEN** it stops before starting any service and reports both the missing credential and the GPU count

#### Scenario: Rerun resumes after the last completed stage
- **WHEN** a pipeline run whose preflight, qualify and baseline stages completed is rerun with the same run directory
- **THEN** it starts at the train stage without repeating qualification or baseline rollouts
