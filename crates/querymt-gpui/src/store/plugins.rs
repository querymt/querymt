//! Provider plugin updates (desktop `updatePluginsForAgent` and the
//! `pluginUpdateStatus` / `pluginUpdateComplete` notifications): what is
//! being downloaded while an update runs, then how it went.

use gpui_kit::*;
use serde_json::{Value, json};

use super::{AgentRuntime, AgentsStore};

/// The plugin update step in progress (querymt `pluginUpdateStatus`).
#[derive(Clone, Debug, PartialEq)]
pub struct PluginProgress {
    pub plugin: String,
    pub phase: String,
    /// Download progress, 0–100, when the size is known.
    pub percent: Option<f64>,
    pub message: Option<String>,
}

impl PluginProgress {
    /// Shown from the click until the agent reports a step.
    fn starting() -> Self {
        Self {
            plugin: "Preparing update".into(),
            phase: "starting".into(),
            percent: None,
            message: Some("Starting plugin update…".into()),
        }
    }

    fn parse(v: &Value) -> Option<Self> {
        let text = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        Some(Self {
            plugin: text("plugin_name")?,
            phase: text("phase").unwrap_or_default(),
            percent: v
                .get("percent")
                .and_then(Value::as_f64)
                .map(|p| p.clamp(0., 100.)),
            message: text("message"),
        })
    }

    /// "phase · 42%".
    pub fn status(&self) -> String {
        match self.percent {
            Some(p) => format!("{} · {p:.0}%", self.phase),
            None => self.phase.clone(),
        }
    }
}

/// Record a finished update from its `results`: the summary line and the
/// plugins that failed.
fn apply_results(rt: &mut AgentRuntime, v: &Value) {
    let results = v
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let text = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).map(str::to_owned);
    let failed: Vec<(String, String)> = results
        .iter()
        .filter(|r| r.get("success").and_then(Value::as_bool) != Some(true))
        .map(|r| {
            (
                text(r, "plugin_name").unwrap_or_default(),
                text(r, "message").unwrap_or_else(|| "Update failed.".into()),
            )
        })
        .collect();
    let ok = results.len() - failed.len();
    rt.maintenance = Some(if failed.is_empty() {
        (
            format!("Updated {ok} plugin{}.", if ok == 1 { "" } else { "s" }),
            false,
        )
    } else {
        (
            format!(
                "{} of {} plugin updates failed.",
                failed.len(),
                results.len()
            ),
            true,
        )
    });
    rt.failed_plugins = failed;
    rt.plugin_progress = None;
}

impl AgentsStore {
    /// Update provider plugins (`querymt/updatePlugins`); the outcome lands in `maintenance`.
    pub fn update_plugins(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(rt) = self.runtime.get_mut(id) {
            rt.maintenance = None;
            rt.plugin_progress = Some(PluginProgress::starting());
        }
        self.ext(
            id,
            "plugins",
            "_querymt/updatePlugins",
            json!({}),
            cx,
            |s, id, res, cx| {
                let Some(rt) = s.runtime.get_mut(id) else {
                    return;
                };
                rt.plugin_progress = None;
                if let Ok(v) = res {
                    apply_results(rt, &v);
                }
                s.refresh_models(id, cx);
            },
        );
    }

    /// `querymt/pluginUpdateStatus` / `querymt/pluginUpdateComplete` from agent `id`.
    pub(super) fn on_plugin_update(
        &mut self,
        id: &str,
        method: &str,
        params: &Value,
        cx: &mut Context<Self>,
    ) {
        let Some(rt) = self.runtime.get_mut(id) else {
            return;
        };
        match method.trim_start_matches('_') {
            "querymt/pluginUpdateStatus" => match PluginProgress::parse(params) {
                Some(progress) => rt.plugin_progress = Some(progress),
                None => return,
            },
            "querymt/pluginUpdateComplete" => apply_results(rt, params),
            _ => return,
        }
        cx.notify();
    }
}

pub(super) fn is_plugin_update(method: &str) -> bool {
    matches!(
        method.trim_start_matches('_'),
        "querymt/pluginUpdateStatus" | "querymt/pluginUpdateComplete"
    )
}

#[cfg(test)]
mod tests {
    use super::{AgentRuntime, PluginProgress, apply_results};
    use serde_json::json;

    #[test]
    fn progress_reads_the_status_notification() {
        let p = PluginProgress::parse(&json!({
            "plugin_name": "openai", "image_reference": "ghcr.io/x", "phase": "downloading",
            "bytes_downloaded": 10, "percent": 41.6, "message": " "
        }))
        .unwrap();
        assert_eq!(p.status(), "downloading · 42%");
        assert_eq!(p.message, None);
        assert_eq!(PluginProgress::parse(&json!({ "phase": "x" })), None);
        assert_eq!(PluginProgress::starting().status(), "starting");
    }

    #[test]
    fn results_say_how_the_update_went() {
        let mut rt = AgentRuntime {
            plugin_progress: Some(PluginProgress::starting()),
            ..Default::default()
        };
        apply_results(
            &mut rt,
            &json!({ "results": [{ "plugin_name": "a", "success": true }] }),
        );
        assert_eq!(rt.maintenance, Some(("Updated 1 plugin.".into(), false)));
        assert_eq!(rt.plugin_progress, None);
        apply_results(
            &mut rt,
            &json!({ "results": [
                { "plugin_name": "a", "success": true },
                { "plugin_name": "b", "success": false }
            ] }),
        );
        assert_eq!(
            rt.maintenance,
            Some(("1 of 2 plugin updates failed.".into(), true))
        );
        assert_eq!(rt.failed_plugins, [("b".into(), "Update failed.".into())]);
    }
}
