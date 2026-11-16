//! Settings of a runtime's delegation service.

use super::*;

/// How the delegation service of one runtime behaves.
///
/// The defaults are those of the `[quorum]` keys these settings replace.
#[derive(Debug, Clone)]
pub struct DelegationSettings {
    /// How to handle several delegations completing.
    pub wait_policy: DelegationWaitPolicy,
    /// Seconds to wait for delegations to complete.
    pub wait_timeout_secs: u64,
    /// Seconds in-flight delegates may keep running after the wait policy is met.
    pub cancel_grace_secs: u64,
    /// Delegations that may run in parallel. Must be at least 1.
    pub max_parallel: usize,
    /// Run the verification pass after a delegation completes.
    pub verification: bool,
    /// Optional summary of the parent conversation handed to the delegate.
    pub summary: Option<DelegationSummaryConfig>,
}

impl Default for DelegationSettings {
    fn default() -> Self {
        Self {
            wait_policy: DelegationWaitPolicy::default(),
            wait_timeout_secs: 120,
            cancel_grace_secs: 5,
            max_parallel: 5,
            verification: false,
            summary: None,
        }
    }
}
