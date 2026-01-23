use anthropic_auth::{AsyncOAuthClient as AnthropicOAuthClient, OAuthConfig, OAuthMode, TokenSet};
use anyhow::{Result, anyhow};
use colored::*;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

    /// Whether this provider supports automatic callback server
    fn supports_callback_server(&self) -> bool {
        false
    }

    /// Get the callback server port (if supported)
    fn callback_port(&self) -> Option<u16> {
        None
    }
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

/// OpenAI OAuth provider implementation
pub struct OpenAIProvider {
    client: openai_auth::OAuthClient,
    api_key: Arc<Mutex<Option<String>>>,
}

impl OpenAIProvider {
    pub fn new() -> Self {
        let config = openai_auth::OAuthConfig::builder()
            .redirect_port(1455)
            .build();
        let client =
            openai_auth::OAuthClient::new(config).expect("Failed to create OpenAI OAuth client");
        Self {
            client,
            api_key: Arc::new(Mutex::new(None)),
        }
    }
}

/// Codex OAuth provider implementation.
///
/// This is intentionally separate from OpenAI so tokens are stored and looked up
/// under `oauth_codex` in the keychain (no aliasing).
pub struct CodexProvider {
    client: openai_auth::OAuthClient,
}

impl CodexProvider {
    pub fn new() -> Self {
        let config = openai_auth::OAuthConfig::builder()
            .redirect_port(1455)
            .build();
        let client =
            openai_auth::OAuthClient::new(config).expect("Failed to create OpenAI OAuth client");
        Self { client }
    }
}

#[async_trait::async_trait]
impl OAuthProvider for OpenAIProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn display_name(&self) -> &str {
        "OpenAI"
    }

    async fn start_flow(&self) -> Result<OAuthFlowData> {
        let flow = self.client.start_flow()?;

        Ok(OAuthFlowData {
            authorization_url: flow.authorization_url,
            state: flow.state,
            verifier: flow.pkce_verifier,
        })
    }

    async fn exchange_code(&self, code: &str, _state: &str, verifier: &str) -> Result<TokenSet> {
        // Use exchange_code_for_api_key to get tokens with API key in one call
        let tokens = self
            .client
            .exchange_code_for_api_key(code, verifier)
            .await?;

        // Store the API key if present
        if let Some(ref api_key) = tokens.api_key
            && let Ok(mut slot) = self.api_key.lock()
        {
            *slot = Some(api_key.clone());
        }

        Ok(TokenSet {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
        })
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet> {
        let tokens = self.client.refresh_token(refresh_token).await?;

        // Try to obtain API key if we have an id_token
        if let Some(ref id_token) = tokens.id_token {
            match self.client.obtain_api_key(id_token).await {
                Ok(api_key) => {
                    if let Ok(mut slot) = self.api_key.lock() {
                        *slot = Some(api_key);
                    }
                }
                Err(err) => {
                    println!(
                        "OpenAI OAuth: API key exchange failed ({}). Try the `codex` provider if you only have OAuth access.",
                        err
                    );
                }
            }
        }

        Ok(TokenSet {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
        })
    }

    async fn create_api_key(&self, _access_token: &str) -> Result<Option<String>> {
        let mut api_key = None;
        if let Ok(mut slot) = self.api_key.lock() {
            api_key = slot.take();
        }
        Ok(api_key)
    }

    fn api_key_name(&self) -> Option<&str> {
        Some("OPENAI_API_KEY")
    }

    fn supports_callback_server(&self) -> bool {
        true
    }

    fn callback_port(&self) -> Option<u16> {
        Some(1455)
    }
}

#[async_trait::async_trait]
impl OAuthProvider for CodexProvider {
    fn name(&self) -> &str {
        "codex"
    }

    fn display_name(&self) -> &str {
        "Codex"
    }

    async fn start_flow(&self) -> Result<OAuthFlowData> {
        let flow = self.client.start_flow()?;

        Ok(OAuthFlowData {
            authorization_url: flow.authorization_url,
            state: flow.state,
            verifier: flow.pkce_verifier,
        })
    }

