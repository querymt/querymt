//! Workspaces page (desktop `routes/workspaces`): saved workspace folders
//! and those of recent sessions, each one click from a new session.
//!
//! A path is added by typing it (with the same completion as Start) or, when
//! the agent runs on this machine, with the system folder picker. Saved
//! folders also lead the completion on Start and in New automation.

use std::collections::HashMap;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::QmtApp;
use super::agents::modal_scrim;
use super::kit;
use super::smooth_scroll::SmoothScroll;
use super::workspace_select::{WorkspaceField, home_dir, local_fs};
use crate::store::AgentsStore;
use crate::theme::{Palette, palette};
use crate::util::{short_time, workspace_name};

/// Recent-session workspaces listed below the saved ones.
const RECENT_MAX: usize = 20;

pub(super) struct WorkspacesPage {
    input: Entity<InputState>,
    field: Entity<WorkspaceField>,
    error: Option<String>,
    /// The system folder picker is open.
    picking: bool,
    /// Saved path awaiting "Remove from Workspaces?".
    confirm_remove: Option<String>,
}

impl WorkspacesPage {
    pub(super) fn new(
        store: Entity<AgentsStore>,
        smooth_scroll: SmoothScroll,
        window: &mut Window,
        cx: &mut Context<QmtApp>,
    ) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("/path/on/the/agent/machine"));
        let field = {
            let input = input.clone();
            cx.new(|cx| WorkspaceField::new(store, input, true, smooth_scroll, cx))
        };
        Self {
            input,
            field,
            error: None,
            picking: false,
            confirm_remove: None,
        }
    }

    /// Enter in the path field (once no suggestion is highlighted) adds it.
    pub(super) fn subscribe(&self, window: &mut Window, cx: &mut Context<QmtApp>) -> Subscription {
        cx.subscribe_in(
            &self.input,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => this.add_typed_workspace(window, cx),
                InputEvent::Change => {
                    this.workspaces.error = None;
                    cx.notify();
                }
                _ => {}
            },
        )
    }
}

/// A workspace row: saved, or seen in recent sessions.
struct Row {
    path: String,
    saved: bool,
    sessions: usize,
    /// Latest `updated_at` of its sessions.
    last_used: Option<String>,
}

/// Session count and latest activity per workspace, from local sessions.
fn usage(store: &AgentsStore) -> HashMap<String, (usize, String)> {
    let mut out: HashMap<String, (usize, String)> = HashMap::new();
    for s in store
        .all_sessions()
        .into_iter()
        .filter(|s| s.root && s.remote.is_none())
    {
        let cwd = s.group_cwd();
        if cwd.trim().is_empty() {
            continue;
        }
        let entry = out.entry(cwd.to_owned()).or_default();
        entry.0 += 1;
        if s.updated_at > entry.1 {
            entry.1 = s.updated_at.clone();
        }
    }
    out
}

/// Saved rows in their saved order, then up to [`RECENT_MAX`] others by
/// latest use.
fn rows(store: &AgentsStore) -> (Vec<Row>, Vec<Row>) {
    let usage = usage(store);
    let row = |path: &str, saved: bool| {
        let (sessions, last) = usage.get(path).cloned().unwrap_or_default();
        Row {
            path: path.to_owned(),
            saved,
            sessions,
            last_used: (!last.is_empty()).then_some(last),
        }
    };
    let saved: Vec<Row> = store
        .prefs
        .workspaces
        .iter()
        .map(|p| row(p, true))
        .collect();
    let mut recent: Vec<Row> = usage
        .keys()
        .filter(|p| !store.prefs.workspaces.contains(p))
        .map(|p| row(p, false))
        .collect();
    recent.sort_by(|a, b| {
        b.last_used
            .cmp(&a.last_used)
            .then_with(|| a.path.cmp(&b.path))
    });
    recent.truncate(RECENT_MAX);
    (saved, recent)
}

/// A typed path as saved: trimmed, `~` expanded when the agent is local,
/// without a trailing separator. Absolute paths only: sessions start in
/// them on the agent's machine.
fn normalize(input: &str, home: Option<&str>) -> Result<String, &'static str> {
    let path = input.trim();
    if path.is_empty() {
        return Err("Enter a folder path.");
    }
    let path = match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            format!("{}{rest}", home.trim_end_matches('/'))
        }
        _ => path.to_owned(),
    };
    let windows = |p: &str| {
        let b = p.as_bytes();
        (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'))
            || p.starts_with("\\\\")
    };
    if !path.starts_with('/') && !windows(&path) {
        return Err("Enter an absolute path, like /home/me/project.");
    }
    let trimmed = path.trim_end_matches(['/', '\\']);
    Ok(if trimmed.is_empty() || trimmed.ends_with(':') {
        path
    } else {
        trimmed.to_owned()
    })
}

