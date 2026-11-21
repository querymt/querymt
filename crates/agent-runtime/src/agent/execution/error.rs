//! The error of a run: what stopped the loop without an outcome.
//!
//! A failure keeps its source, so a caller can tell a provider failure from
//! a middleware failure by matching on the variant instead of searching a
//! chain of strings.
//!
//! # How a failure is rendered
//!
//! `Provider`, `Middleware` and `History` print their source in their
//! `Display` *and* keep it as `#[source]`. `to_string()` is the text a client
//! and the `Error` event carry, and it must contain the cause, so it does.
//! The cost is that a reporter which also walks the source chain (`{:#}` on an
//! `anyhow::Error` made from a `TurnError`, `anyhow::Report`) prints the cause
//! twice. Nothing renders a `TurnError` that way: the run reports it with
//! `to_string()` only. Whoever adds such a reporter should drop `{source}` from
//! these `Display`s and join the chain at the boundary instead (`Other`, being
//! transparent over `anyhow`, already prints only its outermost context).
//! `rendering_is_pinned` fixes today's text.

use crate::middleware::MiddlewareError;
use crate::session::error::SessionError;
use querymt::error::LLMError;

/// Why a run failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TurnError {
    /// The provider could not be built or a model request failed for good.
    #[error("LLM {operation} error (provider={provider}, model={model}): {source}")]
    Provider {
        /// What was being done: `provider initialization`, `chat`, `streaming`.
        operation: &'static str,
        provider: String,
        model: String,
        #[source]
        source: LLMError,
    },
    /// A middleware hook returned an error.
    #[error("middleware {name} failed: {source}")]
    Middleware {
        /// The configured name of the middleware.
        name: &'static str,
        #[source]
        source: MiddlewareError,
    },
    /// Reading or writing the session's stored conversation failed. The run
    /// is the only writer of its history, so this is never retried.
    #[error("{what}: {source}")]
    History {
        /// What was being done: `Failed to store assistant message`, ...
        what: &'static str,
        #[source]
        source: SessionError,
    },
    /// A Stop hook command failed.
    #[error(transparent)]
    Hook(#[from] crate::hooks::HookError),
    /// Any other failure of a step: storage, hooks, tools, accounting.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<crate::middleware::driver::HookFailure> for TurnError {
    fn from(failure: crate::middleware::driver::HookFailure) -> Self {
        TurnError::Middleware {
            name: failure.name,
            source: failure.source,
        }
    }
}

impl TurnError {
    /// The conversion for `map_err` on a history read or write:
    /// `.map_err(TurnError::history("Failed to store wait message"))`.
    pub(crate) fn history(what: &'static str) -> impl FnOnce(SessionError) -> Self {
        move |source| TurnError::History { what, source }
    }

    /// The provider's error when this failure is one, whichever way it
    /// reached the run: directly, or inside an [`anyhow::Error`] chain.
    pub(crate) fn llm_error(&self) -> Option<&LLMError> {
        match self {
            TurnError::Provider { source, .. } => Some(source),
            TurnError::Middleware { .. } | TurnError::History { .. } | TurnError::Hook(_) => None,
            TurnError::Other(error) => error
                .chain()
                .find_map(|cause| cause.downcast_ref::<LLMError>()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    /// The text of each variant is what clients and the `Error` event carry:
    /// the cause is in it, and it is also reachable as the source. See the
    /// module documentation for why that is kept.
    #[test]
    fn rendering_is_pinned() {
        let provider = TurnError::Provider {
            operation: "chat",
            provider: "mock".into(),
            model: "mock-model".into(),
            source: LLMError::ProviderError("boom".into()),
        };
        let middleware = TurnError::Middleware {
            name: "Limits",
            source: MiddlewareError::ExecutionError("bad".into()),
        };
        let history = TurnError::History {
            what: "Failed to store wait message",
            source: SessionError::Other("disk full".into()),
        };
        let other = TurnError::Other(anyhow::anyhow!("inner").context("outer"));

        assert_eq!(
            provider.to_string(),
            "LLM chat error (provider=mock, model=mock-model): LLM Provider Error: boom"
        );
        assert_eq!(
            middleware.to_string(),
            "middleware Limits failed: Execution error: bad"
        );
        assert_eq!(
            history.to_string(),
            "Failed to store wait message: disk full"
        );
        // Transparent over `anyhow`: only the outermost context.
        assert_eq!(other.to_string(), "outer");

        for typed in [&provider, &middleware, &history] {
            assert!(typed.source().is_some(), "{typed}");
        }
    }
}
