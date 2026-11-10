# Spec Delta

## Purpose

Defines how the llama.cpp provider constrains a local model's output while it writes tool calls: which grammar engine is used, what tool arguments may contain, how fallbacks behave, and how much the constraint may cost per generated token.

## ADDED Requirements

### Requirement: Grammar engine setting
The llama-cpp provider SHALL accept the parameter `grammar_backend` (TOML path `[agent.parameters].grammar_backend` in an agent profile, or the same key in llama-cpp provider parameters). Type: string enum, `"llguidance"` or `"gbnf"`. Default: `"llguidance"`. Any other value SHALL fail provider configuration with an error naming the key.

#### Scenario: Grammar backend defaults to llguidance
- **WHEN** a llama-cpp configuration omits `grammar_backend` and a request with tools is generated
- **THEN** the tool-call grammar is enforced by the llguidance engine

#### Scenario: Grammar backend gbnf selects GBNF
- **WHEN** `grammar_backend = "gbnf"` is configured and a request with tools is generated
- **THEN** the tool-call grammar is enforced by the GBNF engine and llguidance is not used

#### Scenario: Grammar backend rejects unknown value
- **WHEN** `grammar_backend = "fast"` is configured
- **THEN** loading the provider configuration fails with an error that names `grammar_backend`

### Requirement: Grammar coverage of all tool formats
For every tool-call format the provider detects (Qwen3 JSON, Qwen XML-function, Ministral, LFM2, FunctionGemma, Gemma 4), the llguidance engine SHALL provide a grammar. A request in any of these formats SHALL NOT fall back to GBNF only because its format lacks an llguidance grammar.

#### Scenario: Every detected format compiles under llguidance
- **WHEN** a request with tools is prepared for each of the six detected tool formats with `grammar_backend = "llguidance"`
- **THEN** each request is constrained by an llguidance grammar and no fallback is logged

### Requirement: Lazy grammar activation
The tool-call grammar SHALL NOT constrain output until the format's tool-call trigger text has been generated, whether the trigger is one special token or several ordinary tokens. Free text and reasoning before the trigger SHALL be sampled unconstrained. Text of the triggering token that follows the trigger SHALL be checked by the grammar.

#### Scenario: Text before the trigger is unconstrained
- **WHEN** the model generates prose containing `<`, braces and the word `function` before any tool-call trigger
- **THEN** no generated token is masked by the grammar

#### Scenario: Multi-token trigger activates the grammar
- **WHEN** the vocabulary splits `<tool_call>` into several tokens and the model generates them
- **THEN** the next token is constrained by the tool-call grammar

#### Scenario: Special-token trigger activates the grammar
- **WHEN** the trigger is a single special token (Gemma 4 `<|tool_call>`) and the model generates it
- **THEN** the next token is constrained by the tool-call grammar

### Requirement: Text values exclude only their terminator
String parameter values delimited by a terminator (`</parameter>` in Qwen XML-function, `<escape>` in FunctionGemma, `<|"|>` in Gemma 4) SHALL accept any text that does not contain that terminator, including `<`, comparison operators, markup and partial terminators. A value SHALL end only at its terminator. This SHALL hold for both engines.

#### Scenario: Value accepts less-than and markup
- **WHEN** a tool-call value is being generated and the model emits `if a < b:` or `<div></div>` or `</param`
- **THEN** those tokens are allowed by the grammar under both engines

#### Scenario: Value cannot contain its terminator
- **WHEN** a Qwen XML-function value is being generated
- **THEN** the grammar does not allow `</parameter>` to appear inside the value except as the value's closing delimiter

#### Scenario: Special-token terminator closes the value
- **WHEN** a Gemma 4 string value is being generated and the model emits the special token `<|"|>`
- **THEN** the grammar accepts it as the end of the string

### Requirement: Schema-aware tool arguments
When a tool set's schemas compile, the llguidance grammar SHALL restrict each call to a declared tool name and its arguments to that tool's JSON schema: declared parameter names, value types, enum members and required parameters. Undeclared parameters SHALL be allowed only when the schema permits additional properties. This SHALL apply in every format.

