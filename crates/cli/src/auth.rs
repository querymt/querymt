use anthropic_auth::{AsyncOAuthClient as AnthropicOAuthClient, OAuthConfig, OAuthMode, TokenSet};
use anyhow::{anyhow, Result};
use colored::*;
use std::io::{self, Write};

use crate::secret_store::SecretStore;

/// Trait for OAuth providers to implement
#[async_trait::async_trait]
pub trait OAuthProvider: Send + Sync {
    /// Get the provider name (e.g., "anthropic", "openai")
    fn name(&self) -> &str;

    /// Get the display name for user-facing messages
    fn display_name(&self) -> &str;

    /// Start the OAuth flow and return the authorization URL, state, and verifier
    async fn start_flow(&self) -> Result<OAuthFlowData>;

    /// Exchange authorization code for tokens
    async fn exchange_code(&self, code: &str, state: &str, verifier: &str) -> Result<TokenSet>;

    /// Refresh an expired token
    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet>;

    /// Optionally create an API key (for providers that support it)
    async fn create_api_key(&self, access_token: &str) -> Result<Option<String>>;

    /// Get the API key environment variable name (e.g., "ANTHROPIC_API_KEY")
    fn api_key_name(&self) -> Option<&str>;
}

/// OAuth flow data returned when starting a flow
pub struct OAuthFlowData {
    pub authorization_url: String,
    pub state: String,
    pub verifier: String,
}

/// Anthropic OAuth provider implementation
pub struct AnthropicProvider {
    mode: OAuthMode,
}

impl AnthropicProvider {
    pub fn new(mode: OAuthMode) -> Self {
        Self { mode }
    }
}

#[async_trait::async_trait]
impl OAuthProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn display_name(&self) -> &str {
        "Anthropic"
    }

    async fn start_flow(&self) -> Result<OAuthFlowData> {
        let client = AnthropicOAuthClient::new(OAuthConfig::default())?;
        let flow = client.start_flow(self.mode)?;

        Ok(OAuthFlowData {
            authorization_url: flow.authorization_url,
            state: flow.state,
            verifier: flow.verifier,
        })
    }

    async fn exchange_code(&self, code: &str, state: &str, verifier: &str) -> Result<TokenSet> {
        let client = AnthropicOAuthClient::new(OAuthConfig::default())?;
        Ok(client.exchange_code(code, state, verifier).await?)
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet> {
        let client = AnthropicOAuthClient::new(OAuthConfig::default())?;
        Ok(client.refresh_token(refresh_token).await?)
    }

    async fn create_api_key(&self, access_token: &str) -> Result<Option<String>> {
        if matches!(self.mode, OAuthMode::Console) {
            let client = AnthropicOAuthClient::new(OAuthConfig::default())?;
            Ok(Some(client.create_api_key(access_token).await?))
        } else {
            Ok(None)
        }
    }

    fn api_key_name(&self) -> Option<&str> {
        Some("ANTHROPIC_API_KEY")
    }
}

/// Generic OAuth authentication flow
///
/// # Arguments
///
/// * `provider` - The OAuth provider to authenticate with
/// * `store` - The secret store to save tokens to
///
/// # Returns
///
/// * `Result<()>` - Success or an error
pub async fn authenticate(provider: &dyn OAuthProvider, store: &mut SecretStore) -> Result<()> {
    println!(
        "{}",
        format!("=== {} OAuth Authentication ===\n", provider.display_name()).bright_blue()
    );

    println!(
        "Starting OAuth flow for {}...",
        provider.display_name().bright_cyan()
    );
    let flow = provider.start_flow().await?;

    println!(
        "\n{} Please visit this URL to authorize:",
        "🔐".bright_green()
    );
    println!("{}\n", flow.authorization_url.bright_yellow());

    // Try to open browser automatically
    {
        match anthropic_auth::open_browser(&flow.authorization_url) {
            Ok(_) => println!("{} Browser opened automatically\n", "✓".bright_green()),
            Err(_) => println!(
                "{} Could not open browser automatically\n",
                "!".bright_yellow()
            ),
        }
    }

    print!("Paste the authorization response (code#state format): ");
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    let response = response.trim();

    println!("\n{} Exchanging code for tokens...", "🔄".bright_blue());
    let tokens = provider
        .exchange_code(response, &flow.state, &flow.verifier)
        .await?;

    // Store tokens
    store.set_oauth_tokens(provider.name(), &tokens)?;
    println!("{} Successfully authenticated!", "✓".bright_green());

    // Try to create API key if provider supports it
    if let Ok(Some(api_key)) = provider.create_api_key(&tokens.access_token).await {
        println!("\n{} Creating API key...", "🔑".bright_blue());

        // Store API key separately
        if let Some(key_name) = provider.api_key_name() {
            store.set(key_name, &api_key)?;
        }

        println!("{} API key created and stored!", "✓".bright_green());
        println!(
            "\n{} Your API key has been securely stored in your system keychain.",
            "💡".bright_cyan()
        );
        println!(
            "   You can now use it with: {}",
            format!("qmt -p {} \"your prompt\"", provider.name()).bright_yellow()
        );
    } else {
        println!(
            "\n{} Your OAuth tokens have been securely stored in your system keychain.",
            "💡".bright_cyan()
        );
        println!(
            "   You can now use {} with: {}",
            provider.display_name(),
            format!("qmt -p {} \"your prompt\"", provider.name()).bright_yellow()
        );
    }

    Ok(())
}

