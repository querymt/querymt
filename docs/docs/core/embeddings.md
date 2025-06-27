# Embeddings

Embeddings are numerical representations of text (or other data types) in a high-dimensional vector space. These vectors capture the semantic meaning of the input, such that pieces of text with similar meanings will have vectors that are close together in this space. QueryMT provides a way to generate embeddings using LLMs.

## Key Components

*   **`EmbeddingProvider`**: A trait that LLM providers implement to support the generation of text embeddings. It has a single core method:
    *   `embed(&self, inputs: Vec<String>)`: Takes a vector of input strings and returns a `Result` containing a vector of embedding vectors (`Vec<Vec<f32>>`), where each inner vector corresponds to an input string. Each `f32` value is a dimension of the embedding.
    *   Source: `crates/querymt/src/embedding/mod.rs`

## How It Works

1.  **Prepare Inputs:** Your application prepares a list of text strings for which you want to generate embeddings.
2.  **Request Embeddings:** You call the `embed` method on an `LLMProvider` instance, passing the vector of input strings.
3.  **Provider Interaction:** The `LLMProvider` (or its underlying `HTTPEmbeddingProvider` for HTTP-based models) formats the request according to the specific LLM's API, sends it, and receives the raw response containing the embedding data.
4.  **Parse Response:** The provider parses the raw response and converts it into a `Vec<Vec<f32>>`, where each inner vector is an embedding for the corresponding input string.
5.  **Use Embeddings:** Your application can then use these embedding vectors for various downstream tasks.

## Example Flow (Conceptual)

```rust
// Assuming 'llm_provider' is an instance of Box<dyn LLMProvider>

let texts_to_embed = vec![
    "The quick brown fox jumps over the lazy dog.".to_string(),
    "A fast, agile, caramel-colored canine leaps above a sleepy hound.".to_string(),
    "The weather is sunny today.".to_string(),
];

match llm_provider.embed(texts_to_embed).await {
    Ok(embeddings) => {
        for (i, embedding_vector) in embeddings.iter().enumerate() {
            println!("Embedding for text {}: [{} dimensions]", i + 1, embedding_vector.len());
            // You would typically store these vectors in a vector database
            // or use them for similarity calculations.
            // e.g., println!("{:?}", embedding_vector);
        }
        // Example: Check similarity (conceptual, actual similarity requires a function)
        // let similarity_score = cosine_similarity(&embeddings[0], &embeddings[1]);
        // println!("Similarity between text 1 and 2: {}", similarity_score);
    }
    Err(e) => {
        eprintln!("Error generating embeddings: {}", e);
    }
}
```

## Use Cases for Embeddings

Embeddings are a cornerstone of many modern NLP applications, including:

*   **Semantic Search:** Finding documents or text snippets that are semantically similar to a query, rather than just matching keywords.
*   **Recommendation Systems:** Recommending items (articles, products, etc.) similar to what a user has previously interacted with.
*   **Clustering:** Grouping similar documents or texts together.
*   **Classification:** Training machine learning models to classify text based on its semantic content.
*   **Anomaly Detection:** Identifying outliers in textual data.
*   **Question Answering:** Finding relevant passages in a knowledge base to answer user questions.

QueryMT's `EmbeddingProvider` trait allows you to leverage different LLMs for embedding generation, enabling you to choose models optimized for this task or experiment with various embedding dimensions and characteristics.

