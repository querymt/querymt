# Spec Delta

## Purpose

Expose bounded, citable repository knowledge through explicit tools and selective automatic context delivery, preserving legacy behavior while helping agents both discover relevant files and understand files they index.

## ADDED Requirements

### Requirement: Repository selection is explicit and backward compatible

The `knowledge_query`, `knowledge_list`, and `knowledge_stats` tools SHALL accept `collection: "repository"`. Omitting collection SHALL preserve existing session/scoped behavior, including authorization, parameters, and the unconsolidated-entry list. Repository mode SHALL reject a simultaneous raw `scope` and resolve access from the effective authorized workspace, not a caller-supplied scope string. Repository query SHALL retain the required `question` parameter and reject explicitly supplied legacy-only `retrieval_mode` and `include_consolidations` parameters with an explanation.

#### Scenario: Existing caller is unchanged
- **WHEN** a caller invokes a knowledge tool without collection using previously valid arguments
- **THEN** its existing session or explicitly authorized scope semantics remain unchanged

#### Scenario: Ambiguous repository scope
- **WHEN** a caller supplies both `collection: "repository"` and `scope`
- **THEN** the tool rejects the request rather than guessing a collection or bypassing scope checks

#### Scenario: Repository service is unavailable
- **WHEN** repository mode is requested without an enabled and authorized effective workspace
- **THEN** the tool returns an actionable unavailable or permission diagnostic
- **AND** it does not silently search session memory or another checkout

### Requirement: Queries distinguish navigation from documentation

Repository query SHALL support `navigation`, `documentation`, and default `all` lanes with an optional relative path filter. Navigation SHALL prioritize exact paths and symbol identifiers over incidental prose mentions. Documentation SHALL retrieve authored section content using the question text. The combined lane SHALL deduplicate results. Equivalent inputs and indexed contents SHALL have deterministic tie ordering. Queries SHALL NOT require a model, embedding service, or network request.

#### Scenario: Identifier lookup
- **WHEN** a navigation query exactly names an indexed qualified symbol also mentioned in a long document
- **THEN** the corresponding navigation record ranks ahead of incidental document mentions

#### Scenario: Conceptual lookup
- **WHEN** a documentation query matches an architecture explanation
- **THEN** its relevant authored sections can be returned without requiring a symbol-name match

#### Scenario: Query contains punctuation
- **WHEN** a question includes quotes, punctuation, or text resembling search operators
- **THEN** it is safely treated as question text rather than arbitrary query-language input
- **AND** empty or unsupported input yields a clear diagnostic instead of a storage error

### Requirement: Results carry provenance and bounded citations

Repository query results SHALL identify record kind, authored or source-derived provenance, relative source path, original line range, section or symbol selector, retrievable ID, bounded excerpt, and freshness/evidence status. Default result count SHALL be 10 and the repository maximum SHALL be 50. Serialized output SHALL obey a default 16 KiB budget and a host-controlled maximum no greater than 64 KiB. Truncation, incomplete coverage, and unavailable sources SHALL be disclosed with narrowing or follow-up guidance.

#### Scenario: Large query result
- **WHEN** matching results exceed count or byte budgets
- **THEN** the response stays within the applicable bounds
- **AND** reports that it is truncated rather than implying all matches were returned

#### Scenario: Generated and authored hits overlap
- **WHEN** results include a generated symbol record and an authored explanation
- **THEN** their provenance is distinguishable and each cites its actual source location

### Requirement: Full sections can be fetched without silent omission

A repository-only `knowledge_get` tool SHALL fetch current content by retrievable ID or relative path with an optional section selector. It SHALL provide bounded pagination, original source coordinates, and continuation information. A path-only request for a multi-section document SHALL return its section catalog and preamble rather than silently claiming one arbitrary section is the whole document. IDs SHALL be resolved only in the current authorized checkout, and stale or ambiguous selectors SHALL fail explicitly.

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

Repository list SHALL provide a paginated catalog of active documents or navigation records rather than unconsolidated private entries. Repository stats SHALL report active source and chunk counts, index format/generation, coverage limits, and freshness/diagnostic counts. Pagination SHALL disclose a changed generation instead of silently mixing pages from different generations.

#### Scenario: Inspect corpus availability
- **WHEN** a caller lists and inspects stats for the repository collection
- **THEN** it can distinguish indexed sources from skipped, stale-evidence, invalid, or unavailable sources
- **AND** no private session entry counts or contents are included

### Requirement: Retrieval guidance is capability-gated and concise

When repository knowledge is enabled and query/get tools are allowed, the runtime SHALL provide one compact, stable policy directing unfamiliar-subsystem discovery toward repository retrieval, explaining advisory automatic context, relevant-section fetch, source verification before edits, and fallback to ordinary source tools. It SHALL NOT inject the corpus, require a KB call before every targeted read, rewrite the system prompt with changing cards, or instruct use of disabled tools. Explicit tool allowlists SHALL remain authoritative. Separately host-authorized automatic delivery SHALL NOT grant additional tools and SHALL use relative citations without unavailable-tool instructions when get is excluded. Repository knowledge text SHALL be treated as project evidence, not a source of higher-priority permissions or instructions.

