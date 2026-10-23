# Spec Delta

## Purpose

Expose bounded, citable repository knowledge through dedicated tools and selective automatic context delivery, preserving legacy knowledge tools while helping agents both discover relevant files and understand files they index.

## ADDED Requirements

### Requirement: Repository knowledge uses dedicated tools

The system SHALL expose repository knowledge through `repo_knowledge_query`, `repo_knowledge_get`, `repo_knowledge_list`, and `repo_knowledge_stats`. The existing `knowledge_query`, `knowledge_list`, `knowledge_stats`, ingest, and consolidation tools SHALL keep their schemas and behavior unchanged and SHALL NOT return repository collection results.

#### Scenario: Existing caller is unchanged
- **WHEN** a caller invokes an existing knowledge tool with previously valid arguments while repository knowledge is enabled
- **THEN** its schema, session or authorized scope semantics, and results are unchanged

#### Scenario: Profile allowlists only repository tools
- **WHEN** a profile allows the repository knowledge tools but not the session knowledge tools
- **THEN** repository retrieval is usable and session memory tools remain unavailable

### Requirement: Repository access is bound to the effective checkout

Repository tools SHALL resolve access from the effective authorized session checkout on every operation. They SHALL NOT accept a scope, checkout, or root parameter that selects a different collection.

#### Scenario: Repository service is unavailable
- **WHEN** a repository tool is invoked without an enabled and authorized effective workspace
- **THEN** the tool returns an actionable unavailable or permission diagnostic
- **AND** it does not search session memory or another checkout

### Requirement: Queries distinguish navigation from documentation

`repo_knowledge_query` SHALL require `question` and support `navigation`, `documentation`, and default `all` lanes with an optional relative path filter and limit. Navigation SHALL prioritize exact paths and symbol identifiers over incidental prose mentions. Documentation SHALL retrieve authored section content. The combined lane SHALL deduplicate results with deterministic tie ordering, without a model, embedding service, or network request.

#### Scenario: Identifier lookup
- **WHEN** a navigation query exactly names an indexed path or qualified symbol also mentioned in a long document
- **THEN** the corresponding navigation record ranks ahead of incidental document mentions

#### Scenario: Conceptual lookup
- **WHEN** a documentation query matches an architecture explanation
- **THEN** its relevant authored sections can be returned without requiring a symbol-name match

#### Scenario: Query contains punctuation
- **WHEN** a question includes quotes, punctuation, or text resembling search operators
- **THEN** it is safely treated as question text rather than query-language input
- **AND** empty input yields a clear diagnostic instead of a storage error

### Requirement: Results carry provenance and bounded citations

Repository query results SHALL identify record kind, authored or source-derived provenance, relative source path, original line range, section or symbol selector, retrievable ID, and bounded excerpt.

#### Scenario: Generated and authored hits overlap
- **WHEN** results include a generated navigation record and an authored explanation
- **THEN** their provenance is distinguishable and each cites its actual source location

### Requirement: Repository responses are bounded

Repository query SHALL default to 10 results with a maximum of 50. Serialized repository tool output SHALL obey a default 16 KiB budget and a host-controlled maximum no greater than 64 KiB. Truncation SHALL be UTF-8 safe. Truncation, incomplete coverage, and unavailable sources SHALL be disclosed with narrowing or follow-up guidance.

#### Scenario: Large query result
- **WHEN** matching results exceed count or byte budgets
- **THEN** the response stays within the applicable bounds
- **AND** reports that it is truncated rather than implying all matches were returned

### Requirement: Results report state signals separately

Authored results SHALL expose index currency, evidence state, and lifecycle status as separate fields. A lifecycle status SHALL NOT certify current correctness or clear evidence drift. Draft and deprecated documents SHALL remain explicitly retrievable with their lifecycle label.

#### Scenario: Deprecated document has unchanged evidence
- **WHEN** an explicitly requested document is deprecated and its acknowledged evidence is unchanged
- **THEN** it is returned with both the deprecated label and unchanged evidence state

### Requirement: Full sections can be fetched without silent omission

