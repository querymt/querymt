//! OAuth token refresh providers
//!
//! This module provides abstractions for refreshing OAuth tokens for different providers.

use anthropic_auth::{AsyncOAuthClient as AnthropicOAuthClient, OAuthConfig, TokenSet};
use anyhow::{Result, anyhow};

/// Trait for OAuth providers to implement token refresh
#[async_trait::async_trait]
pub trait TokenRefresher: Send + Sync {
    /// Get the provider name (e.g., "anthropic", "openai")
    fn provider_name(&self) -> &str;

    /// Refresh an expired token
    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet>;
}

/// Anthropic OAuth token refresher
pub struct AnthropicRefresher;

#[async_trait::async_trait]
impl TokenRefresher for AnthropicRefresher {
    fn provider_name(&self) -> &str {
        "anthropic"
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet> {
        let client = AnthropicOAuthClient::new(OAuthConfig::default())?;
        Ok(client.refresh_token(refresh_token).await?)
    }
}

/// OpenAI/Codex OAuth token refresher.
///
/// `provider_name` is used as the keyring lookup key (e.g. `oauth_openai` vs `oauth_codex`).
pub struct OpenAIRefresher {
    provider_name: &'static str,
}

#[async_trait::async_trait]
impl TokenRefresher for OpenAIRefresher {
    fn provider_name(&self) -> &str {
        self.provider_name
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet> {
        let config = openai_auth::OAuthConfig::builder()
            .redirect_port(1455)
            .build();
        let client = openai_auth::OAuthClient::new(config)?;

        // Refresh the tokens
        let tokens = client.refresh_token(refresh_token).await?;

        // Convert openai_auth::TokenSet to anthropic_auth::TokenSet
        Ok(TokenSet {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
        })
    }
}

/// Get the appropriate token refresher for a provider
///
/// # Arguments
///
/// * `provider` - The provider name (e.g., "anthropic", "openai")
///
/// # Returns
///
/// * `Result<Box<dyn TokenRefresher>>` - The token refresher or an error
pub fn get_token_refresher(provider: &str) -> Result<Box<dyn TokenRefresher>> {
    match provider {
        "anthropic" => Ok(Box::new(AnthropicRefresher)),
        "openai" => Ok(Box::new(OpenAIRefresher {
            provider_name: "openai",
        })),
        "codex" => Ok(Box::new(OpenAIRefresher {
            provider_name: "codex",
        })),
        _ => Err(anyhow!(
            "OAuth token refresh not supported for provider: {}",
            provider
        )),
    }
}
