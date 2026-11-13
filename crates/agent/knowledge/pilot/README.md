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

- Question set version 1, refrozen 2026-11-13 at commit `b856dc8c`. The pin that
  identifies the inputs is the corpus fingerprint:
  `curated = 8173a8ad722347e6edb1c5e7c0cd1ced`,
  `baseline = 354b3ec283921d37d4440b343e86c542`. The first freeze
  (`curated = b7c75bf6ae632fa484e8c5add77ff80f`, revision `42823824+worktree`)
  predates the changes listed under "Order of events".
- Gate: `gate_version = 1`, fingerprint `985e6fcb143a079c9aea1a8fe6ef8e00`
  (thresholds, caps and both word lists). Held-out replay refuses to run if the
  compiled gate differs; `--allow-unfrozen` runs but labels the report
  `gate_frozen = false`.
- 13 dev and 17 held-out questions, plus 3 dev and 3 held-out scenarios. The
  loader rejects absolute paths, credentials and transcript identifiers.
  Nothing derives from a session database.

Order of events (first freeze): notes and questions were written first; dev replay was used to
look at results; the harness excluded its own source file from the corpus
(changing the curated fingerprint); then the gate and fingerprints were frozen
and held-out replay ran. No gate or corpus change followed. One harness change
did follow the first held-out run: the `repeat` scenario wrongly failed when a
card the first turn had omitted for budget reasons appeared on the second turn
(it was never delivered, so that is correct). The check now only flags a card
that was actually delivered and sent again. The change touches scoring of that
scenario only, not retrieval, the gate or the corpus.

**The corpus changed after the held-out replay, so the held-out numbers are no
longer strictly held out.** Between the first and the second freeze, driven by
live sessions on a dev-style question (how the loop retries provider errors):
the OpenSpec main specs were added to the corpus (as retrieval-only, so they
are searchable but never attached automatically); `execution.md` was rewritten
(its Retry section, `knowledge_sources`) and re-acknowledged; and card
selection changed (section choice for `index` cards, description shown only
for a top-level citation). The gate did not change (`985e6fcb...`, v1).
Treat the held-out figures below as a regression check on a corpus that was
tuned after seeing live behaviour, not as an unbiased estimate.

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

## Held-out results, `curated`, k = 5 (offline, no model; refrozen corpus)

Retrieval, 11 positive questions (navigation 4, conceptual 4, path-free 3):

| Lane | recall@5 | MRR | wrong top hit | response bytes |
|------|----------|-----|---------------|----------------|
| documentation | 0.91 | 0.72 | 4 of 11 | 29,882 |
| all (default) | 0.36 | 0.18 | 10 of 11 | 13,240 |
| navigation | 0.27 | 0.15 | 10 of 11 | 12,534 |

