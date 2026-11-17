//! Workspace path field of the Start composer with completion: the
//! workspaces the session list knows about, plus (for an agent on this
//! machine) the folders under the typed path, both fuzzy-matched.
//!
//! Up/Down move the highlight, Enter or a click picks a suggestion, Tab
//! completes into a folder (`path/`) and keeps listing its children, Escape
//! closes the list.

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Enter, Escape, IndentInline, Input, MoveDown, MoveUp};
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{QmtApp, kit};
use crate::store::Endpoint;
use crate::theme::Palette;
use crate::util::{fuzzy_score, workspace_name};

/// Known workspaces shown at most (they come before folders).
const MAX_WORKSPACES: usize = 8;
/// Suggestions shown at most.
const MAX_SUGGESTIONS: usize = 50;
/// Folder names read from one directory at most.
#[cfg(not(target_family = "wasm"))]
const MAX_DIR_ENTRIES: usize = 2000;
const LIST_MAX_H: f32 = 280.;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Suggestion {
    pub path: String,
    /// From the session list's workspaces rather than a directory listing.
    pub workspace: bool,
}

/// `~` expanded to `home` (when known).
fn expand(input: &str, home: Option<&str>) -> String {
    let input = input.trim();
    match (input.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            format!(
                "{}{}",
                home.trim_end_matches('/'),
                if rest.is_empty() { "/" } else { rest }
            )
        }
        _ => input.to_string(),
    }
}

/// Directory to list (with a trailing `/`) and the partial name typed after
/// it; `None` unless the input is an absolute path.
fn dir_query(input: &str, home: Option<&str>) -> Option<(String, String)> {
    let expanded = expand(input, home);
    if !expanded.starts_with('/') {
        return None;
    }
    let (base, partial) = expanded.rsplit_once('/')?;
    Some((format!("{base}/"), partial.to_string()))
}

/// Suggestions for `input`: matching workspaces (best first, recency breaks
/// ties), then the folders of `listing` (base dir and names) that match the
/// partial name. Hidden folders only show once a `.` is typed.
fn suggestions(
    input: &str,
    home: Option<&str>,
    workspaces: &[String],
    listing: Option<(&str, &[String])>,
) -> Vec<Suggestion> {
    let query = expand(input, home);
    let typed = query.trim_end_matches('/');
    let mut ws: Vec<(u32, &String)> = workspaces
        .iter()
        .filter(|w| w.trim_end_matches('/') != typed)
        .filter_map(|w| Some((fuzzy_score(&query, &[&workspace_name(w), w])?, w)))
        .collect();
    ws.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    let mut out: Vec<Suggestion> = ws
        .into_iter()
        .take(MAX_WORKSPACES)
        .map(|(_, w)| Suggestion {
            path: w.clone(),
            workspace: true,
        })
        .collect();

    if let (Some((base, partial)), Some((listed, names))) = (dir_query(input, home), listing)
        && listed == base
    {
        let hidden = partial.starts_with('.');
        let mut dirs: Vec<(u32, &String)> = names
            .iter()
            .filter(|n| hidden || !n.starts_with('.'))
            .filter_map(|n| Some((fuzzy_score(&partial, &[n])?, n)))
            .collect();
        dirs.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
        });
        for (_, name) in dirs {
            let path = format!("{base}{name}");
            if path != typed && !out.iter().any(|s| s.path.trim_end_matches('/') == path) {
                out.push(Suggestion {
                    path,
                    workspace: false,
                });
            }
        }
    }
    out.truncate(MAX_SUGGESTIONS);
    out
}

/// Directory holding `path`, shown dimmed after its name.
fn parent_dir(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/".into(),
        Some(ix) => trimmed[..ix].into(),
        None => String::new(),
    }
}

/// `host:port` that names this machine.
fn is_loopback(address: &str) -> bool {
    let host = address
        .rsplit_once(':')
        .map_or(address, |(host, _)| host)
        .trim_matches(['[', ']']);
    matches!(host, "" | "localhost" | "::1") || host.starts_with("127.")
}

#[cfg(not(target_family = "wasm"))]
fn list_dirs(base: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        // `Path::is_dir` follows symlinks, so linked folders are offered too.
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .take(MAX_DIR_ENTRIES)
        .collect()
}

/// The browser has no filesystem; [`QmtApp::local_fs`] is false there.
#[cfg(target_family = "wasm")]
fn list_dirs(_base: &str) -> Vec<String> {
    Vec::new()
}

#[cfg(not(target_family = "wasm"))]
fn home_dir() -> Option<String> {
    dirs::home_dir().map(|h| h.to_string_lossy().into_owned())
}

#[cfg(target_family = "wasm")]
fn home_dir() -> Option<String> {
    None
}

impl QmtApp {
    /// The agent sessions start on runs on this machine, so its paths can be
    /// completed from the local filesystem.
    fn local_fs(&self, cx: &App) -> bool {
        if cfg!(target_family = "wasm") {
            return false;
        }
        let store = self.store.read(cx);
        match store
            .primary_agent()
            .and_then(|a| store.config(&a))
            .map(|c| &c.endpoint)
        {
            Some(Endpoint::WebSocket { address, .. }) => is_loopback(address),
            Some(Endpoint::Process { .. }) | None => true,
        }
    }