impl QmtApp {
    fn workspace_local(&self, cx: &App) -> bool {
        local_fs(self.store.read(cx), None)
    }

    /// Save `path`; on this machine it must be an existing folder.
    fn save_workspace(&mut self, path: &str, cx: &mut Context<Self>) -> bool {
        let local = self.workspace_local(cx);
        let home = if local { home_dir() } else { None };
        let path = match normalize(path, home.as_deref()) {
            Ok(path) => path,
            Err(error) => {
                self.workspaces.error = Some(error.into());
                cx.notify();
                return false;
            }
        };
        if local && cfg!(not(target_family = "wasm")) && !std::path::Path::new(&path).is_dir() {
            self.workspaces.error = Some(format!("No folder at {path}."));
            cx.notify();
            return false;
        }
        self.workspaces.error = None;
        if !self
            .store
            .update(cx, |s, cx| s.add_workspace(path.clone(), cx))
        {
            self.workspaces.error = Some(format!("{} is saved already.", workspace_name(&path)));
        }
        cx.notify();
        true
    }

    fn add_typed_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.workspaces.input.read(cx).value().to_string();
        if self.save_workspace(&value, cx) {
            self.workspaces
                .input
                .update(cx, |s, cx| s.set_value(SharedString::default(), window, cx));
        }
    }

    fn pick_workspace_folder(&mut self, cx: &mut Context<Self>) {
        if self.workspaces.picking {
            return;
        }
        self.workspaces.picking = true;
        self.workspaces.error = None;
        cx.notify();
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = rx.await;
            let _ = this.update(cx, |this, cx| {
                this.workspaces.picking = false;
                match picked {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.first() {
                            this.save_workspace(&path.to_string_lossy(), cx);
                        }
                    }
                    Ok(Err(err)) => {
                        this.workspaces.error =
                            Some(format!("Could not open the folder picker: {err}"))
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Prefill Start with `path` and focus the prompt.
    fn use_workspace(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.cwd
            .update(cx, |s, cx| s.set_value(path.clone(), window, cx));
        self.store.update(cx, |s, _| s.set_cwd(path));
        self.new_session(window, cx);
    }

    pub(super) fn close_workspace_overlay(&mut self) -> bool {
        self.workspaces.confirm_remove.take().is_some()
    }

    pub(super) fn render_workspace_overlay(
        &self,
        radii: Corners<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let path = self.workspaces.confirm_remove.clone()?;
        let p = *palette(cx);
        let card = self
            .modal_card(&p)
            .w(px(440.))
            .child(div().text_lg().font_semibold().child(format!(
                "Remove \u{201c}{}\u{201d} from Workspaces?",
                workspace_name(&path)
            )))
            .child(div().text_sm().text_color(p.muted).child(
                "Only this shortcut is removed. The folder and everything in it stay on disk.",
            ))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        kit::flat_btn("workspace-remove-cancel", true, true, &p)
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.workspaces.confirm_remove = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        kit::accent_btn("workspace-remove-ok", true, true, &p)
                            .child("Remove")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.workspaces.confirm_remove = None;
                                this.store.update(cx, |s, cx| s.remove_workspace(&path, cx));
                                cx.notify();
                            })),
                    ),
            );
        Some(self.centered(modal_scrim("scrim-workspace-remove", radii, &p), card, cx))
    }

    pub(super) fn render_workspaces(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *palette(cx);
        let can_pick = cfg!(not(target_family = "wasm")) && self.workspace_local(cx);
        let host = self.store.read(cx).primary_agent().and_then(|a| {
            self.store
                .read(cx)
                .runtime(&a)?
                .mesh
                .as_ref()?
                .hostname
                .clone()
        });
        let (saved, recent) = rows(self.store.read(cx));

        let mut actions = Vec::new();
        if can_pick {
            let picking = self.workspaces.picking;
            actions.push(
                kit::flat_btn("workspace-pick", !picking, true, &p)
                    .child(if picking {
                        kit::spinner("workspace-pick-spin", 14., p.muted).into_any_element()
                    } else {
                        kit::icon(IconName::FolderPlus, 14., p.text).into_any_element()
                    })
                    .child(if picking {
                        "Opening folder picker…"
                    } else {
                        "Add folder"
                    })
                    .when(!picking, |el| {
                        el.on_click(cx.listener(|this, _, _, cx| this.pick_workspace_folder(cx)))
                    })
                    .into_any_element(),
            );
        }
        let header = kit::page_header(
            "Workspaces",
            None,
            Some("Folders to start sessions in, on the machine where the agent runs."),
            actions,
            &p,
        );

        // ---- Add a path
        let typed = !self.workspaces.input.read(cx).value().trim().is_empty();
        let add = v_flex()
            .gap_1p5()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.workspaces.field.clone()),
                    )
                    .child(
                        // Full-size button: the height of the 36px path field.
                        kit::flat_btn("workspace-add", typed, false, &p)
                            .child("Add path")
                            .when(typed, |el| {
                                el.on_click(cx.listener(|this, _, window, cx| {
                                    this.add_typed_workspace(window, cx)
                                }))
                            }),
                    ),
            )
            .child(match &self.workspaces.error {
                Some(error) => h_flex()
                    .gap_1p5()
                    .text_xs()
                    .text_color(p.danger)
                    .child(kit::icon(IconName::CircleAlert, 13., p.danger))
                    .child(error.clone()),
                None => div().text_xs().text_color(p.dim).child(match &host {
                    Some(host) => format!("An absolute path on {host}, where the agent runs."),
                    None => "An absolute path on the machine where the agent runs.".into(),
                }),
            });

        let saved_count = saved.len();
        let saved_section = section(
            "Saved",
            saved_count,
            if saved.is_empty() {
                kit::card(&p)
                    .child(
                        h_flex()
                            .gap_3()
                            .px(px(14.4))
                            .py_4()
                            .child(kit::icon_tile(IconName::FolderOpen, false, &p))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_sm().font_semibold().child("No saved workspaces yet"))
                                    .child(div().text_xs().text_color(p.muted).child(
                                        if can_pick {
                                            "Add a folder or a path above, or save one from your recent sessions."
                                        } else {
                                            "Add a path above, or save one from your recent sessions."
                                        },
                                    )),
                            ),
                    )
                    .into_any_element()
            } else {
                self.workspace_rows(saved, &p, cx).into_any_element()
            },
            &p,
        );
        let recent_section = (!recent.is_empty()).then(|| {
            section(
                "From recent sessions",
                recent.len(),
                self.workspace_rows(recent, &p, cx).into_any_element(),
                &p,
            )
        });

        kit::page(
            [
                header.into_any_element(),
                add.into_any_element(),
                saved_section.into_any_element(),
            ]
            .into_iter()
            .chain(recent_section.map(IntoElement::into_any_element)),
        )
    }

    fn workspace_rows(&self, rows: Vec<Row>, p: &Palette, cx: &mut Context<Self>) -> Div {
        let (_, hover) = kit::subtle(p);
        let current = self.cwd.read(cx).value().trim().to_string();
        let items: Vec<AnyElement> = rows
            .into_iter()
            .enumerate()
            .map(|(ix, row)| {
                let key = row.path.clone();
                let in_use = row.path == current;
                let meta = match (row.sessions, &row.last_used) {
                    (0, _) => "No sessions yet".to_string(),
                    (n, Some(at)) => format!(
                        "{n} {} · last active {}",
                        if n == 1 { "session" } else { "sessions" },
                        match short_time(at).as_str() {
                            "now" => "just now".to_string(),
                            t if t.ends_with(['m', 'h', 'd']) => format!("{t} ago"),
                            t => t.to_string(),
                        }
                    ),
                    (n, None) => format!("{n} {}", if n == 1 { "session" } else { "sessions" }),
                };
                let use_path = row.path.clone();
                let save_path = row.path.clone();
                let remove_path = row.path.clone();
                let tip_path = row.path.clone();
                kit::card_row(ix == 0, p)
                    .id(kit::keyed("workspace-row", key.clone()))
                    .hover(move |s| s.bg(hover))
                    .child(kit::icon_tile(
                        if row.saved {
                            IconName::Folder
                        } else {
                            IconName::FolderClock
                        },
                        in_use,
                        p,
                    ))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .text_size(kit::rpx(13.4))
                                            .font_semibold()
                                            .truncate()
                                            .child(workspace_name(&row.path)),
                                    )
                                    .when(in_use, |el| el.child(kit::chip("On Start", p))),
                            )
                            .child(
                                div()
                                    .id(kit::keyed("workspace-path", key.clone()))
                                    .mt(px(2.))
                                    .text_xs()
                                    .font_family(kit::mono_family(cx))
                                    .text_color(p.muted)
                                    .truncate()
                                    .child(row.path.clone())
                                    .tooltip(move |window, cx| {
                                        kit::wrap_tip(tip_path.clone()).build(window, cx)
                                    }),
                            )
                            .child(div().mt(px(1.)).text_xs().text_color(p.dim).child(meta)),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .child(
                                kit::flat_btn(
                                    kit::keyed("workspace-use", key.clone()),
                                    true,
                                    true,
                                    p,
                                )
                                .child("Use")
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.use_workspace(use_path.clone(), window, cx)
                                    },
                                )),
                            )
                            // Flat square buttons the height of Use, on the same fill.
                            .child(if row.saved {
                                kit::flat_icon_btn(
                                    kit::keyed("workspace-remove", key),
                                    IconName::Trash,
                                    "Remove from Workspaces",
                                    p,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.workspaces.confirm_remove = Some(remove_path.clone());
                                    cx.notify();
                                }))
                                .into_any_element()
                            } else {
                                kit::flat_icon_btn(
                                    kit::keyed("workspace-save", key),
                                    IconName::BookmarkPlus,
                                    "Save to Workspaces",
                                    p,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.store
                                        .update(cx, |s, cx| s.add_workspace(save_path.clone(), cx));
                                }))
                                .into_any_element()
                            }),
                    )
                    .into_any_element()
            })
            .collect();
        kit::card(p).children(items).child(kit::card_mask(p))
    }
}

