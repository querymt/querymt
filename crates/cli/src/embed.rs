use querymt::LLMProvider;
use std::io::{self, Read};

/// Read either the provided `text` or stdin, then call your provider's embedding method
pub async fn embed_pipe(
    provider: &Box<dyn LLMProvider>,
    input: Option<&String>,
    document_separator: Option<&String>,
) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
    // read from stdin if no `text` was passed
    let mut stdin_buf = Vec::new();
    io::stdin().read_to_end(&mut stdin_buf)?;

    let to_embed = if let Some(t) = input {
        t.clone()
    } else {
        String::from_utf8_lossy(&stdin_buf).to_string()
    };

    let documents = if let Some(separator) = document_separator {
        to_embed
            .split(separator)
            .filter(|s| !s.is_empty())
            .map(|v| v.to_string())
            .collect()
    } else {
        vec![to_embed]
    };

    let embeddings = provider
        .embed(documents)
        .await
        .map_err(|e| format!("Embedding error: {:#}", e))?;

    Ok(embeddings)
}
