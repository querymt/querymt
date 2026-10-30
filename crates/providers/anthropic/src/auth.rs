//! OAuth identity handling, version validation and auth headers.

use crate::config::{Anthropic, AuthType, detect_auth_type};
use crate::wire::system::{AnthropicSystemPrompt, TextBlockParam};
use http::header::{AUTHORIZATION, USER_AGENT};
use querymt::error::LLMError;
use sha2::{Digest, Sha256};

/// OAuth system prompt
pub(crate) const OAUTH_SYSTEM_PROMPT: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
pub(crate) const CLAUDE_CODE_VERSION: &str = "2.1.284";

pub(crate) fn oauth_text_block(text: String) -> TextBlockParam {
    TextBlockParam {
        block_type: "text".to_string(),
        text,
        cache_control: None,
        citations: None,
    }
}

/// Identity lines injected by QueryMT application prompts.
///
/// OAuth requests already carry Anthropic's Claude identity via
/// `OAUTH_SYSTEM_PROMPT`, so application-level identity declarations would
/// compete with it and are pruned. Everything else — operational rules,
/// `<env>`/`<directories>` metadata, mentions of QueryMT in instructions —
/// is preserved verbatim.
pub(crate) const IDENTITY_LINE_PREFIXES: [&str; 2] =
    ["You are QueryMT, a coding agent", "You are powered by "];

pub(crate) fn sanitize_oauth_text(text: &str) -> String {
    // Drop only whole lines that declare an application identity.
    let kept: Vec<&str> = text
        .split('\n')
        .filter(|line| {
            !IDENTITY_LINE_PREFIXES
                .iter()
                .any(|prefix| line.starts_with(prefix))
        })
        .collect();

    // Collapse paragraphs left empty by the pruning above.
    let mut paragraphs = Vec::new();
    let mut current = Vec::new();
    for line in kept {
        if line.is_empty() {
            if !current.is_empty() {
                paragraphs.push(current.join("\n"));
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current.join("\n"));
    }
    paragraphs.join("\n\n").trim().to_string()
}

pub(crate) fn oauth_billing_header(text: &str, version: &str) -> String {
    let cch: String = Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // JavaScript indexes UTF-16 code units, not UTF-8 bytes or Unicode scalars.
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let sampled: Vec<u16> = [4, 7, 20]
        .iter()
        .map(|&i| utf16.get(i).copied().unwrap_or(u16::from(b'0')))
        .collect();
    let suffix: String = Sha256::digest(
        format!(
            "59cf53e54c78{}{}",
            String::from_utf16_lossy(&sampled),
            version
        )
        .as_bytes(),
    )
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
    format!(
        "x-anthropic-billing-header: cc_version={version}.{}; cc_entrypoint=sdk-cli; cch={};",
        &suffix[..3],
        &cch[..5]
    )
}

pub(crate) fn validate_client_version(version: &str) -> Result<(), LLMError> {
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(LLMError::InvalidRequest(
            "claude_code_version must be major.minor.patch".to_string(),
        ));
    }
    Ok(())
}

impl Anthropic {
    pub(crate) fn oauth_version(&self) -> &str {
        self.claude_code_version
            .as_deref()
            .map(str::trim)
            .unwrap_or(CLAUDE_CODE_VERSION)
    }

    pub(crate) fn validate_oauth_version(&self) -> Result<(), LLMError> {
        validate_client_version(self.oauth_version())
    }

    /// Returns the current API key, using the resolver if available.
    pub(crate) fn resolved_key(&self) -> String {
        if let Some(ref resolver) = self.key_resolver {
            resolver.current()
        } else {
            self.api_key.clone()
        }
    }

    /// Determines the authentication type to use.
    /// Delegates to `detect_auth_type` for the actual logic.
    pub(crate) fn determine_auth_type(&self) -> AuthType {
        let key = self.resolved_key();
        detect_auth_type(&key, self.auth_type.clone())
    }

    /// Returns true if using OAuth authentication
    pub(crate) fn is_oauth(&self) -> bool {
        self.determine_auth_type() == AuthType::OAuth
    }

    /// Sanitizes the system prompt for OAuth requests.
    ///
    /// For OAuth: prunes application identity lines (see
    /// `IDENTITY_LINE_PREFIXES`), drops blocks left empty by that pruning,
    /// and prepends `OAUTH_SYSTEM_PROMPT` as the first block, converting a
    /// plain `Text` prompt to `Blocks` in the process. When no system prompt
    /// is configured and OAuth is active, returns the `OAUTH_SYSTEM_PROMPT`
    /// const wrapped in a single-element `Blocks`.
    ///
    /// For non-OAuth: returns the configured system prompt unchanged.
    pub(crate) fn sanitize_system_prompt(&self) -> Option<AnthropicSystemPrompt> {
        if !self.is_oauth() {
            return self.system.clone();
        }
        let mut blocks = match &self.system {
            None => Vec::new(),
            Some(AnthropicSystemPrompt::Text(text)) => {
                vec![oauth_text_block(sanitize_oauth_text(text))]
            }
            Some(AnthropicSystemPrompt::Blocks(blocks)) => blocks
                .iter()
                .cloned()
                .map(|mut block| {
                    block.text = sanitize_oauth_text(&block.text);
                    block
                })
                .collect(),
        };
        // Anthropic rejects empty text blocks; identity-only prompts prune to
        // nothing here and only the injected identity block remains.
        blocks.retain(|block| !block.text.is_empty());
        // Keep the identity separate from cached application instructions.
        if blocks
            .first()
            .is_none_or(|block| block.text != OAUTH_SYSTEM_PROMPT)
        {
            blocks.insert(0, oauth_text_block(OAUTH_SYSTEM_PROMPT.to_string()));
        }
        Some(AnthropicSystemPrompt::Blocks(blocks))
    }

    /// Adds authentication headers to the request builder based on auth type
    pub(crate) fn add_auth_headers(
        &self,
        builder: http::request::Builder,
    ) -> http::request::Builder {
        let key = self.resolved_key();
        let auth_type = self.determine_auth_type();
        let builder = match auth_type {
            AuthType::OAuth => builder
                .header(AUTHORIZATION, format!("Bearer {}", key))
                .header(
                    "anthropic-beta",
                    "oauth-2025-04-20,interleaved-thinking-2025-05-14",
                )
                .header(
                    USER_AGENT,
                    format!("claude-cli/{} (external, cli)", self.oauth_version()),
                ),
            AuthType::ApiKey => builder.header("x-api-key", &key),
        };
        builder.header("anthropic-version", "2023-06-01")
    }
}