`repo_knowledge_get` SHALL fetch current content by retrievable ID or relative path with an optional section selector. It SHALL provide bounded pagination, original source coordinates, and continuation information. A path-only request for a multi-section document SHALL return its section catalog and preamble rather than presenting one section as the whole document. IDs SHALL be resolved only in the current authorized checkout, and stale or ambiguous selectors SHALL fail explicitly.

#### Scenario: Fetch matching architecture section
- **WHEN** a caller fetches a section ID returned by repository query
- **THEN** it receives that section's body, not only its title or summary
- **AND** any omitted bytes have explicit continuation information

#### Scenario: Fetch a document without a section
- **WHEN** a path-only request addresses a document containing multiple sections
- **THEN** the response identifies the available sections and how to fetch them
- **AND** it does not describe a single section as the entire document

#### Scenario: Source changes after query
- **WHEN** a queried section is removed or renamed before fetch
- **THEN** fetch reports the stale selector or current catalog
- **AND** does not substitute an unrelated body

#### Scenario: Cross-checkout or unsafe fetch
- **WHEN** a request attempts to fetch another checkout's record or a path outside the authorized root
- **THEN** access is rejected without disclosing that source's contents

### Requirement: Repository catalog and statistics describe coverage

`repo_knowledge_list` SHALL provide a paginated catalog of active documents or navigation records. `repo_knowledge_stats` SHALL report active source and chunk counts, index format/generation, coverage limits, and freshness/diagnostic counts. Pagination SHALL disclose a changed generation instead of silently mixing pages from different generations.

#### Scenario: Inspect corpus availability
- **WHEN** a caller lists and inspects stats for the repository collection
- **THEN** it can distinguish indexed sources from skipped, changed-evidence, invalid, or unavailable sources
- **AND** no private session entry counts or contents are included

### Requirement: Catalogs support progressive disclosure

Repository catalogs SHALL expose bounded linked listings with authored titles/descriptions where present. Missing directory listings SHALL be synthesizable read-only from selected documents. Authored listings SHALL remain citable and unchanged; synthetic listings SHALL be labeled derived and cite their source documents. Synthesis SHALL NOT enable automatic orientation.

#### Scenario: Browse a curated subsystem
- **WHEN** a caller opens a selected authored index or requests a directory catalog
- **THEN** it receives bounded concept/subdirectory pointers and descriptions with continuation guidance
- **AND** can fetch relevant documents without loading the whole corpus

#### Scenario: Directory has no authored listing
- **WHEN** selected documents have titles or descriptions but no authored listing
- **THEN** a derived catalog can be returned without repository writes or invented descriptions
- **AND** only selected authorized sources appear

### Requirement: Retrieval guidance is capability-gated and stable

When repository knowledge is enabled and `repo_knowledge_query` and `repo_knowledge_get` are allowed, the runtime SHALL provide one compact, stable policy describing repository retrieval, advisory automatic context, relevant-section fetch, source verification before edits, and fallback to ordinary source tools. The policy SHALL NOT require a knowledge call before every targeted read and SHALL NOT change between turns.

#### Scenario: Opt-in coding session
- **WHEN** an enabled session has usable repository query/get tools
- **THEN** it receives concise retrieval guidance and effective-workspace availability information
- **AND** repeated turns do not accumulate duplicate policy parts

### Requirement: Tool allowlists remain authoritative

Explicit tool allowlists SHALL remain authoritative over repository knowledge configuration. Guidance SHALL NOT instruct use of disabled tools, and automatic delivery SHALL NOT grant tools. When get is excluded, delivered content SHALL use relative citations instead of tool-call instructions.

#### Scenario: Tools are excluded
- **WHEN** explicit configuration excludes the repository retrieval tools
- **THEN** the runtime preserves the allowlist and omits retrieval instructions
- **AND** reports the configuration mismatch
- **AND** any separately enabled automatic delivery uses source citations rather than instructing calls to unavailable tools

### Requirement: Repository text is evidence, not authority

Repository knowledge text SHALL be treated as project evidence. It SHALL NOT be placed in the system prompt, grant permissions, or alter existing directory-instruction precedence.

