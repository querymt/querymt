//! First-run setup: whether the app is ready to start a session (desktop
//! `domain/readiness.ts`) and whether setup was finished or skipped
//! (desktop `domain/onboarding.ts`).

use serde::{Deserialize, Serialize};

use super::{AgentsStore, ConnState, ProviderAuth, find_model};
use crate::util::rfc3339_now;

/// Bump to offer setup again after it gains a step existing users need.
pub const ONBOARDING_VERSION: u32 = 1;

/// When setup was finished or skipped; persisted with the preferences.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Onboarding {
    pub version: u32,
    pub completed_at: Option<String>,
    pub skipped_at: Option<String>,
}

impl Onboarding {
    pub fn done(&self) -> bool {
        self.version >= ONBOARDING_VERSION
            && (self.completed_at.is_some() || self.skipped_at.is_some())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckId {
    Agent,
    Providers,
    Models,
    Model,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckStatus {
    Ok,
    Pending,
    Blocked,
    Warning,
    NotApplicable,
}

/// What resolves an open check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckAction {
    /// Connect (or reconnect) this configured agent.
    RetryAgent(String),
    /// No agent is configured: add one.
    AddAgent,
    /// Sign in or add an API key.
    OpenProviders,
    RefreshProviders(String),
    RefreshModels(String),
    ChooseModel,
}

#[derive(Clone, Debug)]
pub struct Check {
    pub id: CheckId,
    pub title: &'static str,
    pub status: CheckStatus,
    pub reason: Option<String>,
    pub action: Option<CheckAction>,
}

impl Check {
    fn new(
        id: CheckId,
        status: CheckStatus,
        reason: Option<String>,
        action: Option<CheckAction>,
    ) -> Self {
        let title = match id {
            CheckId::Agent => "Agent",
            CheckId::Providers => "Providers",
            CheckId::Models => "Models",
            CheckId::Model => "Model",
        };
        let open = !matches!(status, CheckStatus::Ok | CheckStatus::NotApplicable);
        Self {
            id,
            title,
            status,
            reason: reason.filter(|_| open),
            action: action.filter(|_| open),
        }
    }

    pub fn done(&self) -> bool {
        matches!(self.status, CheckStatus::Ok | CheckStatus::NotApplicable)
    }
}

/// Readiness of the agent new sessions start on.
#[derive(Clone, Debug)]
pub struct Readiness {
    /// Agent the provider and model checks refer to.
    pub agent: Option<String>,
    /// Agent, providers, models, model; in that order.
    pub checks: [Check; 4],
}

impl Readiness {
    pub fn complete(&self) -> bool {
        self.checks.iter().all(Check::done)
    }

    pub fn check(&self, id: CheckId) -> &Check {
        &self.checks[id as usize]
    }

    /// Checks still to resolve, in order.
    pub fn open(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| !c.done())
    }
}

impl ProviderAuth {
    /// Signed in, or an API key is stored or in the environment.
    pub fn usable(&self) -> bool {
        self.oauth_status.as_deref() == Some("connected")
            || self.has_stored_api_key
            || self.has_env_api_key
    }
}

impl AgentsStore {
    /// Desktop `evaluateReadiness`, from the store's current state.
    pub fn readiness(&self) -> Readiness {
        use CheckId::*;
        use CheckStatus::*;

        let agent_id = self.primary_agent();
        let agent = match &agent_id {
            Some(_) => Check::new(Agent, Ok, None, None),
            None if self.configs.is_empty() => Check::new(
                Agent,
                Blocked,
                Some("No agent is configured.".into()),
                Some(CheckAction::AddAgent),
            ),
            None if self
                .configs
                .iter()
                .any(|c| self.state(&c.id) == ConnState::Connecting) =>
            {
                Check::new(Agent, Pending, Some("Starting the agent…".into()), None)
            }
            None => {
                let failing = self
                    .prefs
                    .primary_agent
                    .as_deref()
                    .and_then(|id| self.config(id))
                    .or_else(|| self.configs.first())
                    .expect("configs is not empty");
                let reason = match self.state(&failing.id) {
                    ConnState::Failed(reason) => reason,
                    _ => format!("{} is not running.", failing.name),
                };
                Check::new(
                    Agent,
                    Blocked,
                    Some(reason),
                    Some(CheckAction::RetryAgent(failing.id.clone())),
                )
            }
        };
        let (Some(id), Some(rt)) = (
            agent_id.clone(),
            agent_id.as_deref().and_then(|a| self.runtime(a)),
        ) else {
            let na = |id| Check::new(id, NotApplicable, None, None);
            return Readiness {
                agent: None,
                checks: [agent, na(Providers), na(Models), na(Model)],
            };
        };

        // Providers: only agents that manage sign-in report them.
        let providers = if rt.capabilities.is_none() && !rt.models_loaded {
            Check::new(Providers, Pending, Some("Checking providers…".into()), None)
        } else if !rt.feature("auth") {
            Check::new(Providers, NotApplicable, None, None)
        } else if rt.providers.iter().any(ProviderAuth::usable) {
            Check::new(Providers, Ok, None, None)
        } else if let Some(err) = &rt.providers_error {
            Check::new(
                Providers,
                Blocked,
                Some(format!("Could not load providers: {err}")),
                Some(CheckAction::RefreshProviders(id.clone())),
            )
        } else if !rt.providers_loaded {
            Check::new(Providers, Pending, Some("Checking providers…".into()), None)
        } else {
            let expired: Vec<&str> = rt
                .providers
                .iter()
                .filter(|p| p.oauth_status.as_deref() == Some("expired"))
                .map(|p| p.display_name.as_str())
                .collect();
            if expired.is_empty() {
                Check::new(
                    Providers,
                    Blocked,
                    Some("No provider is connected. Connect one to load models.".into()),
                    Some(CheckAction::OpenProviders),
                )
            } else {
                Check::new(
                    Providers,
                    Warning,
                    Some(format!(
                        "Sign-in expired for {}. Reconnect to continue.",
                        expired.join(", ")
                    )),
                    Some(CheckAction::OpenProviders),
                )
            }
        };

        let models = if !rt.models.is_empty() {
            Check::new(Models, Ok, None, None)
        } else if !rt.models_loaded || rt.pending.contains("models") {
            Check::new(Models, Pending, Some("Loading models…".into()), None)
        } else if matches!(providers.status, Blocked | Warning) {
            Check::new(
                Models,
                Blocked,
                Some("No models available. Connect a provider first.".into()),
                Some(CheckAction::OpenProviders),
            )
        } else {
            Check::new(
                Models,
                Blocked,
                Some("The agent returned no models.".into()),
                Some(CheckAction::RefreshModels(id.clone())),
            )
        };

        let chosen = self
            .prefs
            .launch_models
            .get(&id)
            .and_then(|key| find_model(&rt.models, key));
        let model = if rt.models.is_empty() {
            Check::new(Model, NotApplicable, None, None)
        } else if chosen.is_some() {
            Check::new(Model, Ok, None, None)
        } else {
            Check::new(
                Model,
                Blocked,
                Some("Choose a model for new sessions.".into()),
                Some(CheckAction::ChooseModel),
            )
        };

        Readiness {
            agent: Some(id),
            checks: [agent, providers, models, model],
        }
    }

