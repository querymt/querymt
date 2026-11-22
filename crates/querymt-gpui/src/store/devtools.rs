//! Developer mode (desktop `chatPreferencesStore.developerMode`): reveals
//! developer-only tools across the app.

use gpui_kit::*;

use super::AgentsStore;

impl AgentsStore {
    pub fn set_developer_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_prefs(|p| p.developer_mode = on, cx);
    }
}