    /// Workspaces of the session list (local sessions, most recent first),
    /// led by the last workspace used.
    fn known_workspaces(&self, cx: &App) -> Vec<String> {
        let store = self.store.read(cx);
        let mut sessions: Vec<_> = store
            .all_sessions()
            .into_iter()
            .filter(|s| s.remote.is_none())
            .collect();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let mut out: Vec<String> = Vec::new();
        for w in
            std::iter::once(store.prefs.cwd.as_str()).chain(sessions.iter().map(|s| s.group_cwd()))
        {
            if !w.trim().is_empty() && !out.iter().any(|o| o == w) {
                out.push(w.to_string());
            }
        }
        out
    }

    fn workspace_suggestions(&self, cx: &App) -> Vec<Suggestion> {
        if !self.ws_open {
            return Vec::new();
        }
        let local = self.local_fs(cx);
        let home = local.then(home_dir).flatten();
        let listing = self
            .ws_dirs
            .as_ref()
            .filter(|_| local)
            .map(|(base, names)| (base.as_str(), names.as_slice()));
        suggestions(
            &self.cwd.read(cx).value(),
            home.as_deref(),
            &self.known_workspaces(cx),
            listing,
        )
    }

    /// The field changed or gained focus: reset the highlight and list the
    /// typed directory in the background when it is not listed yet.
    pub(super) fn refresh_workspace_dirs(&mut self, cx: &mut Context<Self>) {
        self.ws_cursor = 0;
        self.ws_scroll.scroll_to_item(0);
        if !self.local_fs(cx) {
            return;
        }
        let home = home_dir();
        let Some((base, _)) = dir_query(&self.cwd.read(cx).value(), home.as_deref()) else {
            return;
        };
        if self.ws_dirs.as_ref().is_some_and(|(b, _)| *b == base)
            || self.ws_loading.as_deref() == Some(base.as_str())
        {
            return;
        }
        self.ws_loading = Some(base.clone());
        // Replacing the task drops (cancels) a listing of an older directory.
        self.ws_task = Some(cx.spawn(async move |this, cx| {
            let dir = base.clone();
            let names = cx
                .background_executor()
                .spawn(async move { list_dirs(&dir) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.ws_loading.as_deref() == Some(base.as_str()) {
                    this.ws_loading = None;
                }
                this.ws_dirs = Some((base, names));
                cx.notify();
            });
        }));
    }

    /// Focus: list again, so folders created meanwhile show up.
    pub(super) fn open_workspace_suggestions(&mut self, cx: &mut Context<Self>) {
        self.ws_open = true;
        self.ws_dirs = None;
        self.ws_loading = None;
        self.refresh_workspace_dirs(cx);
        cx.notify();
    }

    fn pick_workspace(
        &mut self,
        path: String,
        drill: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = if drill && !path.ends_with('/') {
            format!("{path}/")
        } else {
            path
        };
        self.cwd.update(cx, |s, cx| s.set_value(value, window, cx));
        self.ws_open = drill;
        self.refresh_workspace_dirs(cx);
        cx.notify();
    }

    fn move_workspace_cursor(&mut self, down: bool, cx: &mut Context<Self>) -> bool {
        let len = self.workspace_suggestions(cx).len();
        if len == 0 {
            return false;
        }
        let cursor = self.ws_cursor.min(len - 1);
        self.ws_cursor = if down {
            (cursor + 1) % len
        } else {
            (cursor + len - 1) % len
        };
        self.ws_scroll.scroll_to_item(self.ws_cursor);
        cx.notify();
        true
    }

    /// Pick the highlighted suggestion; `drill` completes into it instead.
    fn accept_workspace(
        &mut self,
        drill: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let items = self.workspace_suggestions(cx);
        let Some(item) = items.get(self.ws_cursor.min(items.len().saturating_sub(1))) else {
            return false;
        };
        self.pick_workspace(item.path.clone(), drill, window, cx);
        true
    }

