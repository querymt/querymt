# Chaining Prompts

QueryMT allows you to orchestrate sequences of LLM interactions, known as "prompt chains." This is useful for breaking down complex tasks into smaller, manageable steps, where the output of one LLM call can be used as input for subsequent calls. QueryMT provides two main mechanisms for this: `PromptChain` for single-provider chains and `MultiPromptChain` for chains involving multiple different providers.

These are primarily defined in `crates/querymt/src/chain/mod.rs` and `crates/querymt/src/chain/multi.rs`.

## `PromptChain` (Single Provider)

A `querymt::chain::PromptChain` executes a sequence of steps using a single `LLMProvider` instance.

### Key Components:

*   **`querymt::chain::ChainStep`**: Defines a single step in the chain.
    *   `id`: A unique identifier for this step. The output of this step will be stored in memory using this ID.
    *   `template`: A prompt template string. It can contain placeholders like `{{variable_name}}`, which will be replaced by outputs from previous steps (stored in memory).
    *   `mode`: `enum@querymt::chain::ChainStepMode::Chat` or `enum@querymt::chain::ChainStepMode::Completion`, indicating how this step should be executed.
    *   Optional parameters like `temperature`, `max_tokens`, `top_p`.
*   **`querymt::chain::PromptChain<'a>`**: Manages the sequence of `ChainStep`s.
    *   It holds a reference to an `LLMProvider` and a `memory` (HashMap) to store the outputs of each step.
    *   `new(llm: &'a dyn LLMProvider)`: Creates a new chain.
    *   `step(step: ChainStep)`: Adds a step to the chain.
    *   `run()`: Executes all steps in sequence.
        *   For each step, it applies the template using values from memory.
        *   Executes the step using the LLM (chat or completion).
        *   Stores the LLM's response text in memory, keyed by the step's `id`.
        *   Returns the final memory map containing all step outputs.

### Example (Conceptual `PromptChain`):

```rust
// Assuming 'llm' is a Box<dyn LLMProvider>
use querymt::chain::{PromptChain, ChainStepBuilder, ChainStepMode};

let chain = PromptChain::new(&*llm)
    .step(
        ChainStepBuilder::new("step1_idea", "Generate a topic for a short story.", ChainStepMode::Chat)
            .build()
    )
    .step(
        ChainStepBuilder::new("step2_plot", "Write a brief plot for a story about: {{step1_idea}}", ChainStepMode::Chat)
            .max_tokens(200)
            .build()
    );

let results = chain.run().await?;
println!("Generated Plot: {}", results.get("step2_plot").unwrap_or_default());
```

## `MultiPromptChain` (Multiple Providers)

A `querymt::chain::MultiPromptChain` allows you to define steps that can be executed by different LLM providers, registered in an `querymt::chain::LLMRegistry`. It also supports complex, iterative tool-calling within each step.

### Key Components:

*   **`querymt::chain::LLMRegistry`**: A collection (`HashMap`) that stores multiple `LLMProvider` instances, each identified by a unique string key (e.g., "openai", "anthropic-haiku").
    *   `querymt::chain::LLMRegistryBuilder` provides a fluent way to construct this registry.
*   **`querymt::chain::multi::MultiChainStep`**: Similar to `ChainStep`, but includes:
    *   `provider_id`: The string key of the `LLMProvider` in the `LLMRegistry` that should execute this step.
    *   `response_transform`: An optional function `Box<dyn Fn(String) -> String + Send + Sync>` to transform the raw string output of the LLM before storing it in memory.
*   **`querymt::chain::multi::MultiChainStepBuilder`**: Builder for `MultiChainStep`.
*   **`querymt::chain::MultiPromptChain<'a>`**: Manages the sequence of `MultiChainStep`s.
    *   Holds a reference to an `LLMRegistry`.
    *   The `run()` method works similarly to `PromptChain`, but for each step, it retrieves the specified `LLMProvider` from the registry before execution.
    *   **Tool Calling Loop**: Within a single chat step, `MultiPromptChain` can handle iterative tool calls. If the LLM responds with a request to call a tool, the chain will execute the tool via the provider's `call_tool` method, append the result to the conversation history, and send it back to the LLM. This loop continues until the LLM provides a final text response without any tool calls.

### Example (Conceptual `MultiPromptChain`):

```rust
// Assuming 'registry' is an LLMRegistry with providers "fast_model" and "creative_model"
use querymt::chain::{MultiPromptChain, multi::{MultiChainStepBuilder, MultiChainStepMode}, LLMRegistryBuilder};

// let registry = LLMRegistryBuilder::new()
//     .register("fast_model", fast_llm_provider)
//     .register("creative_model", creative_llm_provider)
//     .build();

let chain = MultiPromptChain::new(&registry)
    .step(
        MultiChainStepBuilder::new(MultiChainStepMode::Chat)
            .provider_id("fast_model")
            .id("step1_keywords")
            .template("Extract 3 keywords from this text: 'The future of AI is exciting.'")
            .build()?
    )
    .step(
        MultiChainStepBuilder::new(MultiChainStepMode::Completion)
            .provider_id("creative_model")
            .id("step2_tagline")
            .template("Generate a catchy tagline using these keywords: {{step1_keywords}}")
            .max_tokens(50)
            .response_transform(|s| s.trim().to_uppercase()) // Example transform
            .build()?
    );

let results = chain.run().await?;
println!("Generated Tagline: {}", results.get("step2_tagline").unwrap_or_default());
```

Prompt chaining is a powerful technique for building more sophisticated LLM applications by decomposing tasks and leveraging the strengths of different models or configurations for different parts of a workflow.
