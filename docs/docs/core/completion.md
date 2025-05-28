# Text Completion

Text completion is a fundamental capability of Large Language Models where the model generates text that continues from a given input prompt. QueryMT provides a standardized way to perform text completion tasks.

## Key Components

*   **`CompletionRequest`**: Represents a request for text completion. It includes:
    *   `prompt`: The input text that the LLM should complete.
    *   `suffix`: Optional text that should appear after the model's completion.
    *   `max_tokens`: Optional limit on the number of tokens to generate.
    *   `temperature`: Optional parameter (0.0-1.0) to control the randomness of the output. Higher values make the output more random, while lower values make it more deterministic.
    *   Source: `crates/querymt/src/completion/mod.rs`

*   **`CompletionResponse`**: Represents the LLM's generated completion. It primarily contains:
    *   `text`: The generated text string.
    *   It also implements the `ChatResponse` trait, meaning its `text()` method can be used to retrieve the completion, but `tool_calls()` will typically be `None` for completions.
    *   Source: `crates/querymt/src/completion/mod.rs`

*   **`CompletionProvider`**: A trait that LLM providers implement to support text completion. It has a single method:
    *   `complete(&self, req: &CompletionRequest)`: Sends a completion request to the LLM and returns a `CompletionResponse`.
    *   Source: `crates/querymt/src/completion/mod.rs`

## How It Works

1.  **Create Request:** Your application creates a `CompletionRequest` object, providing the prompt and any optional parameters like `max_tokens` or `temperature`. QueryMT offers a `CompletionRequest::builder()` for a more fluent way to construct these requests.
2.  **Send Request:** You call the `complete` method on an `LLMProvider` instance, passing the `CompletionRequest`.
3.  **Provider Interaction:** The `LLMProvider` (or its underlying `HTTPCompletionProvider` if it's an HTTP-based model) formats the request according to the specific LLM's API, sends it, and receives the raw response.
4.  **Parse Response:** The provider parses the raw response into a `CompletionResponse` object.
5.  **Use Completion:** Your application can then access the generated text from the `CompletionResponse.text` field.

## Example Flow (Conceptual)

```rust
// Assuming 'llm_provider' is an instance of Box<dyn LLMProvider>

let request = CompletionRequest::builder("Once upon a time, in a land far, far away,")
    .max_tokens(100)
    .temperature(0.7)
    .build();

match llm_provider.complete(&request).await {
    Ok(response) => {
        println!("Generated story: {}", response.text);
    }
    Err(e) => {
        eprintln!("Error during completion: {}", e);
    }
}
```

Text completion is a versatile tool for tasks such as:

*   Drafting emails or documents.
*   Summarizing text.
*   Translating languages.
*   Generating code snippets.
*   Creative writing and storytelling.

QueryMT's `CompletionProvider` abstraction ensures that you can switch between different LLM backends for completion tasks without significantly changing your application code.