#### Scenario: Retrieved document contains instructions
- **WHEN** an indexed AGENTS document or note tells the agent to override permissions or publish session history
- **THEN** retrieval does not execute those instructions or elevate them to system authority
- **AND** this capability does not alter existing directory-instruction precedence

### Requirement: Orientation triggers work before a source path is known

The system SHALL offer a separately host-enabled user-prompt orientation trigger that selects bounded repository context cards before the turn's first model request, without requiring a source path, prior index call, or model-issued retrieval call. Selection SHALL use deterministic local retrieval with citations, not LLM summaries. Cards SHALL identify candidate files, subsystems, or map sections, not a mandatory edit target.

#### Scenario: Unfamiliar feature request has no filenames
- **WHEN** an accepted user prompt describes a feature without paths or symbols and matching repository documentation passes the relevance gate
- **THEN** the agent receives cited candidate subsystem/source pointers before choosing any file to index
- **AND** it need not voluntarily invoke repo_knowledge_query to receive that orientation

#### Scenario: No relevant match exists
- **WHEN** a prompt has no section passing the relevance gate and no eligible configured entrypoint fallback
- **THEN** no orientation card is injected and ordinary discovery remains available

### Requirement: The orientation relevance gate is specified and versioned

The orientation gate SHALL carry a version. A section SHALL pass when the prompt contains an exact indexed repository-relative path or qualified identifier, or when at least two distinct gate tokens match its title, headings, tags, or description, or at least three match its body. Passing sections SHALL be ranked with documentation-lane ranking and deterministic tie-breaking. Thresholds and word lists SHALL change only with a new gate version.

#### Scenario: Gate is reproducible
- **WHEN** the same prompt and index are evaluated with the same gate version
- **THEN** the same sections pass in the same order

### Requirement: Gate tokenization is fully specified

Gate tokenization SHALL normalize the prompt with Unicode NFKC and lowercase, truncate it to 2 KiB, split on non-alphanumeric characters other than underscore, and additionally split identifiers on `::`, `.`, `/`, `-`, `_`, and camelCase boundaries while keeping the original identifier. It SHALL keep at most 64 distinct tokens and drop tokens shorter than three characters, pure numbers, and tokens in fixed compiled-in stopword and generic-coding-verb lists.

#### Scenario: Identifier is split
- **WHEN** a prompt contains `SessionStore::compact`
- **THEN** tokens include `sessionstore::compact`, `session`, `store`, and `compact`

#### Scenario: Generic prompt
- **WHEN** a prompt contains only stopwords, generic coding verbs, and one domain term
- **THEN** no search-derived card is produced

#### Scenario: Exact path in prompt
- **WHEN** a prompt names an indexed repository-relative path
- **THEN** sections associated with that path pass the gate regardless of other token counts

### Requirement: Configured entrypoints are a bounded fallback

An explicitly configured, selected repository entrypoint SHALL be eligible as a bounded fallback on the first task of a session or after compaction when no section passes the gate. Absent entrypoint configuration SHALL NOT cause automatic repository injection, and synthesized catalogs SHALL NOT serve as fallback.

#### Scenario: First task uses a configured repository map
- **WHEN** the first task has no gated match and an authorized selected repository-map entrypoint is configured
- **THEN** a bounded cited map pointer can be supplied as general orientation
- **AND** it is not presented as a confident match to a particular implementation file

### Requirement: Index enrichment adds knowledge without changing the outline

The system SHALL offer a separately host-enabled post-tool enrichment stage for successful built-in index calls. It SHALL resolve the indexed file using effective tool arguments and root semantics, validate corpus membership, and attach a bounded digest as separately attributed hook context with the tool-call ID and relative path. The original structural result and error status SHALL be preserved byte-for-byte.

#### Scenario: Indexed file has associated knowledge
- **WHEN** a successful built-in index call targets an authorized indexed file with a matching File/Purpose record or declared association
- **THEN** the model receives the original outline and a separately labeled, cited knowledge digest in the same result exchange
- **AND** the underlying index output remains byte-for-byte unchanged

### Requirement: Enrichment selects only directly associated knowledge

The digest SHALL be selected from File/Purpose rows, documents whose `knowledge_sources` name the file, and `knowledge_paths` associations, with citations and evidence state. It SHALL NOT generate descriptions, duplicate the outline's signatures, or infer applicability from fuzzy filename similarity or incidental mentions.

