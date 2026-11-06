use super::*;
use std::num::NonZeroU64;
use std::time::Duration;

/// `[agent.permissions]`, `[planner.permissions]` and `[delegates.permissions]`.
///
/// ```toml
/// [agent.permissions]
/// ask         = ["shell"]             # prompt; "Allow for this session" offered
/// always_ask  = ["ibkr.place_order"]  # prompt on every call
/// timeout_sec = 120                   # reject unanswered prompts; unset = wait
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionsConfig {
    /// Tools that need the user's approval. The prompt offers "Allow once",
    /// "Allow for this session" and "Reject".
    pub ask: Vec<ToolSpec>,

    /// Tools that prompt on every call and never offer the session option.
    /// Wins over `ask`.
    pub always_ask: Vec<ToolSpec>,

    /// Seconds after which an unanswered approval prompt (from a list or a
    /// hook) is rejected. Prompts wait indefinitely when unset.
    pub timeout_sec: Option<NonZeroU64>,
}

impl PermissionsConfig {
    /// The runtime form of this section.
    pub fn to_permissions(&self) -> Permissions {
        Permissions {
            ask: self.ask.iter().cloned().collect(),
            always_ask: self.always_ask.iter().cloned().collect(),
            timeout: self.timeout_sec.map(|s| Duration::from_secs(s.get())),
        }
    }

    /// Every selector of both lists, `ask` first.
    pub fn selectors(&self) -> impl Iterator<Item = (&'static str, &ToolSpec)> {
        self.ask.iter().map(|spec| ("permissions.ask", spec)).chain(
            self.always_ask
                .iter()
                .map(|s| ("permissions.always_ask", s)),
        )
    }
}

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

/// Runtime approval policy of an agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Permissions {
    pub ask: ToolSet,
    pub always_ask: ToolSet,
    pub timeout: Option<Duration>,
}

impl Permissions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds tools that prompt, with a session option.
    #[must_use]
    pub fn ask(mut self, tools: impl IntoIterator<Item = ToolSpec>) -> Self {
        self.ask.extend(tools);
        self
    }

    /// Adds tools that prompt on every call.
    #[must_use]
    pub fn always_ask(mut self, tools: impl IntoIterator<Item = ToolSpec>) -> Self {
        self.always_ask.extend(tools);
        self
    }

    /// Rejects approval prompts left unanswered for `timeout`.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// The approval the lists require for `tool`: `always_ask` over `ask`
    /// over none. Hook decisions are combined by the caller.
    pub fn need(&self, tool: ToolRef<'_>) -> ApprovalNeed {
        if self.always_ask.contains(tool) {
            ApprovalNeed::EveryCall { reason: None }
        } else if self.ask.contains(tool) {
            ApprovalNeed::Session
        } else {
            ApprovalNeed::None
        }
    }

    /// `server.tool` entries that name a server present in `snapshot` but none
    /// of its discovered tools (typos such as `placeOrder` for `place_order`).
    /// Sorted and deduplicated.
    pub fn unmatched(&self, snapshot: &crate::agent::core::McpToolSnapshot) -> Vec<ToolSpec> {
        let mut discovered: HashMap<&str, HashSet<&str>> = HashMap::new();
        for (name, adapter) in &snapshot.tools {
            discovered
                .entry(adapter.server_name())
                .or_default()
                .insert(name.as_str());
        }
        let mut out: Vec<ToolSpec> = self
            .ask
            .specs()
            .into_iter()
            .chain(self.always_ask.specs())
            .filter(|spec| match spec {
                ToolSpec::McpSpecific(server, name) => discovered
                    .get(server.as_str())
                    .is_some_and(|names| !names.contains(name.as_str())),
                _ => false,
            })
            .collect();
        out.sort_by_key(ToString::to_string);
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stricter_orders_every_call_over_session_over_none() {
        let every = || ApprovalNeed::EveryCall { reason: None };
        assert_eq!(
            ApprovalNeed::None.stricter(ApprovalNeed::Session),
            ApprovalNeed::Session
        );
        assert_eq!(
            ApprovalNeed::Session.stricter(ApprovalNeed::None),
            ApprovalNeed::Session
        );
        assert_eq!(ApprovalNeed::Session.stricter(every()), every());
        assert_eq!(every().stricter(ApprovalNeed::Session), every());
        assert_eq!(
            ApprovalNeed::None.stricter(ApprovalNeed::None),
            ApprovalNeed::None
        );
    }

    fn perms() -> Permissions {
        let cfg: PermissionsConfig = toml::from_str(
            r#"
            ask = ["shell", "ibkr.*"]
            always_ask = ["ibkr.place_order"]
            timeout_sec = 120
            "#,
        )
        .unwrap();
        cfg.to_permissions()
    }

    #[test]
    fn always_ask_wins_over_ask_wins_over_none() {
        let p = perms();
        assert_eq!(
            p.need(ToolRef::mcp("ibkr", "place_order")),
            ApprovalNeed::EveryCall { reason: None }
        );
        assert_eq!(p.need(ToolRef::mcp("ibkr", "quote")), ApprovalNeed::Session);
        assert_eq!(p.need(ToolRef::builtin("shell")), ApprovalNeed::Session);
        assert_eq!(p.need(ToolRef::builtin("read_tool")), ApprovalNeed::None);
        assert_eq!(p.need(ToolRef::mcp("other", "shell")), ApprovalNeed::None);
    }

    #[test]
    fn timeout_converts_to_a_duration() {
        assert_eq!(perms().timeout, Some(Duration::from_secs(120)));
        assert_eq!(PermissionsConfig::default().to_permissions().timeout, None);
    }

    #[test]
    fn zero_timeout_and_unknown_keys_are_rejected() {
        assert!(toml::from_str::<PermissionsConfig>("timeout_sec = 0").is_err());
        let err = toml::from_str::<PermissionsConfig>("remember = true")
            .unwrap_err()
            .to_string();
        assert!(err.contains("remember"), "{err}");
    }

    #[tokio::test]
    async fn unmatched_reports_specific_entries_without_a_discovered_tool() {
        use crate::test_utils::mcp_harness::{HarnessOptions, McpHarness};
        let harness = McpHarness::start(HarnessOptions::default()).await;
        let p = Permissions::new()
            .always_ask(["harness.placeOrder".parse().unwrap()])
            .ask([
                "harness.echo".parse().unwrap(),
                "harness.*".parse().unwrap(),
                "shell".parse().unwrap(),
                "unconnected.tool".parse().unwrap(),
            ]);

        let unmatched = p.unmatched(&harness.tool_state.load());

        assert_eq!(unmatched, vec!["harness.placeOrder".parse().unwrap()]);
    }
}