    async fn exchange_code(&self, code: &str, _state: &str, verifier: &str) -> Result<TokenSet> {
        // Codex backend uses the OAuth access token directly; we intentionally ignore any API key.
        let tokens = self
            .client
            .exchange_code_for_api_key(code, verifier)
            .await?;

        Ok(TokenSet {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
        })
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet> {
        let tokens = self.client.refresh_token(refresh_token).await?;

        Ok(TokenSet {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
        })
    }

    async fn create_api_key(&self, _access_token: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn api_key_name(&self) -> Option<&str> {
        None
    }

    fn supports_callback_server(&self) -> bool {
        true
    }

    fn callback_port(&self) -> Option<u16> {
        Some(1455)
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

    // Try automatic callback server or fall back to manual entry
    // For OpenAI with callback server, we get tokens directly
    let (tokens, callback_api_key) = if provider.supports_callback_server()
        && (provider.name() == "openai" || provider.name() == "codex")
    {
        match try_callback_server_openai(provider, &flow).await {
            Ok((tokens, api_key)) => {
                // Callback server already exchanged tokens
                let api_key = if provider.api_key_name().is_some() {
                    api_key
                } else {
                    None
                };
                (tokens, api_key)
            }
            Err(e) => {
                println!("{} Callback server failed: {}", "⚠️".bright_yellow(), e);
                println!("Falling back to manual code entry...\n");
                let code = manual_code_entry()?;
                println!("\n{} Exchanging code for tokens...", "🔄".bright_blue());
                let tokens = provider
                    .exchange_code(&code, &flow.state, &flow.verifier)
                    .await?;
                (tokens, None)
            }
        }
    } else {
        // Manual flow for other providers or when callback not supported
        let code = manual_code_entry()?;
        println!("\n{} Exchanging code for tokens...", "🔄".bright_blue());
        let tokens = provider
            .exchange_code(&code, &flow.state, &flow.verifier)
            .await?;
        (tokens, None)
    };

    // Store tokens
    store.set_oauth_tokens(provider.name(), &tokens)?;
    println!("{} Successfully authenticated!", "✓".bright_green());

    // Try to create API key if provider supports it
    // First check if we already got one from callback server
    let api_key = if let Some(key) = callback_api_key {
        Some(key)
    } else {
        provider
            .create_api_key(&tokens.access_token)
            .await
            .ok()
            .flatten()
    };

    if let Some(api_key) = api_key {
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

/// Try to use callback server for automatic code capture and token exchange (OpenAI only)
async fn try_callback_server_openai(
    provider: &dyn OAuthProvider,
    flow: &OAuthFlowData,
) -> Result<(TokenSet, Option<String>)> {
    use openai_auth::run_callback_server;

    let port = provider.callback_port().unwrap_or(1455);

    println!(
        "{} Starting callback server on port {}...",
        "🌐".bright_blue(),
        port
    );
    println!("{} Waiting for OAuth callback...", "⏳".bright_cyan());
    println!("   (The browser should redirect automatically after you authorize)\n");

    // Create a temporary client for the callback server
    let config = openai_auth::OAuthConfig::builder()
        .redirect_port(port)
        .build();
    let client = openai_auth::OAuthClient::new(config)?;

    // Start callback server with 2 minute timeout
    // New API: pass client and verifier, returns TokenSet directly
    let tokens_future = run_callback_server(port, &flow.state, &client, &flow.verifier);
    let timeout_duration = Duration::from_secs(120); // 2 minutes

    match tokio::time::timeout(timeout_duration, tokens_future).await {
        Ok(Ok(tokens)) => {
            println!(
                "{} Authorization and token exchange complete!",
                "✓".bright_green()
            );

            // Extract API key if present
            let api_key = tokens.api_key.clone();

            // Convert openai_auth::TokenSet to anthropic_auth::TokenSet
            let token_set = TokenSet {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                expires_at: tokens.expires_at,
            };

            Ok((token_set, api_key))
        }
        Ok(Err(e)) => Err(anyhow!("Callback server error: {}", e)),
        Err(_) => Err(anyhow!("Timeout waiting for OAuth callback (2 minutes)")),
    }
}

/// Prompt user to manually enter the authorization code
fn manual_code_entry() -> Result<String> {
    print!("Paste the authorization response (code#state format): ");
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    let response = response.trim();

    // For OpenAI, try to extract code from query string if present
    // This handles cases where user pastes the full callback URL
    if (response.contains('?') || response.contains('&'))
        && let Some(code) = extract_code_from_query(response)
    {
        return Ok(code);
    }

    Ok(response.to_string())
}

/// Extract authorization code from query string or URL
fn extract_code_from_query(input: &str) -> Option<String> {
    use url::Url;

    // Handle full URLs like http://localhost:1455/auth/callback?code=xxx&state=yyy
    if input.starts_with("http") {
        if let Ok(url) = Url::parse(input) {
            for (key, value) in url.query_pairs() {
                if key == "code" {
                    return Some(value.into_owned());
                }
            }
        }
        return None;
    }

    // Handle query string like ?code=xxx&state=yyy or code=xxx&state=yyy
    let query = input.trim_start_matches('?');
    for part in query.split('&') {
        if let Some((key, value)) = part.split_once('=')
            && key == "code"
        {
            return Some(value.to_string());
        }
    }

    None
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

    if let Ok(Some(api_key)) = provider.create_api_key(&new_tokens.access_token).await
        && let Some(key_name) = provider.api_key_name()
    {
        store.set(key_name, &api_key)?;
    }

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
    if provider.name() == "openai" {
        if let Some(token) = store.get_valid_access_token(provider.name()) {
            return Ok(token);
        }
        if let Some(key_name) = provider.api_key_name()
            && let Some(api_key) = store.get(key_name)
        {
            return Ok(api_key);
        }
        return Err(anyhow!(
            "OpenAI API key not found; run 'qmt auth login openai' to re-authenticate"
        ));
    }

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
/// * `try_refresh` - Whether to attempt automatic token refresh for expired tokens
///
/// # Returns
///
/// * `Result<()>` - Success or an error
pub async fn show_auth_status(
    store: &mut SecretStore,
    provider_name: Option<&str>,
    try_refresh: bool,
) -> Result<()> {
    let providers_to_check = if let Some(p) = provider_name {
        vec![p.to_string()]
    } else {
        // List all known OAuth providers
        vec![
            "anthropic".to_string(),
            "openai".to_string(),
            "codex".to_string(),
        ]
    };

    println!("{}", "OAuth Authentication Status".bright_blue());
    println!("{}", "===========================\n".bright_blue());

    for p in providers_to_check {
        print!("{}: ", p.bright_cyan());

        if let Some(tokens) = store.get_oauth_tokens(&p) {
            if tokens.is_expired() {
                // Try to refresh if enabled
                if try_refresh {
                    // Attempt to get OAuth provider and refresh
                    match get_oauth_provider(&p, None) {
                        Ok(oauth_provider) => {
                            match refresh_tokens(oauth_provider.as_ref(), store).await {
                                Ok(new_tokens) => {
                                    // Refresh successful
                                    println!("{}", "Valid ✓".bright_green());
                                    let expires_str = crate::secret_store::format_timestamp(
                                        new_tokens.expires_at,
                                    );
                                    println!("  Access token expires: {}", expires_str.dimmed());
                                    println!("  {}", "Refresh token available".dimmed());
                                }
                                Err(e) => {
                                    // Refresh failed
                                    println!("{}", "Expired ⚠️".bright_yellow());
                                    println!(
                                        "  {}",
                                        format!("Token refresh failed: {}", e).dimmed()
                                    );
                                    println!(
                                        "  {}",
                                        format!("Run 'qmt auth login {}' to re-authenticate", p)
                                            .bright_cyan()
                                    );
                                }
                            }
                        }
                        Err(_) => {
                            // Provider doesn't support OAuth (shouldn't happen but handle it)
                            println!("{}", "Expired ⚠️".bright_yellow());
                            println!(
                                "  {}",
                                format!("Run 'qmt auth login {}' to re-authenticate", p).dimmed()
                            );
                        }
                    }
                } else {
                    // No refresh, just show expired status
                    println!("{}", "Expired ⚠️".bright_yellow());
                    println!(
                        "  {}",
                        format!("Run 'qmt auth login {}' to re-authenticate", p).dimmed()
                    );
                }
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
                format!("Run 'qmt auth login {}' to authenticate", p).dimmed()
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
                    ));
                }
            };
            Ok(Box::new(AnthropicProvider::new(oauth_mode)))
        }
        "openai" => Ok(Box::new(OpenAIProvider::new())),
        "codex" => Ok(Box::new(CodexProvider::new())),
        _ => Err(anyhow!(
            "OAuth is not supported for provider '{}'",
            provider_name
        )),
    }
}
