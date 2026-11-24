//! The slash command list of the session composer (design D6), and the hint
//! strip shown once a command with input has been completed.
//!
//! The list is derived on every render from the composer text, the cursor,
//! the focus and the session's commands; the only state kept is the
//! highlighted row (`slash_cursor`), the text Esc closed the list at
//! (`slash_dismissed`) and the scroll handle.

use gpui_kit::component::input::{Enter, IndentInline, MoveDown, MoveUp, Position};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use querymt_agent_session_core::view::SlashCommandView;

use super::{Page, QmtApp, kit};
use crate::theme::Palette;

/// Rows visible at once; more scroll.
const MAX_ROWS: usize = 8;
const ROW_HEIGHT: f32 = 30.;

/// The text typed after the leading `/`, up to the cursor, when the list
/// applies: the text starts with `/` and the text before the cursor holds no
/// whitespace (the cursor is still in the first word, after the `/`).
fn slash_query(text: &str, cursor: usize) -> Option<&str> {
    if !text.starts_with('/') || cursor == 0 || !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    if before.contains(char::is_whitespace) {
        return None;
    }
    Some(&before[1..])
}

/// `text` with its first word replaced by `/name `, keeping what follows,
/// and the cursor column (on the first line) just after that space. A space
/// already following the first word is reused, not doubled.
fn complete(text: &str, name: &str) -> (String, u32) {
    let name = name.trim_start_matches('/');
    let first_end = text.find(char::is_whitespace).unwrap_or(text.len());
    let rest = &text[first_end..];
    let mut out = format!("/{name}");
    let column = out.chars().count() as u32 + 1;
    if !rest.starts_with([' ', '\t']) {
        out.push(' ');
    }
    out.push_str(rest);
    (out, column)
}

/// The command whose hint is shown: the text is exactly `/name ` and that
/// command takes input with a hint. Typing anything more hides it.
fn hinted<'a>(text: &str, commands: &'a [SlashCommandView]) -> Option<&'a SlashCommandView> {
    let name = text.strip_prefix('/')?.strip_suffix(' ')?;
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    commands
        .iter()
        .find(|c| c.name == name && c.input_hint.as_deref().is_some_and(|h| !h.is_empty()))
}

/// The commands the list shows, best match first.
pub(super) struct SlashList {
    pub(super) matches: Vec<SlashCommandView>,
}

impl QmtApp {
    /// The list as it would render now, or `None` when it is not shown: not
    /// on the session page, the composer unfocused, the text not a command
    /// word, Esc having closed it for this text, or nothing to list. Every
    /// slash key handler asks this, so keys act only while the list shows.
    pub(super) fn slash_list(&self, window: &Window, cx: &App) -> Option<SlashList> {
        if self.page != Page::Session {
            return None;
        }
        if !Focusable::focus_handle(&self.session_prompt, cx).is_focused(window) {
            return None;
        }
        let state = self.session_prompt.read(cx);
        let text = state.value();
        let query = slash_query(&text, state.cursor())?;
        if self.slash_dismissed.as_deref() == Some(text.as_ref()) {
            return None;
        }
        let commands = self.store.read(cx).session_slash_commands();
        // Names only: descriptions make short queries match unrelated
        // commands (the palette is where descriptions are searched).
        let ranked = crate::search::rank(query, commands, |c| vec![c.name.clone()]);
        if ranked.is_empty() {
            return None;
        }
        Some(SlashList {
            matches: ranked
                .into_iter()
                .map(|(ix, _)| commands[ix].clone())
                .collect(),
        })
    }

    /// The command to hint at below the completed `/name `, as `(name, hint)`.
    pub(super) fn slash_hint(&self, cx: &App) -> Option<(String, String)> {
        if self.page != Page::Session {
            return None;
        }
        let text = self.session_prompt.read(cx).value();
        let commands = self.store.read(cx).session_slash_commands();
        hinted(&text, commands).map(|c| (c.name.clone(), c.input_hint.clone().unwrap_or_default()))
    }