    /// The workspace path input and its suggestion list.
    pub(super) fn workspace_field(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let items = self.workspace_suggestions(cx);
        let cursor = self.ws_cursor.min(items.len().saturating_sub(1));
        let field = h_flex()
            .px_2()
            .rounded(px(8.))
            .bg(p.card)
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                if this.move_workspace_cursor(false, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                if this.move_workspace_cursor(true, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Enter, window, cx| {
                if this.accept_workspace(false, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if this.accept_workspace(true, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                if !this.workspace_suggestions(cx).is_empty() {
                    this.ws_open = false;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(kit::icon(IconName::FolderSearch, 15., p.dim))
            .child(Input::new(&self.cwd).appearance(false).flex_1());

        let (hover, text, dim) = (p.card_hover, p.text, p.dim);
        let popover = (!items.is_empty()).then(|| {
            let rows: Vec<AnyElement> = items
                .into_iter()
                .enumerate()
                .map(|(ix, item)| {
                    let path = item.path.clone();
                    h_flex()
                        .id(kit::keyed("ws-suggestion", item.path.clone()))
                        .flex_none()
                        .h(px(30.))
                        .px_2()
                        .gap_2()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .hover(move |s| s.bg(hover))
                        .when(ix == cursor, |el| el.bg(hover))
                        .child(kit::icon(
                            if item.workspace {
                                IconName::FolderClock
                            } else {
                                IconName::Folder
                            },
                            14.,
                            dim,
                        ))
                        .child(
                            div()
                                .flex_none()
                                .max_w(px(260.))
                                .truncate()
                                .font_medium()
                                .text_color(text)
                                .child(workspace_name(&item.path)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.2))
                                .text_color(dim)
                                .child(parent_dir(&item.path)),
                        )
                        .when(item.workspace, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.))
                                    .text_color(dim)
                                    .child("Workspace"),
                            )
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.pick_workspace(path.clone(), false, window, cx)
                        }))
                        .into_any_element()
                })
                .collect();
            deferred(
                v_flex()
                    .id("ws-suggestions")
                    .absolute()
                    .top(relative(1.))
                    .left_0()
                    .right_0()
                    .mt_1()
                    .p_1()
                    .max_h(px(LIST_MAX_H))
                    .overflow_y_scroll()
                    .track_scroll(&self.ws_scroll)
                    .rounded(px(10.))
                    .bg(p.surface)
                    .border_1()
                    .border_color(p.border)
                    .shadow_lg()
                    .occlude()
                    // Keep focus in the field: the shell would take it on
                    // mouse down and the blur would close the list first.
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .children(rows)
                    .map(|el| self.smooth_scroll.region("workspace-list", el)),
            )
            .with_priority(1)
        });
        div().relative().w_full().child(field).children(popover)
    }
}

#[cfg(test)]
mod tests {
    // Not a glob: `gpui_kit::*` would shadow the `#[test]` attribute.
    use super::{Suggestion, dir_query, is_loopback, parent_dir, suggestions};

    fn paths(items: Vec<Suggestion>) -> Vec<String> {
        items.into_iter().map(|s| s.path).collect()
    }

    #[test]
    fn dir_query_splits_base_and_partial() {
        assert_eq!(dir_query("/", None), Some(("/".into(), "".into())));
        assert_eq!(dir_query("/ho", None), Some(("/".into(), "ho".into())));
        assert_eq!(
            dir_query("/home/iv", None),
            Some(("/home/".into(), "iv".into()))
        );
        assert_eq!(
            dir_query("~/pr", Some("/home/me")),
            Some(("/home/me/".into(), "pr".into()))
        );
        assert_eq!(
            dir_query("~", Some("/home/me/")),
            Some(("/home/me/".into(), "".into()))
        );
        assert_eq!(dir_query("~/x", None), None);
        assert_eq!(dir_query("proj", None), None);
    }

    #[test]
    fn folders_fuzzy_match_the_partial_name() {
        let names: Vec<String> = ["bin", "boot", "home", "projects", ".cache"]
            .map(String::from)
            .to_vec();
        let listing = Some(("/", names.as_slice()));
        assert_eq!(
            paths(suggestions("/", None, &[], listing)),
            ["/bin", "/boot", "/home", "/projects"]
        );
        assert_eq!(
            paths(suggestions("/prj", None, &[], listing)),
            ["/projects"]
        );
        assert_eq!(paths(suggestions("/.c", None, &[], listing)), ["/.cache"]);
        // A listing of another directory is ignored.
        assert!(suggestions("/home/x", None, &[], listing).is_empty());
    }

    #[test]
    fn workspaces_lead_and_match_by_name_or_path() {
        let ws: Vec<String> = ["/w/querymt-gpui", "/w/website"].map(String::from).to_vec();
        let names: Vec<String> = ["querymt-gpui", "qmcp"].map(String::from).to_vec();
        let listing = Some(("/w/", names.as_slice()));
        assert_eq!(paths(suggestions("", None, &ws, None)), ws);
        assert_eq!(
            paths(suggestions("gpui", None, &ws, None)),
            ["/w/querymt-gpui"]
        );
        let got = suggestions("/w/q", None, &ws, listing);
        // The workspace is not repeated as a folder.
        assert_eq!(paths(got.clone()), ["/w/querymt-gpui", "/w/qmcp"]);
        assert!(got[0].workspace && !got[1].workspace);
        // The exact current value is not suggested.
        assert_eq!(
            paths(suggestions("/w/website", None, &ws, None)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(parent_dir("/bin"), "/");
        assert_eq!(parent_dir("/a/b/"), "/a");
        assert!(
            is_loopback("127.0.0.1:4000") && is_loopback("localhost:1") && is_loopback("[::1]:9")
        );
        assert!(!is_loopback("lstr:4000"));
    }
}