#### Scenario: Schema rejects wrong value type
- **WHEN** a tool declares `limit` as an integer and the model starts writing a non-numeric value for `limit`
- **THEN** the grammar does not allow the non-numeric value in any of the six formats

#### Scenario: Schema rejects value outside enum
- **WHEN** a tool declares `mode` with enum `["fast", "full"]`
- **THEN** the grammar allows only `fast` or `full` as the value of `mode`

#### Scenario: Schema requires required parameters
- **WHEN** a tool declares `pattern` as required and the model tries to close the call without it
- **THEN** the grammar does not allow the call to close

#### Scenario: Schema rejects undeclared parameter when additional properties are forbidden
- **WHEN** a tool schema sets `additionalProperties: false` and the model starts an undeclared parameter name
- **THEN** the grammar does not allow it

#### Scenario: Schema rejects unknown tool name
- **WHEN** the model writes a tool name that is not in the request's tool list
- **THEN** the grammar does not allow it

### Requirement: Generic grammar fallback for uncompilable schemas
When a tool set's schemas cannot be compiled into an llguidance grammar, the provider SHALL use a generic llguidance grammar for the format that checks only call structure and declared tool names, accepting the same language as the GBNF grammar for that format. The fallback SHALL be logged once per request and SHALL NOT fail the request.

#### Scenario: Uncompilable schema falls back to generic grammar
- **WHEN** a tool's schema uses a construct the schema compiler rejects
- **THEN** the request is generated with the generic llguidance grammar, one fallback message is logged, and the request succeeds

#### Scenario: Generic grammar matches GBNF language
- **WHEN** the generic llguidance grammar and the GBNF grammar for the same format and tools are applied at the same position over a real vocabulary
- **THEN** they allow the same set of tokens, except two classes that are not differences in the language: byte tokens that are not complete UTF-8 on their own, and control/special tokens (for example `<|im_start|>`), whose spelling GBNF accepts as text inside a value while llguidance never lets a text lexeme consume a special token

### Requirement: GBNF fallback when llguidance is unavailable or fails
When llguidance cannot build a grammar for a request, or reports an error during generation, the provider SHALL enforce the GBNF grammar for the rest of that request instead of generating unconstrained. The fallback SHALL be logged once per request. Builds without llguidance SHALL use GBNF.

#### Scenario: Matcher error switches to GBNF
- **WHEN** the llguidance matcher reports an error partway through a tool call
- **THEN** the following tokens are constrained by the GBNF grammar and one fallback message is logged

#### Scenario: Llguidance build failure uses GBNF
- **WHEN** llguidance cannot be initialised for the loaded model
- **THEN** the request is constrained by the GBNF grammar and succeeds

### Requirement: Rejection sampling
For grammar-constrained generation the provider SHALL first sample from the unmasked distribution and check only the sampled token against the grammar. Only when that token is rejected SHALL it mask the whole vocabulary and sample again. Every accepted token, including forced tokens, SHALL advance the grammar state.

#### Scenario: Allowed favourite is kept without full mask
- **WHEN** the most likely token is allowed by the grammar under greedy sampling
- **THEN** that token is returned and no whole-vocabulary mask is computed

#### Scenario: Rejected favourite is resampled from allowed tokens
- **WHEN** under greedy sampling the most likely token is forbidden by the grammar and an allowed token is the next most likely
- **THEN** the allowed token is returned, the same token grammar-first sampling would return

### Requirement: Grammar on every decoding path
Tool-call and structured-output grammars SHALL be enforced on streaming and non-streaming generation, on speculative (MTP) decoding, and across tokens the provider forces into the output when a thinking budget is spent.

#### Scenario: Speculative decoding respects the tool grammar
- **WHEN** speculative decoding is configured and the model writes a tool call
- **THEN** every token delivered for the call is allowed by the tool-call grammar

