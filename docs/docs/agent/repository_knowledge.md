# QueryMT Agent - Repository Knowledge

Repository knowledge lets an agent search and read **authored documentation that lives in the repository** (architecture notes, directory `AGENTS.md` files, `.agents/memories/*.md`) through four dedicated, read-only tools. The Markdown stays where it is and stays readable by any tool. QueryMT only builds a private, disposable search index from it.

It is **off by default** and separate from session memory (`knowledge_*` tools), which is unchanged.

## Enabling

Only host configuration can enable the feature. A repository file can select *what* is indexed, but can never enable the feature, grant tools, or widen filesystem access.

```toml
[agent.repository_knowledge]
enabled = true
# root = "."                   # explicit root; required for non-Git workspaces
# allowed_roots = ["/srv/repos"]
# max_files = 2000
# max_file_bytes = 524288
# max_total_bytes = 33554432
# max_response_bytes = 16384   # capped at 64 KiB
# default_result_limit = 10    # capped at 50
# force_hash = false           # hash every selected file on every refresh
# allow_ignored = false        # include Git-ignored files matched by the manifest
```

The same table exists under `[quorum.repository_knowledge]`. When tool allowlists are used, add the tools you want: `repo_knowledge_query`, `repo_knowledge_get`, `repo_knowledge_list`, `repo_knowledge_stats`. An allowlist is never widened for you.

## Selecting documents: `.qmt/knowledge.toml`

```toml
version = 1

[documents]
include = [".agents/memories/*.md", "docs/architecture/**/*.md", "crates/agent/**/AGENTS.md"]
exclude = ["openspec/changes/archive/**"]

[code]
include = ["crates/agent/src/**/*.rs"]
```

