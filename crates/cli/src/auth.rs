//! Console OAuth UI implementation
//!
//! This module provides a console-based implementation of the `OAuthUI` trait
//! from `querymt_utils::oauth`. It handles presenting OAuth flows to users via
//! the terminal with colored output, browser auto-opening, and optional callback
//! server support.

use anyhow::Result;
use async_trait::async_trait;
use colored::*;
use querymt_utils::oauth::{OAuthFlowData, OAuthFlowKind, OAuthProvider, OAuthUI};
use std::io::{self, Write};
use std::time::Duration;

/// Console-based OAuth UI implementation
///
/// This implementation:
/// - Uses colored terminal output for status messages
/// - Automatically opens the browser for authorization URLs
/// - Supports Codex callback server for automatic code capture
/// - Falls back to manual code entry if callback server fails
pub struct ConsoleOAuthUI;

#[async_trait]
impl OAuthUI for ConsoleOAuthUI {
    async fn authorize(&self, _provider_name: &str, url: &str, _state: &str) -> Result<String> {
        println!(
            "\n{} Please visit this URL to authorize:",
            "🔐".bright_green()
        );
        println!("{}\n", url.bright_yellow());

        // Try to open browser automatically
        match open::that(url) {
            Ok(_) => println!("{} Browser opened automatically\n", "✓".bright_green()),
            Err(_) => println!(
                "{} Could not open browser automatically\n",
                "!".bright_yellow()
            ),
        }

        // Manual code entry
        manual_code_entry()
    }

    async fn authorize_and_exchange(
        &self,
        provider: &dyn OAuthProvider,
        flow: &OAuthFlowData,
    ) -> Result<Option<(querymt_utils::oauth::TokenSet, Option<String>)>> {
        // Device-poll flow: show URL, open browser, poll for token.
        if provider.flow_kind() == OAuthFlowKind::DevicePoll {
            println!(
                "\n{} Please visit this URL to authorize:",
                "🔐".bright_green()
            );
            println!("{}\n", flow.authorization_url.bright_yellow());

            match open::that(&flow.authorization_url) {
                Ok(_) => println!("{} Browser opened automatically\n", "✓".bright_green()),
                Err(_) => println!(
                    "{} Could not open browser automatically\n",
                    "!".bright_yellow()
                ),
            }

            println!("{} Waiting for device authorization...", "⏳".bright_cyan());
            println!(
                "   (Approve the request in your browser, then authentication will complete automatically)\n"
            );

            let tokens = provider
                .exchange_code("", &flow.state, &flow.verifier)
                .await?;

            println!("{} Device authorization complete!", "✓".bright_green());

            let api_key = provider
                .create_api_key(&tokens.access_token)
                .await
                .ok()
                .flatten();

            return Ok(Some((tokens, api_key)));
        }

        // Only use callback server for Codex
        if provider.name() != "codex" {
            return Ok(None);
        }

        println!(
            "\n{} Please visit this URL to authorize:",
            "🔐".bright_green()
        );
        println!("{}\n", flow.authorization_url.bright_yellow());

        // Try to open browser automatically
        match open::that(&flow.authorization_url) {
            Ok(_) => println!("{} Browser opened automatically\n", "✓".bright_green()),
            Err(_) => println!(
                "{} Could not open browser automatically\n",
                "!".bright_yellow()
            ),
        }

        // Try callback server
        let port = 1455;
        println!(
            "{} Starting callback server on port {}...",
            "🌐".bright_blue(),
            port
        );
        println!("{} Waiting for OAuth callback...", "⏳".bright_cyan());
        println!("   (The browser should redirect automatically after you authorize)\n");

        match querymt_utils::oauth::openai_callback_server(
            port,
            &flow.state,
            &flow.verifier,
            Duration::from_secs(120),
        )
        .await
        {
            Ok((tokens, api_key)) => {
                println!(
                    "{} Authorization and token exchange complete!",
                    "✓".bright_green()
                );
                // Only return API key if provider supports it
                let api_key = if provider.api_key_name().is_some() {
                    api_key
                } else {
                    None
                };
                Ok(Some((tokens, api_key)))
            }
            Err(e) => {
                println!("{} Callback server failed: {}", "⚠️".bright_yellow(), e);
                println!("Falling back to manual code entry...\n");
                Ok(None) // Fall back to manual entry
            }
        }
    }

    fn status(&self, message: &str) {
        println!("{}", message);
    }

    fn success(&self, message: &str) {
        println!("{} {}", "✓".bright_green(), message);
    }

    fn error(&self, message: &str) {
        println!("{} {}", "⚠️".bright_yellow(), message);
    }
}

/// Prompt user to manually enter the authorization code
fn manual_code_entry() -> Result<String> {
    print!("Paste the authorization response (code#state format): ");
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    let response = response.trim();

    // Try to extract code from query string if present
    // This handles cases where user pastes the full callback URL
    if response.contains('?') || response.contains('&') {
        if let Some(code) = querymt_utils::oauth::extract_code_from_query(response) {
            return Ok(code);
        }
    }

    Ok(response.to_string())
}