    /// Agents are done connecting and loading; a first run can decide now.
    pub fn settled(&self) -> bool {
        self.configs.iter().all(|c| match self.state(&c.id) {
            ConnState::Connecting => false,
            ConnState::Connected => self
                .runtime(&c.id)
                .is_none_or(|rt| rt.models_loaded && (rt.providers_loaded || !rt.feature("auth"))),
            _ => true,
        })
    }

    pub fn finish_onboarding(&mut self, skipped: bool) {
        let now = rfc3339_now();
        self.prefs.onboarding = Onboarding {
            version: ONBOARDING_VERSION,
            completed_at: (!skipped).then(|| now.clone()),
            skipped_at: skipped.then_some(now),
        };
        self.first_run = false;
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::super::{AgentConfig, AgentRuntime, Endpoint, HashMap, LaunchState, Prefs};
    use super::*;
    use serde_json::json;

    fn store(state: ConnState, runtime: AgentRuntime) -> AgentsStore {
        AgentsStore {
            configs: vec![AgentConfig {
                id: "a".into(),
                name: "Local qmtcode".into(),
                endpoint: Endpoint::Process {
                    command: "qmtcode --acp".into(),
                },
                auto_start: true,
                host_managed: false,
            }],
            prefs: Prefs::default(),
            launch: LaunchState::default(),
            session: None,
            requests: Vec::new(),
            runtime: HashMap::from([("a".to_string(), AgentRuntime { state, ..runtime })]),
            first_run: false,
            _launch_task: None,
        }
    }

    fn statuses(r: &Readiness) -> [CheckStatus; 4] {
        r.checks.clone().map(|c| c.status)
    }

    fn auth_agent() -> AgentRuntime {
        AgentRuntime {
            capabilities: Some(json!({ "features": { "auth": true } })),
            models_loaded: true,
            providers_loaded: true,
            ..Default::default()
        }
    }

    #[test]
    fn no_agent_blocks_and_offers_to_add_one() {
        let mut s = store(ConnState::Idle, AgentRuntime::default());
        s.configs.clear();
        let r = s.readiness();
        assert_eq!(r.check(CheckId::Agent).action, Some(CheckAction::AddAgent));
        assert!(!r.complete());
        assert_eq!(r.open().count(), 1);
    }

    #[test]
    fn failed_agent_offers_retry_with_its_reason() {
        let s = store(
            ConnState::Failed("spawn failed".into()),
            AgentRuntime::default(),
        );
        let agent = s.readiness().check(CheckId::Agent).clone();
        assert_eq!(agent.status, CheckStatus::Blocked);
        assert_eq!(agent.reason.as_deref(), Some("spawn failed"));
        assert_eq!(agent.action, Some(CheckAction::RetryAgent("a".into())));
        assert!(!s.settled() || s.readiness().agent.is_none());
    }

    #[test]
    fn connecting_agent_is_pending() {
        let s = store(ConnState::Connecting, AgentRuntime::default());
        assert_eq!(
            s.readiness().check(CheckId::Agent).status,
            CheckStatus::Pending
        );
        assert!(!s.settled());
    }

    #[test]
    fn no_provider_blocks_models_and_points_to_providers() {
        let s = store(ConnState::Connected, auth_agent());
        let r = s.readiness();
        use CheckStatus::*;
        assert_eq!(statuses(&r), [Ok, Blocked, Blocked, NotApplicable]);
        assert_eq!(
            r.check(CheckId::Models).action,
            Some(CheckAction::OpenProviders)
        );
        assert!(s.settled());
    }

    #[test]
    fn ready_once_a_model_is_chosen() {
        let mut rt = auth_agent();
        rt.providers = vec![ProviderAuth {
            provider: "codex".into(),
            display_name: "Codex".into(),
            oauth_status: Some("connected".into()),
            has_stored_api_key: false,
            has_env_api_key: false,
            env_var_name: None,
            supports_oauth: true,
            preferred_method: None,
        }];
        rt.models = super::super::parse_models(&json!({ "models": [
            { "id": "codex/m", "provider": "codex", "model": "m" }
        ]}));
        let mut s = store(ConnState::Connected, rt);
        assert_eq!(
            s.readiness().check(CheckId::Model).action,
            Some(CheckAction::ChooseModel)
        );
        s.prefs.launch_models.insert("a".into(), "codex/m".into());
        assert!(s.readiness().complete());
    }

    #[test]
    fn agent_without_auth_skips_providers() {
        let rt = AgentRuntime {
            capabilities: Some(json!({ "features": {} })),
            models_loaded: true,
            ..Default::default()
        };
        let r = store(ConnState::Connected, rt).readiness();
        use CheckStatus::*;
        assert_eq!(statuses(&r), [Ok, NotApplicable, Blocked, NotApplicable]);
        assert_eq!(
            r.check(CheckId::Models).action,
            Some(CheckAction::RefreshModels("a".into()))
        );
    }

    #[test]
    fn onboarding_is_done_once_finished_or_skipped() {
        assert!(!Onboarding::default().done());
        let mut s = store(ConnState::Idle, AgentRuntime::default());
        s.first_run = true;
        s.finish_onboarding(true);
        assert!(s.prefs.onboarding.done() && s.prefs.onboarding.skipped_at.is_some());
        assert!(!s.first_run);
    }
}