(The first freeze measured documentation recall@5 1.00 / MRR 1.00. Adding the
main specs to the corpus gave the documentation lane more competing sections;
the first run's 1.00 did not hold after that change.)

Automatic orientation (gate v1), counting gated cards and the entrypoint
fallback separately:

| Measure | Result |
|---------|--------|
| Positive prompts with a card citing an expected path | 9 / 11 |
| Missed cues | 2 (`ho-nav-validation`, `ho-free-lock`) |
| Cards citing a path that was not expected | 4 of 13 cards |
| Negative / generic / outside-repo prompts receiving a gated card | 0 / 4 |
| Non-English prompts receiving a gated card | 0 / 2 |
| **Negative and non-English prompts receiving the entrypoint fallback card** | **6 / 6** |
| Added context, all 13 gated cards | 2,779 estimated tokens (11,102 bytes) |

Scenarios (shared budget 2 cards / 600 tokens / 4 KiB): parallel calls were
capped at two cards with one refused for `card_limit`; a repeated turn re-sent
no delivered card; after a compaction boundary the cards were re-selected in
the new epoch. All three passed. The `baseline` arm indexes nothing, so every
cue is missed and its scenarios report that nothing was produced; that is the
expected floor, not a measurement of the baseline workflow.

Dev replay on the same corpus (8 positives, 5 negatives): 6/8 cue hits, no gated
false positives, 5/5 negatives received the fallback card; documentation
recall@5 0.875, default lane 0.25.

Refresh cost (held-out run, debug build, one machine; timings are not compared
for determinism): cold 1,138 ms, 158 files hashed; warm 3 ms, 0 hashed, 158
reused. Query median 5.2 ms, orientation median 29.8 ms (max 35 ms).

## What these numbers do and do not show

- **The default `all` lane buries the notes.** Symbol hits from the selected
  source files fill the top five for conceptual and path-free questions, so a
  caller that reads only `all` results misses the notes that the
  documentation lane ranks first every time. This is a property of the existing
  ranking exposed by the pilot, not tuned away. Choosing the `documentation`
  lane for conceptual questions, or selecting fewer source files, is the cheap
  mitigation; changing the ranking is a separate decision.
- **Orientation is conservative, not precise.** Zero gated false positives on 6
  negatives, but 2 misses and 4 of 13 cards pointing at a different (usually
  neighbouring) note. "False positives" counts gated cards only: every one of
  the 6 negatives still received the general entrypoint card, because the
  fallback applies on a first task when no gated card exists. `eval replay`
  now prints that count (`entrypoint fallbacks`) next to the false-positive
  count.
- **Small and author-correlated.** 11 positives from one author who also wrote
  the notes; held-out means not used for tuning, not independent. Rates are
  indicative only and carry no significance test. Only English-vocabulary
  prompts that echo the notes' wording pass the gate by design.
- **Locating is not the same as answering.** Citing an expected path says
  nothing about whether an agent finished the task correctly or faster.
- Corpus and delivery effects are separate in the plan (`trials-plan`: the
  baseline once; the curated arm in pull-only, index-enriched and orientation
  modes; 90 trials per cell at 3 repeats over all 30 questions).

## Live comparison on read-only questions (2026-11-13)

Not the `trials-plan` matrix below; a smaller, separate run with
`scripts/kb-measure.sh`: the committed tree in a throwaway worktree, read-only
tools, OAuth (no API cost), one model (`claude-sonnet-5-5`), the same prompt with
the feature on and off, alternating arm order.

First run, 3 repeats per arm, 9 prompts (54 sessions), medians per prompt were
mixed and the overall mean was -1.6% total tokens. That total mixed
concept questions with questions the notes do not cover and counted cache
reads as equal to fresh tokens, so it is not the headline.

Rerun on the five concept prompts only, 7 repeats per arm (70 sessions), fresh
tokens (input + output + cache writes):

| Prompt | Median change, on vs off | On-runs below the off median | Rubric coverage on / off |
|--------|--------------------------|------------------------------|--------------------------|
| execution retry | -5.5% | 5 / 7 | 0.89 / 0.93 |
| relevance gate | -9.0% | 5 / 7 | 0.95 / 0.86 |
| concurrent tool calls | -37.6% | 7 / 7 | 0.74 / 0.86 |
| JSON string as array | +2.7% | 3 / 7 | 0.80 / 0.79 |
| compaction | -28.9% | 7 / 7 | 0.80 / 0.80 |
| **All** | **-14.1%** (40.4k vs 47.1k) | **26 / 35** | **0.84 / 0.85** |

Limits that matter when reading this:

- **Coverage is a keyword rubric** (presence of facts a complete answer should
  state), written by one person after reading earlier answers, scored without the
  arm label. It does not check correctness, and there was no human or model
  judge. The one consistent shortfall is the concurrent-tool-calls prompt, where
  feature-on answers covered fewer of the rubric facts in both runs.
- **The fixed cost is real.** The feature adds the four retrieval tool schemas
  (about 3.5k characters), a policy paragraph in the system prompt (about 800
  characters) and, on a first prompt nothing matched, the entrypoint card
  (about 680 characters). On a one-request poem prompt that was roughly +1.7k
  tokens on a 7k request, and the schemas and policy repeat on every request.
  The earlier "no card" prompts were not card-free: all 12 received the
  fallback card.
- **The value came from cards, not search.** Across the 35 feature-on runs the
  model called `repo_knowledge_get` 48 times and `repo_knowledge_query` 9 times
  (7 of them on one prompt); it never called `list` or `stats`.
- One model, one corpus of five notes, read-only questions. Nothing was
  measured on an implementation task.

## Model-backed comparison (`trials-plan` matrix): not run

`eval trials-plan` lists the matrix and `eval trials-report` summarizes a JSON
trial file (model, configuration, tool calls, tokens, first relevant
inspection, judged correctness, authoring/maintenance effort). Cells with no
records are reported `not_run`, a comparison is `conclusive` only when both
cells are complete, fully judged and ran with one model and configuration, and a
drop in correctness is flagged even if tool calls fell. **The matrix above has not been run
and there is no driver in this change that writes the trial file** (the live
comparison above is a separate, smaller experiment); every cell is therefore `not_run`, and the
exhaustive arm is untested. Nothing here supports or argues against exhaustive
per-file documentation, and no repository-wide inventory is recommended or
enabled.
