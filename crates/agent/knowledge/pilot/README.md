# Repository-knowledge pilot (M5)

Controlled, opt-in pilot of concept-first repository knowledge for three
`crates/agent/src` subtrees: `agent/execution`, `session` and `tools`, plus the
repository-knowledge delivery code that hooks into them. It does not enable
anything by default and does not add per-directory inventories.

## Corpus arms

| Arm | What is indexed | Status |
|-----|-----------------|--------|
| `baseline` | Nothing (current source-navigation workflow). | Defined; only a model-backed trial can score it. |
| `curated` | Five reviewed notes in `crates/agent/knowledge/` (one map listing, four concept notes) plus source records for the pilot subtrees, selected by `.qmt/knowledge.toml`. | Replayed offline (below). |
| `exhaustive` | The curated corpus plus a File/Purpose row for every file. | **Not built.** Scaffolding only creates empty purpose cells (`scaffold crates/agent/src/tools` previews 8 empty rows, nothing applied); the purposes must be written and reviewed by a person. Root `AGENTS.md` is unchanged. |

Maps are generated on demand (`map --path <dir>`) and not exported: the default
byte budget truncates each subtree (execution 4 of 7 files shown, session 5 of
36, tools 6 of 44), so a committed map would need a narrower scope per
directory. No map is committed.

## Frozen inputs (`pilot/questions.toml`)

- Question set version 1 at revision `42823824+worktree`. The pilot is not
  committed yet, so the pin that actually identifies the inputs is the corpus
  fingerprint: `curated = b7c75bf6ae632fa484e8c5add77ff80f`,
  `baseline = 354b3ec283921d37d4440b343e86c542`.
- Gate: `gate_version = 1`, fingerprint `985e6fcb143a079c9aea1a8fe6ef8e00`
  (thresholds, caps and both word lists). Held-out replay refuses to run if the
  compiled gate differs; `--allow-unfrozen` runs but labels the report
  `gate_frozen = false`.
- 13 dev and 17 held-out questions, plus 3 dev and 3 held-out scenarios. The
  loader rejects absolute paths, credentials and transcript identifiers.
  Nothing derives from a session database.

Order of events: notes and questions were written first; dev replay was used to
look at results; the harness excluded its own source file from the corpus
(changing the curated fingerprint); then the gate and fingerprints were frozen
and held-out replay ran. No gate or corpus change followed. One harness change
did follow the first held-out run: the `repeat` scenario wrongly failed when a
card the first turn had omitted for budget reasons appeared on the second turn
(it was never delivered, so that is correct). The check now only flags a card
that was actually delivered and sent again. The change touches scoring of that
scenario only, not retrieval, the gate or the corpus.

## Commands

```text
cargo run -p querymt-agent --example repository_knowledge -- --root . eval fingerprint
cargo run -p querymt-agent --example repository_knowledge -- --root . --json eval --split held-out replay --arm curated --no-timing
cargo run -p querymt-agent --example repository_knowledge -- --root . eval trials-plan
cargo run -p querymt-agent --example repository_knowledge -- --root . eval --split held-out trials-report trials.json
```

Replay copies exactly the manifest-selected files into a scratch checkout and a
fresh cache, so two runs share nothing. `--no-timing` output is byte-identical
across runs (verified).

## Held-out results, `curated`, k = 5 (offline, no model)

Retrieval, 11 positive questions (navigation 4, conceptual 4, path-free 3):

| Lane | recall@5 | MRR | wrong top hit | response bytes |
|------|----------|-----|---------------|----------------|
| documentation | 1.00 | 1.00 | 0.00 | 22,495 |
| all (default) | 0.36 | 0.18 | 0.91 | 12,843 |
| navigation | 0.27 | 0.16 | 0.91 | 12,188 |

Automatic orientation (gate v1, no entrypoint fallback):

| Measure | Result |
|---------|--------|
| Positive prompts with a card citing an expected path | 9 / 11 |
| Missed cues | 2 (`ho-nav-validation`, `ho-free-lock`) |
| Cards citing a path that was not expected | 4 of 13 cards |
| Negative / generic / outside-repo prompts receiving a gated card | 0 / 4 |
| Non-English prompts receiving a card | 0 / 2 |
| Added context, all 13 cards | 2,463 estimated tokens (9,830 bytes) |

Scenarios (shared budget 2 cards / 600 tokens / 4 KiB): parallel calls were
capped at two cards with one refused for `card_limit`; a repeated turn re-sent
no delivered card; after a compaction boundary the cards were re-selected in
the new epoch. All three passed. The `baseline` arm indexes nothing, so every
cue is missed and its scenarios report that nothing was produced; that is the
expected floor, not a measurement of the baseline workflow.

Dev replay on the same corpus (8 positives, 5 negatives): 6/8 cue hits, no false
positives, same lane pattern (documentation recall@5 1.00, default lane 0.25).

Refresh cost (dev run, debug build, one machine; timings are not compared for
determinism): cold 918 ms, 125 files hashed; warm 2 ms, 0 hashed, 125 reused.
Query median 3.1 ms, orientation median 4.9 ms.

## What these numbers do and do not show

- **The default `all` lane buries the notes.** Symbol hits from the selected
  source files fill the top five for conceptual and path-free questions, so a
  caller that reads only `all` results misses the notes that the
  documentation lane ranks first every time. This is a property of the existing
  ranking exposed by the pilot, not tuned away. Choosing the `documentation`
  lane for conceptual questions, or selecting fewer source files, is the cheap
  mitigation; changing the ranking is a separate decision.
- **Orientation is conservative, not precise.** Zero false positives on 6
  negatives, but 2 misses and 4 of 13 cards pointing at a different (usually
  neighbouring) note. Every negative prompt would still get the general
  entrypoint card on a first task, because the entrypoint fallback applies when
  no gated card exists.
- **Small and author-correlated.** 11 positives from one author who also wrote
  the notes; held-out means not used for tuning, not independent. Rates are
  indicative only and carry no significance test. Only English-vocabulary
  prompts that echo the notes' wording pass the gate by design.
- **Locating is not the same as answering.** Citing an expected path says
  nothing about whether an agent finished the task correctly or faster.
- Corpus and delivery effects are separate in the plan (`trials-plan`: the
  baseline once; the curated arm in pull-only, index-enriched and orientation
  modes; 90 trials per cell at 3 repeats over all 30 questions).

## Model-backed comparison: not run

`eval trials-plan` lists the matrix and `eval trials-report` summarizes a JSON
trial file (model, configuration, tool calls, tokens, first relevant
inspection, judged correctness, authoring/maintenance effort). Cells with no
records are reported `not_run`, a comparison is `conclusive` only when both
cells are complete, fully judged and ran with one model and configuration, and a
drop in correctness is flagged even if tool calls fell. **No model-backed
trial has been run and there is no driver in this change that executes an agent
and writes the trial file**; every cell is therefore `not_run`, and the
exhaustive arm is untested. Nothing here supports or argues against exhaustive
per-file documentation, and no repository-wide inventory is recommended or
enabled.
