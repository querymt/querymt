//! How a tool call must be approved before it runs.

/// How a tool call must be approved before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalNeed {
    /// Runs without a prompt.
    None,
    /// Prompts; "Allow for this session" is offered and remembered.
    Session,
    /// Prompts on every call; never remembered. `reason` comes from a hook.
    EveryCall { reason: Option<String> },
}

impl ApprovalNeed {
    /// The stricter of two needs: `EveryCall` over `Session` over `None`.
    /// On a tie the receiver (and its reason) wins.
    pub fn stricter(self, other: Self) -> Self {
        let rank = |need: &Self| match need {
            Self::None => 0,
            Self::Session => 1,
            Self::EveryCall { .. } => 2,
        };
        if rank(&other) > rank(&self) {
            other
        } else {
            self
        }
    }
}
