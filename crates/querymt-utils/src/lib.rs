pub mod telemetry;

#[cfg(feature = "secret-store")]
pub mod secret_store;

#[cfg(feature = "oauth")]
pub mod oauth;

#[cfg(feature = "providers")]
pub mod providers;

/// OAuth flow interaction mode.
///
/// Determines the user-facing UX for a given provider's OAuth flow.
/// Defined at the crate root so it is available regardless of feature flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum OAuthFlowKind {
    /// Redirect/callback flow where the user pastes the callback URL or code.
    RedirectCode,
    /// Device flow where the backend polls the provider's token endpoint.
    DevicePoll,
}