- Patterns are relative to the knowledge root; `exclude` wins over `include`.
- No manifest means "not configured". Nothing is crawled and nothing is created.
- An unsupported `version` or an unknown top-level table makes indexing unavailable with a diagnostic, so a future format is never half-interpreted.
- Selected `[code]` files become **source-derived navigation records**: always a path record, plus declarations (kind, qualified name, signature, line range) when the language has an extractor. See [Source-derived navigation](#source-derived-navigation-and-maps). Nothing about their meaning is generated.

### Why `.qmt/` and not `.agents/`

The manifest is meaningful only to QueryMT. The shared dotagents convention standardizes `.agents/AGENTS.md`, `.agents/mcp.json` and `.agents/skills/`; putting QueryMT-only files there would imply portability that does not exist, risk collisions if a shared `knowledge*` file is ever defined, and add noise for other tools that scan `.agents/`. `.qmt/` already holds project-level QueryMT configuration (`.qmt/commands`, `.qmt/skills`).

`.agents/knowledge.toml` is **never read**. Repository knowledge never writes anything under `.agents/`; it may *read* selected `.agents/memories/*.md`, independently of the dotagents protocol loader.

## Document format

Frontmatter is optional. When present it uses the same conventions as `.agents` memories (scalars, quoted values, comma-separated or JSON-array lists). Unknown keys are kept and ignored.

| Key | Meaning |
|-----|---------|
| `id` | Stable identity that survives moves. Without it, identity is the relative path. |
| `title`, `description`, `tags` | Catalog and result text. `description` is never generated for you. |
| `status` | Free-form lifecycle label. `draft` and `deprecated` are recognized. |
| `knowledge_sources` | Evidence dependencies (`path` or `path#qualified::symbol`). |
| `knowledge_paths` | Advisory applicability globs. They never certify freshness. |

Duplicate explicit IDs exclude every conflicting document; malformed frontmatter excludes only that document. Valid siblings stay available and `repo_knowledge_stats` reports coverage as incomplete.

Markdown links resolve relative to their document and are confined to the checkout. They never add index inputs or grant reads, and external URLs are never fetched.

## Pull retrieval

| Tool | Purpose |
|------|---------|
| `repo_knowledge_query` | `question`, optional `lane` (`navigation`, `documentation`, `all`), `path`, `limit` (default 10, max 50). Lexical ranking, no model or network. |
| `repo_knowledge_get` | Fetch by `id`, or by `path` plus an optional heading `section`. A multi-section document returns its table of contents and preamble, never one arbitrary section. Long sections page with a `cursor`. |
| `repo_knowledge_list` | Paginated catalog. `view: directory` browses one directory. |
| `repo_knowledge_stats` | Sources, documents, sections, coverage and diagnostic counts. |

The tools take no scope, root, or checkout argument. Access always comes from the session's effective checkout, and IDs from another checkout simply do not resolve.

### Results

Each result cites a relative path and line range, an ID for `repo_knowledge_get`, and whether it is **authored** or **source-derived**. Authored results carry three separate signals:

```
index: current · evidence: unverified · lifecycle: deprecated
```

- **index**: the indexed bytes were revalidated against the file before being served.
- **evidence**: whether supporting sources still match an explicit acknowledgement. Until acknowledgements exist this is `unverified`.
- **lifecycle**: the author's `status` label (`usable` when none). It never clears evidence drift. Draft and deprecated documents remain explicitly retrievable.

Output is bounded: 16 KiB by default, host-capped at 64 KiB. Truncation is UTF-8 safe and always disclosed, with advice on how to narrow the request.

### Directory catalogs

`repo_knowledge_list` with `view: directory` and a `path` shows one directory:

- **Authored**: if the directory has a selected `index.md` (or `README.md`) that links to selected files, the catalog follows its link order and cites the listing's lines. The listing is never modified.
- **Derived**: otherwise the catalog is synthesized in memory from selected documents' titles and descriptions, labelled *derived*, with each entry citing its document. Missing descriptions stay missing.

Only selected sources appear. Links that resolve outside the corpus are counted, not named. Producing a catalog writes nothing and enables nothing.

## Source-derived navigation and maps

Repository knowledge distinguishes three kinds of content, and results always say which one they are:

| Kind | Where it comes from | Examples |
|------|---------------------|----------|
| **Source-derived** | Extracted from selected `[code]` files by the same tree-sitter extractors the `index` tool uses. Deterministic, no model. | Paths, language, declaration kind, qualified name, signature, parent, line range, digest, syntactic `use`/`import` text, verbatim Rust `///` and `//!` documentation |
| **Authored** | Written by a person in a selected document. | Document sections, `description`, File/Purpose table rows |
| **Private** | Session and global memory. | Never part of repository knowledge |

Source-derived records are *syntax, not semantics*. Imports are the declaration text as written, never resolved to a target; no purpose, invariant, call edge or owner is inferred from a name, a path or a model. Rust documentation comments are copied verbatim with their source coordinates and labelled as source documentation, not as a derived purpose. A file with no extractor, invalid UTF-8, or a parse failure stays visible as a **path-only** record with a coverage diagnostic; it is never dropped or described as indexed.

Extraction is cached per file by content and extractor version: an edit re-parses only that file, an extractor change re-extracts code files only, and deleted files and symbols disappear on the next refresh. Generated records never acknowledge an authored description as reviewed, and they never create per-file or per-symbol documents. The `index`, `get_symbol` and edit-precondition behavior is unchanged, and a map is a navigation aid, not a substitute for reading current source before editing.

### File/Purpose tables

A document may contain a two-column table to say what files are for. It is optional; no exhaustive table, per-file document or per-directory table is required.

```markdown
| File       | Purpose                       |
|------------|-------------------------------|
| `queue.rs` | Holds pending work.           |
| `worker.rs`|                               |
```

Each row is a citable **authored** record (`document:line`). Paths resolve relative to the containing document and are confined to the checkout. An empty purpose stays empty. Tables in fenced code blocks are examples, and rows inside a `<!-- qmt:inventory:begin -->` / `<!-- qmt:inventory:end -->` block are parsed identically.

### Maps

`repo_knowledge_list` with `view: map` (and an optional `path`) renders a bounded map: directories in path order, each selected file with its declarations and any cited authored purpose. Maps are pure functions of the selected sources: stable ordering, no timestamps, generation numbers or machine-local paths. Over-budget maps list whole files only and end with a footer that counts what was omitted and names narrower paths to ask for.

### Exporting a map

Retrieval and indexing never write to the repository. To commit a map, plan and then apply an explicit export (`RepositoryService::plan_export` / `apply_export`):

1. **Plan** renders the map and computes the complete resulting file without touching the target. The plan records the target's content hash (or its absence) as a *preimage*.
2. **Apply** re-reads the target and refuses with `export_stale` if it no longer matches the preimage. Otherwise it replaces the file atomically (temporary sibling file plus rename).

The map always lives between marker lines:

```markdown
<!-- qmt:map:begin -->
…generated map…
<!-- qmt:map:end -->
```

- A **new or empty** target becomes a file holding just the block (a generated artifact).
- An **existing** target must contain exactly one block outside code fences; only that block is replaced and every other byte, including line endings, is preserved. Otherwise the export is refused (`export_target_unmanaged` or `export_target_ambiguous`) with advice to export to a new file or add the two markers where the map belongs. Authored prose is never overwritten.
- Targets are repository-relative and never under `.git/` or `.agents/`; symlinked targets or parent directories are refused.
- Exports are byte-identical for identical sources and extractor version in any checkout path, and exporting unchanged sources is a no-op.
- Generation is offline: no model, credentials, network or session history. Exports from a truncated map include the truncation footer, so narrow with `scope` to export a complete map per directory.

Keep exported maps out of root `AGENTS.md`; they would inflate every tool's instruction context. A dedicated generated file, linked from your documentation, is usually better.

## Evidence and maintenance

Two different questions are tracked separately. **Index freshness** asks whether the indexed bytes match the file. **Evidence** asks whether the source an authored statement relies on still matches what a person last *explicitly acknowledged*. Equal hashes never prove a statement true, and lifecycle `status` (`draft`, `deprecated`, anything else) never changes an evidence state.

### Evidence states

| State | Meaning |
|---|---|
| `unverified` | Nothing acknowledged, or the note or its declared dependency set changed since it was |
| `unchanged` | The note and every dependency match the acknowledgement |
| `changed` | A dependency's bytes differ from the acknowledgement (not automatically false) |
| `missing` | A dependency is gone, or its selector can no longer be resolved unambiguously |

A document reports its most alarming record. Missing or ambiguous selectors are never replaced by a similarly named file or symbol.

### What a record depends on

A document is a set of acknowledgeable *records*: each **section** (dependencies: the document's `knowledge_sources`) and each **File/Purpose row** (dependency: the file the row names). Markdown links, URLs and `knowledge_paths` are never evidence dependencies. A `knowledge_sources` selector is `path` (whole file) or `path#qualified::symbol` (a symbol, hashed with the extractor's digest; ambiguous symbols are unresolved).

### Evidence files

One Git-trackable file per acknowledged document, mirroring its path:

```text
.qmt/knowledge/evidence/docs/scheduler.md.json
```

```json
{
  "document": "docs/scheduler.md",
  "entries": {
    "section:[\"Scheduler\"]": {
      "dependencies": { "src/sched/queue.rs": "sha256-v1:e768…" },
      "text": "sha256-v1:1252…"
    }
  },
  "version": 1
}
```

Keys are sorted and the file ends with a newline. It holds only repository-relative paths and hashes: no absolute paths, session IDs, timestamps or private text. Branches that acknowledge different notes touch different files and merge without conflict. Moving a document moves its evidence file; until then `check` reports the old file as orphaned and the moved note is `unverified`.

### Hash normalization (`sha256-v1`)

Text hashes are taken after stripping a UTF-8 BOM, converting CRLF and lone CR to LF, and removing trailing whitespace on each line. A CRLF checkout or an editor that trims whitespace therefore causes no false drift; a substantive edit does. Normalization is part of the hash version.

### Maintenance commands

`crates/agent/examples/repository_knowledge.rs` needs no model, credentials or network. The cache lives under the QueryMT config directory (`--config-dir` overrides it), never in the repository.

```bash
cargo run -p querymt-agent --example repository_knowledge -- --root . <command>
```

| Command | Effect |
|---|---|
| `refresh` | Reconcile the local cache and report coverage |
| `map [--path P] [--output F [--apply]]` | Print a bounded map, or preview/write an export into F's `qmt:map` block |
| `check [--exhaustive DIR]... [--json]` | Read-only diagnostics; exits **1** on any warning or error |
| `scaffold DIR [--file-name N] [--apply]` | Preview/write empty-purpose inventory rows |
| `acknowledge DOC (--record KEY... \| --whole-document) [--apply]` | Preview/write evidence for the selected records |

Exit status: `0` success, `1` the check found problems, `2` the operation could not run or was refused. Writes happen only with `--apply`, never under `.agents/` or `.git/`, and no Git commit is ever made.

### `check`

Reports, with stable codes and repository-relative paths: malformed or conflicting documents and incomplete coverage (including budgets); broken or unsafe relative links; File/Purpose rows naming missing files; invalid `knowledge_sources`/`knowledge_paths`; evidence drift, stale evidence entries and orphaned evidence files; and unresolvable manifest `[orientation]` entrypoints. It never rewrites content, prunes rows, makes network requests or guesses renames.

Curated corpora pass: missing listings, missing per-file rows, unknown frontmatter keys and never-acknowledged notes are not findings. Per-file rows are required only for directories you name with `--exhaustive`. JSON output contains no generation numbers, timestamps or machine-local paths, so equal repositories give identical output:

```json
{ "ok": false, "status": "configured",
  "counts": { "error": 0, "warning": 1, "info": 0 },
  "findings": [ { "code": "evidence_changed", "severity": "warning",
                  "path": "docs/scheduler.md",
                  "message": "line 4: record section:[\"Scheduler\"] relies on source that changed since acknowledgement" } ] }
```

### Scaffolding inventories

`scaffold` proposes File/Purpose rows for selected code files in one **non-root** directory that no table lists yet. Rows are sorted, purposes are left **empty** for a person to write, and rerunning proposes nothing new. It writes in exactly two places: a *new* directory document (default `AGENTS.md`), or inside the single managed block of an existing one:

```markdown
<!-- qmt:inventory:begin -->
| File | Purpose |
| --- | --- |
| `clock.rs` | |
| `queue.rs` | |
<!-- qmt:inventory:end -->
```

An existing file without exactly one block (or whose block holds anything but the table) is left **byte-for-byte unchanged** and a finding explains why. Authored purposes, orphan rows and all prose are preserved. Blocks never contain instructions; keep inventories out of root `AGENTS.md`.

### Plan, then apply

Every write is two steps. The preview records the target's preimage; `--apply` re-derives the plan from the repository *as it is now* and refuses a stale one (`maintenance_stale`), so newer, unseen contents are never certified or overwritten. `acknowledge` has no repository-wide "accept all drift" form: you must name records or choose one whole document.

Example session (run against a temporary fixture repository):

```text
$ ... acknowledge docs/scheduler.md
error [request_invalid]: no records selected; name the records to acknowledge or choose the whole document explicitly
$ ... acknowledge docs/scheduler.md --whole-document
preview: create 1 record(s) -> .qmt/knowledge/evidence/docs/scheduler.md.json
$ ... acknowledge docs/scheduler.md --whole-document --apply
applied create 1 record(s) -> .qmt/knowledge/evidence/docs/scheduler.md.json
$ ... check            # exit 0
ok: no findings
$ echo 'pub fn run() { work(); }' > src/sched/queue.rs
$ ... check --json     # exit 1: evidence_changed
```

## Freshness, cache and recovery

The index lives at `<config_dir>/cache/knowledge/<checkout-key>/index.sqlite` (default config directory `~/.qmt`). It is never written into the repository or `sessions.db`, and the key never appears in citations or IDs.

Before each retrieval QueryMT compares file size, modification time and file identity. Content is hashed only when those differ, when the modification time is within a racy-timestamp window of the last index, or when metadata is unreliable. Only changed content is re-parsed. Edits made outside QueryMT are picked up on the next call without any watcher. `force_hash = true` hashes every selected file every time.

Linked worktrees are separate checkouts with separate indexes. Starting a session in `crates/agent` resolves the same checkout as starting at the repository root.

Each refresh publishes one complete generation; readers see a whole generation or an explicit unavailable/updating message, never a partial one. If a source becomes invalid its old body is not served as current.

**Recovery:** the cache can always be deleted. It is rebuilt from the selected files alone, and neither your Markdown nor private session memory is touched. **Rollback:** set `enabled = false` (and optionally delete the cache directory); committed Markdown and `.qmt/` files remain readable without QueryMT.

## Automatic delivery (optional)

Pull retrieval needs no further setup. Two automatic stages can additionally push short, cited cards into the conversation. Each is a separate host setting, defaults to off, and neither enables the other, the feature itself, or any tool:

```toml
[agent.repository_knowledge.delivery]
orientation   = true   # cards at an accepted user prompt, before the first model request
index_context = true   # cards after a successful built-in `index` call
max_cards  = 2         # per turn, shared by both stages (cap 4)
max_tokens = 600       # estimated, per turn (cap 2000)
max_bytes  = 4096      # serialized ceiling including attribution (cap 16 KiB)
```

Repository files cannot enable a stage, a handler, a command or a permission.

**Orientation** uses no model. The prompt is normalized (NFKC, lowercase, first 2 KiB), split on non-alphanumerics and then on `::`, `.`, `/`, `-`, `_` and camelCase (the original identifier is kept too), limited to 64 distinct tokens, and stripped of tokens under three characters, pure numbers and the fixed English stopword and generic-coding-verb lists. A section passes the gate (`gate_version = 1`) when the prompt contains an exact indexed path or qualified identifier, or at least two tokens match its title, headings, tags or description, or at least three match its body. Passing sections are ranked like the documentation lane. Prompts that are generic or mostly non-English usually pass nothing and receive no card; that is intended. On the first task of a session, and once after compaction, a configured `[orientation] entrypoints` document can supply a general map when nothing passes. Synthesized catalogs are never a fallback.

**Index context** applies only to a successful call of the built-in `index` tool on a file in the selected corpus. It attaches File/Purpose rows, documents whose `knowledge_sources` name the file, and `knowledge_paths` matches, as separately attributed context tied to the tool call. The tool's own output and error flag are never changed. A foreign tool that merely shares the name, a failed or denied call, and any result a hook suppressed or redacted receive nothing.

**Budgets and duplicates.** Both stages draw on one per-turn budget in stable order (orientation first, then calls in tool-call order). A card that does not fit is omitted whole and not counted as delivered. A card version already in the effective history, including after a resume, is not sent again; changed text or evidence is a new version. After compaction, recently relevant cards are re-selected from current sources, never replayed from old text. Drafts and deprecated documents are never automatic cards; missing evidence yields only a cautionary pointer and changed evidence is labeled.

**Fallback.** Selection is time-bounded. If the index is unavailable or slow, the turn and the tool result proceed unchanged and a rate-limited host log line is emitted.

**Retrieval policy.** When the feature is enabled and both `repo_knowledge_query` and `repo_knowledge_get` are usable under the agent's tool policy, a new session receives one fixed, compact system-prompt part (attached once, never per turn). It does not require a lookup before targeted reads. Allowlists stay authoritative: if the tools are excluded, no guidance is added and a warning is logged. Cards then carry relative citations only, with no instruction to call an unavailable tool. A quorum planner with an explicit `tools` list is treated the same way.

**Local diagnostics.** Per process, in memory, the host counts cards matched, delivered and suppressed (with a reason such as `card_limit` or `already_delivered`), added tokens, selection latency, and `repo_knowledge_get` fetches. No prompt text, card text or statistics are written to the repository. Delivered and fetched are separate observations; nothing infers that advice was followed.

Rollback: set the stage flags (or `enabled`) to `false`.

## Example profile and rollback

`crates/agent/examples/confs/repository_knowledge_coder.toml` is an opt-in coder profile. It lists the four `repo_knowledge_*` tools in its allowlist (browse with `repo_knowledge_list`, search with `_query`, fetch with `_get`), enables both delivery stages with the default budgets, and points at the maintenance example for `check`/`refresh`. A configuration test loads it and verifies the allowlist, delivery flags and rollback.

```bash
cargo run --example from_config -- crates/agent/examples/confs/repository_knowledge_coder.toml
```

**Rollback.** Set `[agent.repository_knowledge] enabled = false` (or remove the table). Delivery flags never enable the feature on their own, so both stages stop with it, the repository is no longer read, and prompts return to their previous form. To also discard the disposable index, delete only `<config_dir>/cache/knowledge/<checkout-key>/`. Committed Markdown, `.qmt/` files, `.agents/`, legacy knowledge and session history are unaffected.

## Trust and privacy

- Retrieved text is project evidence, never system authority. It cannot grant permissions or change instruction precedence. This holds for automatic cards too: an `AGENTS.md` row that says "ignore previous instructions" is shown as labeled evidence in attributed context, and tool allow/deny lists and the system prompt are unchanged (covered by the standalone fixture `instructions_inside_repository_evidence_never_gain_authority`).
- Repository retrieval never includes private session entries, consolidations, or global protocol memories, and rebuilding the cache never alters them.
- Ignored files are excluded by default, but explicitly selected files can still contain secrets; review what you select.