/// Refresh OAuth tokens for a provider
///
/// # Arguments
///
/// * `provider` - The OAuth provider
/// * `store` - The secret store to load/save tokens
///
/// # Returns
///
/// * `Result<TokenSet>` - The new token set or an error
pub async fn refresh_tokens(
    provider: &dyn OAuthProvider,
    store: &mut SecretStore,
) -> Result<TokenSet> {
    let tokens = store
        .get_oauth_tokens(provider.name())
        .ok_or_else(|| anyhow!("No OAuth tokens found for {}", provider.display_name()))?;

    let new_tokens = provider.refresh_token(&tokens.refresh_token).await?;

    // Store the new tokens
    store.set_oauth_tokens(provider.name(), &new_tokens)?;

    Ok(new_tokens)
}

/// Get a valid access token for a provider, refreshing if necessary
///
/// # Arguments
///
/// * `provider` - The OAuth provider
/// * `store` - The secret store to load tokens from
///
/// # Returns
///
/// * `Result<String>` - The access token or an error
pub async fn get_valid_token(
    provider: &dyn OAuthProvider,
    store: &mut SecretStore,
) -> Result<String> {
    // Try to get valid token
    if let Some(token) = store.get_valid_access_token(provider.name()) {
        return Ok(token);
    }

    // Token is expired or missing, try to refresh
    log::info!(
        "{} OAuth token expired, attempting to refresh...",
        provider.display_name()
    );
    let new_tokens = refresh_tokens(provider, store).await?;

    Ok(new_tokens.access_token)
}

/// Display OAuth authentication status
///
/// # Arguments
///
/// * `store` - The secret store to check
/// * `provider_name` - Optional provider name to check (defaults to all supported providers)
///
/// # Returns
///
/// * `Result<()>` - Success or an error
pub fn show_auth_status(store: &SecretStore, provider_name: Option<&str>) -> Result<()> {
    let providers_to_check = if let Some(p) = provider_name {
        vec![p.to_string()]
    } else {
        // List all known OAuth providers
        vec!["anthropic".to_string()]
    };

    println!("{}", "OAuth Authentication Status".bright_blue());
    println!("{}", "===========================\n".bright_blue());

    for p in providers_to_check {
        print!("{}: ", p.bright_cyan());

        if let Some(tokens) = store.get_oauth_tokens(&p) {
            if tokens.is_expired() {
                println!("{}", "Expired ⚠️".bright_yellow());
                println!(
                    "  {}",
                    format!("Run 'qmt auth {}' to re-authenticate", p).dimmed()
                );
            } else {
                println!("{}", "Valid ✓".bright_green());

                let expires_str = crate::secret_store::format_timestamp(tokens.expires_at);
                println!("  Access token expires: {}", expires_str.dimmed());
                println!("  {}", "Refresh token available".dimmed());
            }
        } else {
            println!("{}", "Not authenticated".dimmed());
            println!(
                "  {}",
                format!("Run 'qmt auth {}' to authenticate", p).dimmed()
            );
        }

        println!();
    }

    Ok(())
}

/// Get the appropriate OAuth provider for a given provider name
///
/// # Arguments
///
/// * `provider_name` - The name of the provider (e.g., "anthropic")
/// * `mode` - Optional mode string for providers that support multiple modes
///
/// # Returns
///
/// * `Result<Box<dyn OAuthProvider>>` - The OAuth provider or an error
pub fn get_oauth_provider(
    provider_name: &str,
    mode: Option<&str>,
) -> Result<Box<dyn OAuthProvider>> {
    match provider_name {
        "anthropic" => {
            let oauth_mode = match mode {
                Some("max") | None => OAuthMode::Max,
                Some("console") => OAuthMode::Console,
                Some(m) => {
                    return Err(anyhow!(
                        "Invalid mode '{}' for Anthropic. Use 'max' or 'console'",
                        m
                    ))
                }
            };
            Ok(Box::new(AnthropicProvider::new(oauth_mode)))
        }
        // Future providers can be added here:
        // "openai" => Ok(Box::new(OpenAIProvider::new())),
        _ => Err(anyhow!(
            "OAuth is not supported for provider '{}'",
            provider_name
        )),
    }
}