#### Scenario: Same basename elsewhere
- **WHEN** a note is associated with `a/mod.rs` and the index call targets `b/mod.rs`
- **THEN** that note is not attached

#### Scenario: Explicit root differs from session cwd
- **WHEN** an index call uses a supported root/path combination different from simple cwd-relative resolution
- **THEN** enrichment resolves the same canonical file as the tool and checks that file against authorized repository coverage
- **AND** it does not attach knowledge for a same-named file elsewhere

#### Scenario: File or execution is ineligible
- **WHEN** index fails, permission is denied, the file is outside the selected corpus, or a different tool merely shares the name index
- **THEN** automatic enrichment does not attach file knowledge

#### Scenario: Knowledge service fails or output is suppressed
- **WHEN** enrichment encounters an unavailable index, selector error, timeout, or output-policy suppression
- **THEN** it does not turn successful tool execution into a failure or restore restricted information
- **AND** it leaves the accepted tool result unchanged and records an appropriate host diagnostic

### Requirement: Automatic delivery shares bounds and source trust rules

Orientation and index enrichment SHALL use the same repository collection, authorization, source revalidation, and evidence states as explicit retrieval. Each SHALL default off and require host enablement. Automatic delivery SHALL be non-blocking, bounded in time, and SHALL NOT invoke model-visible tools recursively. Draft and deprecated documents SHALL NOT become automatic cards.

#### Scenario: Associated evidence has changed
- **WHEN** an eligible card's supporting source differs from its acknowledged evidence
- **THEN** delivered content identifies changed evidence and does not present the claim as verified
- **AND** missing evidence is conveyed only as a cautionary pointer rather than a current invariant

#### Scenario: Repository attempts to enable execution
- **WHEN** repository metadata requests a handler, command, or permission not enabled by the host
- **THEN** no such execution or permission is enabled by indexing or delivery

### Requirement: Automatic delivery has a shared per-turn budget

Both delivery stages SHALL share a default per-turn maximum of two cards, 600 estimated tokens, and a 4 KiB serialized ceiling including attribution. Host overrides SHALL be bounded by a fixed maximum. Allocation across parallel results SHALL follow stable tool-call order. A card that does not fit SHALL be omitted and not marked delivered.

#### Scenario: Parallel index calls compete for context budget
- **WHEN** multiple index calls have relevant cards in the same turn as an orientation contribution
- **THEN** all contributions share one turn budget and stable ordering rather than receiving independent full allowances
- **AND** the combined tool-call/result protocol remains valid

### Requirement: Delivery is deduplicated and compaction aware

The system SHALL deduplicate delivered cards across automatic channels by session, checkout, card/source/evidence version, and compaction epoch. Selection alone SHALL NOT count as delivery. Provider retries SHALL reuse prepared context without firing delivery again. Resume SHALL recover delivery state from persisted contribution provenance. After compaction, only a bounded set of still-relevant cards SHALL be re-evaluated against current sources.

#### Scenario: Orientation and index select the same note
- **WHEN** index enrichment selects a card version already delivered by orientation and still represented in effective context
- **THEN** the full card is not delivered again

#### Scenario: Retry or resume
- **WHEN** a provider retries a prepared request or the session resumes with existing card contributions
- **THEN** unchanged cards are not reinjected solely because execution retried or a disposable diagnostics cache was lost

#### Scenario: Compaction removes useful context
- **WHEN** a recent relevant card is no longer retained after compaction
- **THEN** it can be re-evaluated and redelivered under the new epoch and shared budget
- **AND** removed or changed repository content is not resurrected from the old payload

### Requirement: Delivery statistics are local and non-inferential

The system SHALL record local matched, delivered, suppressed, and fetched counts with reasons, added tokens, and latency. Statistics and raw prompts SHALL NOT be written into repository files. Delivery or fetch SHALL NOT be reported as evidence that the model followed the advice.

#### Scenario: Card is delivered and fetched
- **WHEN** a delivered card's section is later fetched
- **THEN** statistics record delivery and fetch as separate events without a "followed" outcome