fn section(title: &'static str, count: usize, body: AnyElement, p: &Palette) -> Div {
    v_flex()
        .gap_3()
        .child(
            h_flex()
                .gap_2()
                .child(div().font_semibold().child(title))
                .child(kit::chip(count.to_string(), p)),
        )
        .child(body)
}

#[cfg(test)]
mod tests {
    use super::super::Page;
    use super::{QmtApp, normalize};

    #[gpui_kit::test]
    fn add_use_and_remove_a_workspace(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use gpui_kit::test::TestWindowExt as _;

        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
        });
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| crate::store::automations::schedule_ui_fixture());
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        let dir = std::env::temp_dir()
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| app.go(Page::Workspaces, cx));
            window.render_frame(cx);
            app.update(cx, |app, cx| {
                // Relative paths and missing folders are refused.
                assert!(!app.save_workspace("relative/dir", cx));
                assert!(app.workspaces.error.is_some());
                assert!(!app.save_workspace("/no/such/folder/for/qmt", cx));
                assert!(app.save_workspace(&format!("{dir}/"), cx));
                assert_eq!(
                    app.store.read(cx).prefs.workspaces,
                    std::slice::from_ref(&dir)
                );
            });
            window.render_frame(cx);
            app.update(cx, |app, cx| app.use_workspace(dir.clone(), window, cx));
            window.render_frame(cx);
            app.update(cx, |app, cx| {
                assert_eq!(app.page, Page::Start);
                assert_eq!(app.cwd.read(cx).value().as_ref(), dir.as_str());
                app.store.update(cx, |s, cx| s.remove_workspace(&dir, cx));
                assert!(app.store.read(cx).prefs.workspaces.is_empty());
            });
        })
        .unwrap();
    }

    #[test]
    fn normalizes_absolute_paths_only() {
        assert_eq!(normalize("  /srv/app/ ", None), Ok("/srv/app".into()));
        assert_eq!(normalize("/", None), Ok("/".into()));
        assert_eq!(
            normalize("~/code", Some("/home/me")),
            Ok("/home/me/code".into())
        );
        assert_eq!(normalize("~", Some("/home/me/")), Ok("/home/me".into()));
        assert_eq!(normalize("C:\\work\\", None), Ok("C:\\work".into()));
        assert!(normalize("~/code", None).is_err());
        assert!(normalize("relative/dir", None).is_err());
        assert!(normalize("   ", None).is_err());
    }
}
