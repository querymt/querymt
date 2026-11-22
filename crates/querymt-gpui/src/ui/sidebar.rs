//! Sidebar markers (desktop `SidebarAttentionDot`): a dot on sessions
//! waiting for the user, and on Agents when an agent is in trouble.

use gpui_kit::*;

use super::kit;
use crate::theme::Palette;

/// Dot on a session waiting for the user (a permission or a question).
pub(super) fn waiting_dot(session_id: &str, p: &Palette) -> Stateful<Div> {
    let selector = format!("wait-{session_id}");
    div()
        .id(kit::keyed("wait", session_id.to_string()))
        .debug_selector(move || selector.clone())
        .flex_none()
        .size(px(7.))
        .rounded_full()
        .bg(p.accent)
        .tooltip(|window, cx| kit::wrap_tip("Waiting for you").build(window, cx))
}

#[cfg(test)]
mod tests {
    use crate::store::{ConnState, SessionSummary};
    use crate::ui::QmtApp;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
    use serde_json::json;

    #[gpui_kit::test]
    fn waiting_sessions_and_troubled_agents_are_marked(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        let (_rpc, _requests) = store.rpc_for_test("test-agent");
        let rt = store.runtime_mut_for_test("test-agent");
        rt.sessions_loaded = true;
        rt.sessions = ["s1", "s2"]
            .map(|id| SessionSummary {
                agent_id: "test-agent".into(),
                id: id.into(),
                title: id.into(),
                root: true,
                open: true,
                ..Default::default()
            })
            .to_vec();
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        let render = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        };
        let store = app.read_with(cx, |app, _| app.store.clone());
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.request_for_test(
                    "test-agent",
                    9,
                    "session/request_permission",
                    json!({ "sessionId": "s2",
                        "toolCall": { "toolCallId": "t1", "title": "Run ls", "kind": "execute" },
                        "options": [{ "optionId": "o", "name": "Allow", "kind": "allow_once" }] }),
                    cx,
                )
            })
        });
        render(cx);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("wait-s2").is_some());
        assert!(vcx.debug_bounds("wait-s1").is_none());
        assert!(vcx.debug_bounds("agents-attention").is_none());

        cx.update(|cx| {
            store.update(cx, |s, _| {
                s.runtime_mut_for_test("other").state = ConnState::Failed("spawn failed".into());
                s.configs.push(crate::store::AgentConfig {
                    id: "other".into(),
                    name: "Other".into(),
                    ..s.configs[0].clone()
                });
            })
        });
        render(cx);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("agents-attention").is_some());
        store.read_with(cx, |s, _| {
            assert_eq!(
                s.agents_needing_attention(),
                [("Other".to_string(), "failed: spawn failed".to_string())]
            );
        });
    }
}