    /// Move the highlight (wrapping). Returns whether the list is shown.
    fn slash_move(&mut self, down: bool, window: &Window, cx: &mut Context<Self>) -> bool {
        let Some(list) = self.slash_list(window, cx) else {
            return false;
        };
        let len = list.matches.len();
        let at = self.slash_cursor.min(len - 1);
        self.slash_cursor = if down {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        self.slash_scroll.scroll_to_item(self.slash_cursor);
        cx.notify();
        true
    }

    /// Complete the highlighted command. Returns whether the list is shown.
    fn slash_complete_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(list) = self.slash_list(window, cx) else {
            return false;
        };
        let at = self.slash_cursor.min(list.matches.len() - 1);
        self.complete_slash(&list.matches[at].name, window, cx);
        true
    }

    /// Replace the composer's first word with `/name `, keep the arguments
    /// typed after it, put the cursor after the space and keep the focus.
    pub(super) fn complete_slash(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.session_prompt.read(cx).value().to_string();
        let (text, column) = complete(&text, name);
        self.session_prompt.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            // Moves the caret and focuses the composer.
            state.set_cursor_position(Position::new(0, column), window, cx);
        });
        self.slash_cursor = 0;
        self.slash_dismissed = None;
        cx.notify();
    }

    /// Esc: close the list for the current text. Returns whether it was
    /// shown (the Esc is then spent: it neither closes anything else nor
    /// counts toward the double Esc that stops a turn).
    pub(super) fn close_slash_list(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        if self.slash_list(window, cx).is_none() {
            return false;
        }
        self.slash_dismissed = Some(self.session_prompt.read(cx).value().to_string());
        true
    }

    /// Up, Down, Enter and Tab of the composer while the list is shown.
    /// Registered before the composer's own handlers; each lets the action
    /// through when the list is not shown (and Enter when it would not send).
    pub(super) fn slash_keys(&self, el: Div, cx: &mut Context<Self>) -> Div {
        el.capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
            if this.slash_move(false, window, cx) {
                cx.stop_propagation();
            }
        }))
        .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
            if this.slash_move(true, window, cx) {
                cx.stop_propagation();
            }
        }))
        .capture_action(cx.listener(|this, a: &Enter, window, cx| {
            let send = this.store.read(cx).prefs.send_shortcut;
            if send.submits(a.shift, a.secondary) && this.slash_complete_highlighted(window, cx) {
                cx.stop_propagation();
            }
        }))
        .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
            if this.slash_complete_highlighted(window, cx) {
                cx.stop_propagation();
            }
        }))
    }

    /// The popover above the composer: `/name`, description and hint per
    /// row, at most [`MAX_ROWS`] visible, rows complete on click.
    pub(super) fn render_slash_list(
        &self,
        list: &SlashList,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *p;
        let selected = self.slash_cursor.min(list.matches.len() - 1);
        let rows = list.matches.iter().enumerate().map(|(ix, command)| {
            let name = command.name.clone();
            let hint = command.input_hint.clone().filter(|h| !h.is_empty());
            h_flex()
                .id(("slash-row", ix))
                .debug_selector(move || format!("slash-row-{ix}"))
                .h(px(ROW_HEIGHT))
                .flex_none()
                .px_2()
                .gap_3()
                .items_center()
                .rounded(px(8.))
                .cursor_pointer()
                .hover(move |el| el.bg(p.card_hover))
                .when(ix == selected, |el| el.bg(p.card_hover))
                .child(
                    div()
                        .flex_none()
                        .font_family(kit::MONO)
                        .text_sm()
                        .text_color(p.text)
                        .child(format!("/{}", command.name)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(p.muted)
                        .child(command.description.clone()),
                )
                .children(hint.map(|h| {
                    div()
                        .flex_none()
                        .max_w(px(180.))
                        .truncate()
                        .text_xs()
                        .text_color(p.dim)
                        .child(h)
                }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.complete_slash(&name, window, cx);
                }))
        });
        v_flex()
            .id("slash-list")
            .debug_selector(|| "slash-list".into())
            .absolute()
            .bottom(relative(1.))
            .left_0()
            .mb_1()
            .w_full()
            .max_w(px(560.))
            .rounded(px(12.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .occlude()
            .child(
                v_flex()
                    .id("slash-rows")
                    .p_1()
                    .max_h(px(ROW_HEIGHT * MAX_ROWS as f32 + 8.))
                    .overflow_y_scroll()
                    .track_scroll(&self.slash_scroll)
                    .children(rows),
            )
            .into_any_element()
    }

    /// One dimmed line above the textarea: `/name` and the command's hint.
    pub(super) fn render_slash_hint(&self, name: String, hint: String, p: &Palette) -> Div {
        h_flex()
            .gap_2()
            .px_1()
            .pt_1()
            .text_xs()
            .text_color(p.dim)
            .debug_selector(|| "slash-hint".into())
            .child(div().font_family(kit::MONO).child(format!("/{name}")))
            .child(div().min_w_0().truncate().child(hint))
    }
}

#[cfg(test)]
mod pure_tests {
    use super::{complete, hinted, slash_query};
    use querymt_agent_session_core::view::SlashCommandView;

    fn command(name: &str, hint: Option<&str>) -> SlashCommandView {
        SlashCommandView {
            name: name.into(),
            description: String::new(),
            input_hint: hint.map(Into::into),
        }
    }

    #[test]
    fn the_query_is_the_text_after_the_slash_up_to_the_cursor() {
        assert_eq!(slash_query("/", 1), Some(""));
        assert_eq!(slash_query("/wt", 3), Some("wt"));
        // The cursor inside the word: the text before it.
        assert_eq!(slash_query("/compact", 4), Some("com"));
        // The cursor before the slash.
        assert_eq!(slash_query("/wt", 0), None);
        // Past the first word.
        assert_eq!(slash_query("/compact focus", 14), None);
        assert_eq!(slash_query("/compact ", 9), None);
        // Back in the first word of a longer text.
        assert_eq!(slash_query("/com focus", 4), Some("com"));
        // Not a command word.
        assert_eq!(slash_query("see a/b", 7), None);
        assert_eq!(slash_query("", 0), None);
        assert_eq!(slash_query(" /wt", 4), None);
        // A cursor inside a multi-byte character is not a position.
        assert_eq!(slash_query("/é", 2), None);
        assert_eq!(slash_query("/é", 3), Some("é"));
    }

    #[test]
    fn completion_replaces_the_first_word_and_keeps_the_arguments() {
        assert_eq!(complete("/com", "compact"), ("/compact ".into(), 9));
        assert_eq!(complete("/", "worktree"), ("/worktree ".into(), 10));
        // Typed arguments stay, with one space between.
        assert_eq!(
            complete("/com focus on the API", "compact"),
            ("/compact focus on the API".into(), 9)
        );
        assert_eq!(
            complete("/com\nsecond line", "compact"),
            ("/compact \nsecond line".into(), 9)
        );
        // A bare or slash-prefixed name gives one slash.
        assert_eq!(complete("/c", "/compact"), ("/compact ".into(), 9));
    }

    #[test]
    fn the_hint_shows_only_for_the_exact_completed_command() {
        let commands = [
            command("compact", Some("[instructions]")),
            command("worktree", None),
            command("empty", Some("")),
        ];
        let name = |text| hinted(text, &commands).map(|c| c.name.as_str());
        assert_eq!(name("/compact "), Some("compact"));
        assert_eq!(name("/compact"), None);
        assert_eq!(name("/compact  "), None);
        assert_eq!(name("/compact focus"), None);
        assert_eq!(name("/worktree "), None, "no hint to show");
        assert_eq!(name("/empty "), None, "an empty hint is no hint");
        assert_eq!(name("/unknown "), None);
        assert_eq!(name("/ "), None);
    }
}

#[cfg(test)]
mod ui_tests {
    // Not `super::*`: the gpui glob brings a `test` macro that shadows `#[test]`.
    use crate::session::SessionView;
    use crate::ui::{Page, QmtApp};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext,
    };
    use serde_json::{Value, json};

    struct Rig {
        handle: AnyWindowHandle,
        app: Entity<QmtApp>,
        requests: async_channel::Receiver<String>,
    }

    fn two_commands() -> Value {
        json!([
            { "name": "compact", "description": "Summarize the conversation",
              "input": { "hint": "[optional instructions]" } },
            { "name": "worktree", "description": "Work in a git worktree" },
        ])
    }

    /// A session of "test-agent" with `commands` and a two-value mode option.
    fn session(commands: Value, running: bool) -> SessionView {
        let mut view = SessionView::new("test-agent", "s1", "/w", "t");
        view.session_update(&json!({ "sessionId": "s1", "update": {
            "sessionUpdate": "available_commands_update", "availableCommands": commands } }));
        view.options = vec![json!({
            "id": "mode", "type": "select", "currentValue": "build",
            "options": [{ "value": "build", "name": "Build" }, { "value": "plan", "name": "Plan" }],
        })];
        view.mode = Some("build".into());
        if running {
            view.phase = "model".into();
        }
        view
    }

    /// The Session page with the composer focused.
    fn rig(cx: &mut TestAppContext, view: SessionView, control: bool) -> Rig {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::launch::peer_fixture();
        let (_rpc, requests) = store.rpc_for_test("test-agent");
        if control {
            store.runtime_mut_for_test("test-agent").capabilities = Some(json!({
                "features": { "steering": true },
                "methods": ["querymt/session/steer", "querymt/session/queue",
                            "querymt/session/runtimeState"],
            }));
        }
        store.session = Some(view);
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                app.session_prompt.update(cx, |s, cx| s.focus(window, cx));
            });
            window.render_frame(cx);
        })
        .unwrap();
        Rig {
            handle,
            app,
            requests,
        }
    }

    fn render(cx: &mut TestAppContext, rig: &Rig) {
        cx.update_window(rig.handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    fn type_text(cx: &mut TestAppContext, rig: &Rig, text: &str) {
        VisualTestContext::from_window(rig.handle, cx).simulate_input(text);
        render(cx, rig);
    }

    fn keys(cx: &mut TestAppContext, rig: &Rig, keys: &str) {
        VisualTestContext::from_window(rig.handle, cx).simulate_keystrokes(keys);
        render(cx, rig);
    }

    fn text(cx: &mut TestAppContext, rig: &Rig) -> String {
        rig.app.read_with(cx, |app, cx| {
            app.session_prompt.read(cx).value().to_string()
        })
    }

    /// Names in the list as it renders now (empty when it is not shown).
    fn listed(cx: &mut TestAppContext, rig: &Rig) -> Vec<String> {
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.read_with(cx, |app, cx| {
                app.slash_list(window, cx)
                    .map(|l| l.matches.into_iter().map(|c| c.name).collect())
                    .unwrap_or_default()
            })
        })
        .unwrap()
    }

    fn focused(cx: &mut TestAppContext, rig: &Rig) -> bool {
        cx.update_window(rig.handle, |_, window, cx| {
            let prompt = rig.app.read(cx).session_prompt.clone();
            gpui_kit::Focusable::focus_handle(&prompt, cx).is_focused(window)
        })
        .unwrap()
    }

    fn sent(rig: &Rig) -> Vec<Value> {
        std::iter::from_fn(|| rig.requests.try_recv().ok())
            .map(|line| serde_json::from_str(&line).expect("a JSON-RPC line"))
            .collect()
    }

    fn methods(requests: &[Value]) -> Vec<&str> {
        requests
            .iter()
            .filter_map(|r| r["method"].as_str())
            .collect()
    }

    fn mode(cx: &mut TestAppContext, rig: &Rig) -> Option<String> {
        rig.app.read_with(cx, |app, cx| {
            app.store
                .read(cx)
                .session
                .as_ref()
                .and_then(|v| v.mode.clone())
        })
    }

    #[gpui_kit::test]
    fn slash_shows_all_commands(cx: &mut TestAppContext) {
        let rig = rig(cx, session(two_commands(), false), false);
        assert!(listed(cx, &rig).is_empty(), "nothing typed, no list");
        type_text(cx, &rig, "/");
        assert_eq!(listed(cx, &rig), ["compact", "worktree"]);
        // Rendered: the popover and a row per command.
        let mut vcx = VisualTestContext::from_window(rig.handle, cx);
        assert!(vcx.debug_bounds("slash-list").is_some());
        assert!(vcx.debug_bounds("slash-row-0").is_some());
        assert!(vcx.debug_bounds("slash-row-1").is_some());
        assert!(vcx.debug_bounds("slash-row-2").is_none());
    }

    #[gpui_kit::test]
    fn typing_narrows_by_name(cx: &mut TestAppContext) {
        let rig = rig(cx, session(two_commands(), false), false);
        type_text(cx, &rig, "/wt");
        assert_eq!(listed(cx, &rig), ["worktree"]);
        let mut vcx = VisualTestContext::from_window(rig.handle, cx);
        assert!(vcx.debug_bounds("slash-row-0").is_some());
        assert!(vcx.debug_bounds("slash-row-1").is_none());
        // Descriptions are not matched here: "git" is only in /worktree's.
        keys(cx, &rig, "backspace backspace backspace");
        type_text(cx, &rig, "/git");
        assert!(listed(cx, &rig).is_empty());
        let mut vcx = VisualTestContext::from_window(rig.handle, cx);
        assert!(
            vcx.debug_bounds("slash-list").is_none(),
            "nothing matches, no list"
        );
    }

    #[gpui_kit::test]
    fn slash_inside_text_shows_nothing(cx: &mut TestAppContext) {
        let rig = rig(cx, session(two_commands(), false), false);
        type_text(cx, &rig, "see a/b");
        assert!(listed(cx, &rig).is_empty());
        // Also a command word after other text, and past the first word.
        keys(cx, &rig, "cmd-a backspace");
        type_text(cx, &rig, "/compact focus /w");
        assert!(listed(cx, &rig).is_empty());
    }

    #[gpui_kit::test]
    fn no_commands_no_list(cx: &mut TestAppContext) {
        let rig = rig(cx, session(json!([]), false), false);
        type_text(cx, &rig, "/");
        assert!(listed(cx, &rig).is_empty());
        let mut vcx = VisualTestContext::from_window(rig.handle, cx);
        assert!(vcx.debug_bounds("slash-list").is_none());
    }

    #[gpui_kit::test]
    fn list_shows_at_most_eight_rows(cx: &mut TestAppContext) {
        let many: Vec<Value> = (0..12)
            .map(|n| json!({ "name": format!("cmd{n:02}"), "description": "d" }))
            .collect();
        let rig = rig(cx, session(json!(many), false), false);
        type_text(cx, &rig, "/cmd");
        assert_eq!(listed(cx, &rig).len(), 12);
        let mut vcx = VisualTestContext::from_window(rig.handle, cx);
        let list = vcx.debug_bounds("slash-list").expect("the list");
        let row = vcx.debug_bounds("slash-row-0").expect("a row");
        // At most eight rows tall (plus padding and border); the rest scrolls.
        assert!(list.size.height <= row.size.height * 8.0 + gpui_kit::px(12.));
        assert!(list.size.height >= row.size.height * 7.0);
    }

    #[gpui_kit::test]
    fn click_completes_and_keeps_focus(cx: &mut TestAppContext) {
        let rig = rig(cx, session(two_commands(), false), false);
        type_text(cx, &rig, "/");
        let mut vcx = VisualTestContext::from_window(rig.handle, cx);
        let row = vcx.debug_bounds("slash-row-1").expect("/worktree");
        vcx.simulate_click(row.center(), Modifiers::none());
        render(cx, &rig);
        assert_eq!(text(cx, &rig), "/worktree ");
        assert!(focused(cx, &rig), "the composer keeps focus");
        assert!(listed(cx, &rig).is_empty(), "completed, so closed");
        let cursor = rig.app.read_with(cx, |app, cx| {
            let c = app.session_prompt.read(cx).cursor_position();
            (c.line, c.character)
        });
        assert_eq!(cursor, (0, 10));
        assert!(sent(&rig).is_empty(), "nothing is sent");
    }

    #[gpui_kit::test]
    fn arrows_move_the_highlight(cx: &mut TestAppContext) {
        let rig = rig(cx, session(two_commands(), false), false);
        type_text(cx, &rig, "/");
        keys(cx, &rig, "down");
        assert_eq!(rig.app.read_with(cx, |a, _| a.slash_cursor), 1);
        assert_eq!(
            text(cx, &rig),
            "/",
            "the arrow did not move the text cursor"
        );
        keys(cx, &rig, "down");
        assert_eq!(rig.app.read_with(cx, |a, _| a.slash_cursor), 0, "wraps");
        keys(cx, &rig, "up");
        assert_eq!(
            rig.app.read_with(cx, |a, _| a.slash_cursor),
            1,
            "wraps back"
        );
        keys(cx, &rig, "enter");
        assert_eq!(
            text(cx, &rig),
            "/worktree ",
            "the highlighted command completes"
        );
        // A new query starts at the best match again.
        keys(cx, &rig, "cmd-a backspace");
        type_text(cx, &rig, "/");
        assert_eq!(rig.app.read_with(cx, |a, _| a.slash_cursor), 0);
    }

    #[gpui_kit::test]
    fn tab_completes_without_cycling_mode(cx: &mut TestAppContext) {
        let rig = rig(cx, session(two_commands(), false), false);
        type_text(cx, &rig, "/com");
        keys(cx, &rig, "tab");
        assert_eq!(text(cx, &rig), "/compact ");
        assert_eq!(mode(cx, &rig).as_deref(), Some("build"));
        assert!(sent(&rig).is_empty(), "no mode change, no prompt");
    }

    #[gpui_kit::test]
    fn enter_completes_instead_of_sending(cx: &mut TestAppContext) {
        let rig = rig(cx, session(two_commands(), false), false);
        type_text(cx, &rig, "/wor");
        keys(cx, &rig, "enter");
        assert_eq!(text(cx, &rig), "/worktree ");
        assert!(sent(&rig).is_empty(), "no prompt is sent");
    }

    #[gpui_kit::test]
    fn completion_keeps_typed_arguments(cx: &mut TestAppContext) {
        let rig = rig(cx, session(two_commands(), false), false);
        type_text(cx, &rig, "/com focus on the API");
        assert!(
            listed(cx, &rig).is_empty(),
            "the cursor is past the first word"
        );
        // Back into the first word: the list returns, Tab keeps the rest.
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.update(cx, |app, cx| {
                app.session_prompt.update(cx, |s, cx| {
                    s.set_cursor_position(
                        gpui_kit::component::input::Position::new(0, 4),
                        window,
                        cx,
                    )
                })
            });
            window.render_frame(cx);
        })
        .unwrap();
        assert_eq!(listed(cx, &rig), ["compact"]);
        keys(cx, &rig, "tab");
        assert_eq!(text(cx, &rig), "/compact focus on the API");
    }

    #[gpui_kit::test]
    fn esc_closes_the_list_and_does_not_stop_the_turn(cx: &mut TestAppContext) {
        let rig = rig(cx, session(two_commands(), true), true);
        type_text(cx, &rig, "/co");
        assert_eq!(listed(cx, &rig), ["compact"]);
        keys(cx, &rig, "escape");
        assert!(listed(cx, &rig).is_empty(), "the first Esc closes the list");
        assert_eq!(text(cx, &rig), "/co", "the text is unchanged");
        assert!(
            rig.app.read_with(cx, |a, _| a.last_escape.is_none()),
            "and is not counted"
        );
        // A second Esc right after: it is the first of a double Esc, no stop.
        keys(cx, &rig, "escape");
        assert!(
            !methods(&sent(&rig)).contains(&"session/cancel"),
            "the turn is not stopped"
        );
        // Typing more opens the list again.
        type_text(cx, &rig, "m");
        assert_eq!(listed(cx, &rig), ["compact"]);
    }

    #[gpui_kit::test]
    fn send_after_completion_sends_arguments(cx: &mut TestAppContext) {
        let rig = rig(cx, session(two_commands(), false), false);
        type_text(cx, &rig, "/compact focus on the API");
        assert!(listed(cx, &rig).is_empty());
        keys(cx, &rig, "enter");
        cx.run_until_parked();
        let requests = sent(&rig);
        let prompts: Vec<&str> = requests
            .iter()
            .filter(|r| r["method"] == "session/prompt")
            .map(|r| r["params"]["prompt"][0]["text"].as_str().unwrap())
            .collect();
        assert_eq!(prompts, ["/compact focus on the API"]);
    }

    #[gpui_kit::test]
    fn keys_are_normal_once_closed(cx: &mut TestAppContext) {
        let rig = rig(cx, session(two_commands(), false), false);
        type_text(cx, &rig, "/co");
        keys(cx, &rig, "escape");
        // Tab is the mode cycle again, not a completion.
        keys(cx, &rig, "tab");
        cx.run_until_parked();
        assert_eq!(text(cx, &rig), "/co");
        let requests = sent(&rig);
        assert!(
            methods(&requests).contains(&"session/set_config_option"),
            "Tab cycles the mode: {requests:?}"
        );
        // Enter sends the text as typed.
        keys(cx, &rig, "enter");
        cx.run_until_parked();
        let requests = sent(&rig);
        assert!(
            requests
                .iter()
                .any(|r| r["method"] == "session/prompt"
                    && r["params"]["prompt"][0]["text"] == "/co"),
            "{requests:?}"
        );
    }

    #[gpui_kit::test]
    fn hint_shows_until_arguments_are_typed(cx: &mut TestAppContext) {
        let rig = rig(cx, session(two_commands(), false), false);
        let hint_shown = |cx: &mut TestAppContext| {
            VisualTestContext::from_window(rig.handle, cx)
                .debug_bounds("slash-hint")
                .is_some()
        };
        // Completing a command with input shows its hint.
        type_text(cx, &rig, "/com");
        keys(cx, &rig, "tab");
        assert_eq!(text(cx, &rig), "/compact ");
        assert!(hint_shown(cx), "the hint shows after completion");
        // The same after the palette puts `/compact ` in the composer.
        type_text(cx, &rig, "focus");
        assert!(!hint_shown(cx), "arguments typed: the hint is gone");
        keys(cx, &rig, "cmd-a backspace");
        assert!(!hint_shown(cx), "no command, no hint");
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.update(cx, |app, cx| {
                app.run_slash_command("compact", true, window, cx)
            });
            window.render_frame(cx);
        })
        .unwrap();
        assert_eq!(text(cx, &rig), "/compact ");
        assert!(hint_shown(cx), "the palette path shows it too");
        // A command without a hint shows none.
        keys(cx, &rig, "cmd-a backspace");
        type_text(cx, &rig, "/worktree ");
        assert!(!hint_shown(cx));
    }
}