#### Scenario: Forced end of reasoning keeps grammar state
- **WHEN** a thinking budget forces the end-of-reasoning marker and the model then writes a tool call
- **THEN** the grammar activates on the trigger and constrains the call as without the forced text

### Requirement: Grammar overhead per token
With `grammar_backend = "llguidance"`, the CPU time the grammar adds per generated token inside a tool-call value SHALL be under 1 ms on average (release build, a vocabulary of at least 150k tokens), so that tool-call generation stays bound by model decoding rather than by the grammar.

#### Scenario: Llguidance value tokens stay under one millisecond
- **WHEN** a long `write_file` content value is fed token by token through the llguidance grammar with the qwen35 vocabulary (151,936 tokens) in a release build
- **THEN** the average grammar time per token, including the whole-vocabulary mask, is below 1 ms

#### Scenario: GBNF text values stay narrow
- **WHEN** the GBNF rule for a `</parameter>`-terminated value is rendered
- **THEN** every state has at most four alternatives and the initial state branches only on `<`

### Requirement: Verification on real model tokenizers
Each tool-call format SHALL be verified against the tokenizer of a real model that uses it, not only a synthetic vocabulary. Missing tokenizers and models SHALL be obtained from pinned sources: llama.cpp vocabulary files at a fixed commit, or Hugging Face model repositories at a fixed revision. Speculative decoding SHALL be verified on a real model with bundled MTP layers.

#### Scenario: Every format is checked on its model's tokenizer
- **WHEN** the grammar verification suite runs with network access or a populated cache
- **THEN** grammar parity, trigger and terminator checks run on these tokenizers:
  - Qwen3 JSON, Qwen XML-function and Ministral: llama.cpp `ggml-vocab-qwen35.gguf`
  - Gemma 4: llama.cpp `ggml-vocab-gemma-4.gguf`
  - LFM2: Hugging Face `LiquidAI/LFM2-350M-GGUF` at revision `8fdc9d526b7ed346b19257551b05816c7912ecc2`, file `LFM2-350M-Q4_0.gguf`
  - FunctionGemma: Hugging Face `ggml-org/functiongemma-270m-it-GGUF` at revision `2566ce14aedfc14fdd0de955ba67346425e67126`, file `functiongemma-270m-it-q8_0.gguf`

#### Scenario: Hugging Face tokenizers are fetched at the pinned revision
- **WHEN** a Hugging Face tokenizer is not in the cache (`$TMPDIR/qmt-llama-vocab`, or `QMT_LLAMA_VOCAB_DIR` when set)
- **THEN** only the model file's metadata prefix (at most 64 MiB) is downloaded from the pinned revision, stored as a vocabulary-only file, and reused by later runs without downloading again

#### Scenario: Real tokenizers report the expected vocabulary
- **WHEN** each real tokenizer is loaded
- **THEN** it reports the expected vocabulary size (151,936 for qwen35, 262,144 for gemma-4, 65,536 for LFM2, 262,144 for FunctionGemma) and each format's delimiters tokenize as the format's grammar expects

#### Scenario: Offline run skips real-tokenizer checks
- **WHEN** a tokenizer is neither cached nor downloadable
- **THEN** the checks that need it are skipped with a message naming the missing model, and the other checks still run

#### Scenario: Changed pinned model file fails loudly
- **WHEN** a pinned file is downloaded but its metadata does not end within 64 MiB or does not load as a vocabulary
- **THEN** the check fails with an error naming the repository, revision and file instead of being skipped

#### Scenario: MTP is checked on a real MTP model
- **WHEN** the live MTP check is enabled with `QMT_RUN_LIVE_LLAMA_MTP=1`
- **THEN** speculative decoding with tools runs on Hugging Face `unsloth/Qwen3.5-0.8B-MTP-GGUF` at revision `cf8a611f6ed2c2060046219a19f12cd3d5ecd67c`, file `Qwen3.5-0.8B-Q4_0.gguf`, which declares bundled MTP layers; without the variable the check is not run
