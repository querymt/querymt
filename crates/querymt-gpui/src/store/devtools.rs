//! Developer mode (desktop `chatPreferencesStore.developerMode`): reveals
//! developer-only tools across the app, and records each agent's JSON-RPC
//! traffic for the debug events viewer while it is on.

use gpui_kit::*;

use super::AgentsStore;

impl AgentsStore {
    pub fn set_developer_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        for rt in self.runtime.values() {
            rt.wire.set_enabled(on);
        }
        self.update_prefs(|p| p.developer_mode = on, cx);
    }

    /// Forget the recorded debug events of `agent`.
    pub fn clear_wire_log(&mut self, agent: &str, cx: &mut Context<Self>) {
        if let Some(rt) = self.runtime.get(agent) {
            rt.wire.clear();
            cx.notify();
        }
    }
}
