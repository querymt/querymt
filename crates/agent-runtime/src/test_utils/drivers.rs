//! A scripted middleware: each hook performs its acts in turn, the last one
//! repeating, and passes when it has none. Every call is logged.

use crate::middleware::{
    EndVerdict, HookCx, LlmResponse, Middleware, MiddlewareError, ReplyVerdict, Result,
    StepVerdict, Stop, StopVerdict, TurnInfo, TurnMiddleware,
};
use async_trait::async_trait;
use querymt_agent_core::model::NoticeSource;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// What a scripted hook does: say a verdict, queue a notice and say it, fail,
/// or never return (until the run is cancelled).
#[derive(Debug, Clone)]
pub enum Act<V> {
    Say(V),
    Notice(&'static str, V),
    Fail,
    Hang,
}

struct Script<V>(VecDeque<Act<V>>);

impl<V> Default for Script<V> {
    fn default() -> Self {
        Self(VecDeque::new())
    }
}

#[derive(Default)]
struct State {
    turn_start: Script<StepVerdict>,
    step_start: Script<StepVerdict>,
    after_llm: Script<ReplyVerdict>,
    turn_end: Script<EndVerdict>,
    stopped: Script<StopVerdict>,
    /// `(hook, session)` of every call, in order.
    calls: Vec<(&'static str, String)>,
}

/// A middleware that does what its scripts say. Clones share the scripts and the
/// log, which every run of every session consumes and extends.
#[derive(Clone, Default)]
pub struct ScriptedMiddleware(Arc<Mutex<State>>);

macro_rules! script {
    ($($hook:ident: $verdict:ty),*) => {$(
        pub fn $hook(self, acts: impl IntoIterator<Item = Act<$verdict>>) -> Self {
            self.0.lock().unwrap().$hook.0 = acts.into_iter().collect();
            self
        }
    )*};
}

impl ScriptedMiddleware {
    script!(turn_start: StepVerdict, step_start: StepVerdict, after_llm: ReplyVerdict,
        turn_end: EndVerdict, stopped: StopVerdict);

    /// `(hook, session)` of every call so far.
    pub fn calls(&self) -> Vec<(&'static str, String)> {
        self.0.lock().unwrap().calls.clone()
    }

    /// How many times `hook` was called.
    pub fn count(&self, hook: &str) -> usize {
        self.calls()
            .iter()
            .filter(|(name, _)| *name == hook)
            .count()
    }

    async fn act<V: Clone>(
        &self,
        hook: &'static str,
        cx: &mut HookCx<'_>,
        pick: impl FnOnce(&mut State) -> &mut Script<V>,
        pass: V,
    ) -> Result<V> {
        let act = {
            let mut state = self.0.lock().unwrap();
            state.calls.push((hook, cx.session_id().to_string()));
            let script = &mut pick(&mut state).0;
            if script.len() > 1 {
                script.pop_front()
            } else {
                script.front().cloned()
            }
        };
        match act {
            None => Ok(pass),
            Some(Act::Say(verdict)) => Ok(verdict),
            Some(Act::Notice(text, verdict)) => {
                cx.queue_notice(NoticeSource::DuplicateToolCall, text);
                Ok(verdict)
            }
            Some(Act::Fail) => Err(MiddlewareError::ExecutionError("test error".into())),
            Some(Act::Hang) => std::future::pending().await,
        }
    }
}

impl Middleware for ScriptedMiddleware {
    fn name(&self) -> &'static str {
        "ScriptedMiddleware"
    }

    fn begin_turn(&self, _turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware> {
        Box::new(self.clone())
    }
}

#[async_trait]
impl TurnMiddleware for ScriptedMiddleware {
    async fn on_turn_start(&mut self, cx: &mut HookCx<'_>) -> Result<StepVerdict> {
        self.act(
            "turn_start",
            cx,
            |s| &mut s.turn_start,
            StepVerdict::Proceed,
        )
        .await
    }

    async fn on_step_start(&mut self, cx: &mut HookCx<'_>) -> Result<StepVerdict> {
        self.act(
            "step_start",
            cx,
            |s| &mut s.step_start,
            StepVerdict::Proceed,
        )
        .await
    }

    async fn on_after_llm(&mut self, cx: &mut HookCx<'_>, _: &LlmResponse) -> Result<ReplyVerdict> {
        self.act("after_llm", cx, |s| &mut s.after_llm, ReplyVerdict::Proceed)
            .await
    }

    async fn on_turn_end(&mut self, cx: &mut HookCx<'_>) -> Result<EndVerdict> {
        self.act("turn_end", cx, |s| &mut s.turn_end, EndVerdict::Finish)
            .await
    }

    async fn on_stopped(&mut self, cx: &mut HookCx<'_>, _: &Stop) -> Result<StopVerdict> {
        self.act("stopped", cx, |s| &mut s.stopped, StopVerdict::Pass)
            .await
    }
}
