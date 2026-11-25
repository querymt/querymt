//! Worktree isolation vocabulary that appears in wire types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Delegation isolation requested for a delegate relative to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DelegationIsolation {
    /// Resolve from the parent's execution mode: isolated parents isolate
    /// delegates by default; local parents keep legacy shared execution.
    #[default]
    Inherit,
    /// Explicitly share the parent's checkout (disclosed as shared).
    Shared,
    /// Explicitly request a private checkout for this delegate.
    Isolated,
}

impl fmt::Display for DelegationIsolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DelegationIsolation::Inherit => "inherit",
            DelegationIsolation::Shared => "shared",
            DelegationIsolation::Isolated => "isolated",
        };
        f.write_str(s)
    }
}

impl FromStr for DelegationIsolation {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "inherit" => Ok(DelegationIsolation::Inherit),
            "shared" => Ok(DelegationIsolation::Shared),
            "isolated" => Ok(DelegationIsolation::Isolated),
            _ => Err(format!("Unknown delegation isolation: {}", s)),
        }
    }
}