#### Scenario: Opt-in coding session
- **WHEN** an enabled session has usable repository query/get tools
- **THEN** it receives concise retrieval guidance and effective-workspace availability information
- **AND** repeated turns do not accumulate duplicate policy parts

#### Scenario: Tools are excluded
- **WHEN** explicit configuration excludes required retrieval tools
- **THEN** the runtime preserves the allowlist and omits unusable retrieval instructions
- **AND** reports the configuration mismatch
- **AND** any separately enabled automatic delivery uses source citations rather than instructing calls to unavailable tools

#### Scenario: Retrieved document contains instructions
- **WHEN** an indexed AGENTS document or note tells the agent to override permissions or publish session history
- **THEN** retrieval does not execute those instructions or elevate them to system authority
- **AND** this capability does not alter existing directory-instruction precedence

### Requirement: Orientation triggers work before a source path is known

The system SHALL offer a separately host-enabled user-prompt orientation trigger that selects a bounded set of repository context cards before the turn's first model request, without requiring a source path, prior index call, or model-issued retrieval call. Selection SHALL use deterministic local retrieval with an explicit relevance gate and citations, not an LLM-generated summary or unconstrained trigger extraction. Low-relevance queries SHALL produce no search-derived card. An explicitly configured, selected repository entrypoint SHALL be eligible as a bounded fallback on the first task or after compaction; absent entrypoint configuration SHALL NOT cause automatic whole-repository injection. Cards SHALL identify candidate files/subsystems or navigable repository-map sections, not assert a mandatory edit target.

#### Scenario: Unfamiliar feature request has no filenames
- **WHEN** an accepted user prompt describes a feature without paths or symbols and matching repository documentation passes the relevance gate
- **THEN** the agent receives cited candidate subsystem/source pointers before choosing any file to index
- **AND** it need not voluntarily invoke knowledge_query to receive that orientation

#### Scenario: No relevant match exists
- **WHEN** a prompt has no sufficiently relevant repository result and no eligible configured entrypoint fallback
- **THEN** no orientation card is injected and ordinary discovery remains available

#### Scenario: First task uses a configured repository map
- **WHEN** the first task has no gated match and an authorized selected repository-map entrypoint is configured
- **THEN** a bounded cited map pointer can be supplied as general orientation
- **AND** it is not presented as a confident match to a particular implementation file

### Requirement: Index enrichment adds knowledge without changing the outline

The system SHALL offer a separately host-enabled post-tool enrichment stage for successful built-in index calls. It SHALL resolve the actual indexed file using effective tool arguments and root semantics, validate authorized corpus membership, and select directly associated authored/file-purpose or directory knowledge. It SHALL attach a bounded digest as separately attributed hook context with the tool-call ID and relative file path, preserving the original structural result and error status. Digest content SHALL be selected from existing knowledge with citations, fetch identifiers where usable, and evidence state; it SHALL NOT generate semantic descriptions, duplicate the structural outline, or infer applicability from fuzzy filename similarity.

#### Scenario: Indexed file has associated knowledge
- **WHEN** a successful built-in index call targets an authorized indexed file with a matching File/Purpose record or declared knowledge association
- **THEN** the model receives the original outline and a separately labeled, cited knowledge digest in the same result exchange
- **AND** the underlying index output remains byte-for-byte unchanged

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

Orientation and index enrichment SHALL use the same repository collection, authorization, source revalidation, and evidence states as explicit retrieval. Each SHALL default off and require host enablement. Automatic delivery SHALL be non-blocking, bounded in time, and SHALL NOT invoke model-visible tools recursively. It SHALL share a default per-turn maximum of two cards, 600 estimated tokens, and a 4 KiB serialized ceiling including attribution. Host overrides SHALL remain bounded. Selection and budgeting across parallel results SHALL have stable ordering. Private session knowledge and arbitrary repository instructions SHALL NOT be introduced through the delivery path.

#### Scenario: Parallel index calls compete for context budget
- **WHEN** multiple index calls have relevant cards in the same turn as an orientation contribution
- **THEN** all contributions share one turn budget and stable ordering rather than receiving independent full allowances
- **AND** the combined tool-call/result protocol remains valid

#### Scenario: Associated evidence has changed
- **WHEN** an eligible card's supporting source differs from its acknowledged evidence
- **THEN** delivered content identifies changed evidence and does not present the claim as verified
- **AND** missing evidence is conveyed only as a cautionary pointer rather than a current invariant

#### Scenario: Repository attempts to enable execution
- **WHEN** repository metadata requests a handler, command, or permission not enabled by the host
- **THEN** no such execution or permission is enabled by indexing or delivery

### Requirement: Delivery is deduplicated and compaction aware

The system SHALL deduplicate delivered cards across automatic channels by session, checkout, card/source/evidence version, and effective compaction epoch. Selection alone SHALL NOT count as delivery. Provider retries SHALL reuse prepared context without firing delivery again. Resume SHALL recover delivery state from effective persisted provenance or equivalent durable state. After compaction, the system SHALL re-evaluate only a bounded set of still-relevant cards against current sources rather than unconditionally replay old text. Local delivery statistics SHALL distinguish matching, actual delivery, suppression, and fetches without treating these as proof the model followed advice.

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
