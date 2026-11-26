//! The model picker: search, "Recent", models grouped by provider with
//! context size, input modalities and mesh node, and the reasoning-effort
//! control (design D12).
//!
//! One component serves the main window (Start, Session, Setup and the
//! delegate dialog) and the quick panel. What a pick does depends on its
//! [`Target`]: a session's model is set by the picker itself; for a launch
//! the picker only reports the choice, so the caller decides where it goes
//! (the start page writes the launch preference, the quick panel keeps it for
//! its next chat).

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState, MoveDown, MoveUp};
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use std::cell::Cell;
use std::rc::Rc;

use super::kit::{self, ButtonExt as _};
use super::smooth_scroll::SmoothScroll;
use crate::session::open::OpenSession;
use crate::store::{AgentsStore, LAUNCH_REASONING, Model};
use crate::theme::{Palette, palette};

/// Rows shown per provider group before "N more" (searching shows everything).
const GROUP_ROW_LIMIT: usize = 40;

/// What a pick applies to.
#[derive(Clone)]
pub(crate) enum Target {
    /// The model of this session: the picker sets it.
    Session(Entity<OpenSession>),
    /// The model of a session about to start on `agent`: the picker only
    /// reports the choice.
    Launch { agent: String },
}

impl Target {
    /// The agent whose models are listed.
    pub(crate) fn agent(&self, cx: &App) -> String {
        match self {
            Self::Session(session) => session.read(cx).key.agent.to_string(),
            Self::Launch { agent } => agent.clone(),
        }
    }
}

/// What the picker tells its owner.
#[derive(Clone, Debug)]
pub(crate) enum ModelPickerEvent {
    /// A model was chosen, by selection key ([`Model::key`]). A session
    /// target has already been changed.
    Picked { key: String },
}

/// How [`ModelPicker::card`] is framed.
pub(crate) enum PickerFrame {
    /// A card of its own, `width` wide and at most `max_h` tall (the main
    /// window's modal).
    Card { width: Pixels, max_h: Pixels },
    /// No frame: it fills what it is placed in and lies on that surface (the
    /// quick panel, design D17). `probe` (a measuring canvas of the host,
    /// absolutely placed) goes over the whole sheet; see
    /// [`ModelPicker::sheet_height`].
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    // used by the desktop-only quick panel
    Sheet { probe: AnyElement },
}

/// Space under the last row, inside the rows' scrolling box.
const LIST_PAD_BOTTOM: f32 = 8.;

/// Model rows a sheet makes room for; the rest scroll (search narrows them).
#[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
const SHEET_MODEL_ROWS: usize = 5;

/// Presentation snapshot of the reasoning-effort control.
struct ReasoningControl {
    /// Session config option to update; `None` edits the launch preference.
    config_id: Option<String>,
    current: String,
    /// `(value, label)` pairs, in display order.
    choices: Vec<(String, String)>,
    /// A change was sent and the agent has not confirmed it yet.
    pending: bool,
}

pub(crate) struct ModelPicker {
    store: Entity<AgentsStore>,
    /// Search field.
    query: Entity<InputState>,
    open: bool,
    target: Option<Target>,
    /// The reasoning-effort control is shown in the modal. A host that must
    /// not touch the launch preference turns it off for launch targets.
    launch_reasoning: bool,
    /// Recent models to list instead of the agent's own (a delegate's).
    recent: Option<Vec<Model>>,
    /// The selection key shown as chosen instead of the target's own.
    /// `Some(None)`: nothing is chosen.
    selected: Option<Option<String>>,
    /// The row the keyboard is on, as an index among the pickable rows.
    /// `None` until an arrow key moves it: it then rests on the chosen model
    /// (or the first one while searching).
    cursor: Option<usize>,
    /// Colours in place of the app's, for a host with a look of its own.
    palette: Option<Palette>,
    /// Scroll position of the list in [`Self::card`].
    scroll: ScrollHandle,
    /// Index, among the list's children, of the row that ends the first
    /// [`SHEET_MODEL_ROWS`] models (headings count as children, not models).
    /// `None`: fewer models than that, so the whole list is wanted.
    sheet_cap: Rc<Cell<Option<usize>>>,
    smooth_scroll: SmoothScroll,
    _subs: Vec<Subscription>,
}

/// One provider group as the list shows it.
struct Group {
    label: String,
    /// The models listed, at most [`GROUP_ROW_LIMIT`] unless searching.
    shown: Vec<Model>,
    /// Models left out, for the "N more" row.
    hidden: usize,
}

impl EventEmitter<ModelPickerEvent> for ModelPicker {}

impl ModelPicker {
    /// The search field.
    #[cfg(test)]
    pub(crate) fn query_for_test(&self) -> &Entity<InputState> {
        &self.query
    }

    pub(crate) fn new(
        store: Entity<AgentsStore>,
        smooth_scroll: SmoothScroll,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx
            .new(|cx| InputState::new(window, cx).placeholder("Search models, providers, nodes…"));
        let subs =
            vec![
                cx.subscribe_in(&query, window, |this, _, ev: &InputEvent, _, cx| match ev {
                    InputEvent::PressEnter { .. } => this.pick_cursor(cx),
                    InputEvent::Change => {
                        // A new list: the keyboard starts at its top again.
                        this.cursor = None;
                        this.scroll.scroll_to_item(0);
                        cx.notify();
                    }
                    _ => {}
                }),
            ];
        Self {
            store,
            query,
            open: false,
            target: None,
            launch_reasoning: true,
            recent: None,
            selected: None,
            cursor: None,
            palette: None,
            scroll: ScrollHandle::new(),
            sheet_cap: Rc::new(Cell::new(None)),
            smooth_scroll,
            _subs: subs,
        }
    }

    /// Show the reasoning-effort control for launch targets too (default).
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn with_launch_reasoning(mut self, shown: bool) -> Self {
        self.launch_reasoning = shown;
        self
    }

    /// Paint with `palette` instead of the app's.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn set_palette(&mut self, palette: Option<Palette>) {
        self.palette = palette;
    }

    /// The colours the picker paints with: its own, else the app's.
    fn colours(&self, cx: &App) -> Palette {
        self.palette.unwrap_or_else(|| *palette(cx))
    }

    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// What the next pick applies to; `None` while there is nothing to pick for.
    pub(crate) fn set_target(&mut self, target: Option<Target>) {
        self.target = target;
    }

    /// Recent models and the chosen key to show instead of the target's own.
    pub(crate) fn set_overrides(
        &mut self,
        recent: Option<Vec<Model>>,
        selected: Option<Option<String>>,
    ) {
        self.recent = recent;
        self.selected = selected;
    }

    /// The agent whose models are listed.
    pub(crate) fn agent(&self, cx: &App) -> Option<String> {
        self.target.as_ref().map(|t| t.agent(cx))
    }

    /// Open the picker on `target` with an empty search.
    pub(crate) fn open(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        self.target = Some(target);
        self.open = true;
        self.cursor = None;
        self.clear_query(window, cx);
        self.query.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// Close the picker; true when it was open.
    pub(crate) fn close(&mut self, cx: &mut Context<Self>) -> bool {
        let was = std::mem::replace(&mut self.open, false);
        if was {
            cx.notify();
        }
        was
    }

    /// Empty the search field; the keyboard goes back to the chosen model.
    pub(crate) fn clear_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.query
            .update(cx, |s, cx| s.set_value(SharedString::default(), window, cx));
        self.cursor = None;
        self.scroll.scroll_to_item(0);
    }

    /// The scroll handle of the list, for hosts that render [`Self::rows`]
    /// in their own scroller: the keyboard highlight scrolls it into view.
    pub(crate) fn scroll_handle(&self) -> ScrollHandle {
        self.scroll.clone()
    }

    /// Put the keyboard focus in the search field.
    pub(crate) fn focus_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.query.update(cx, |s, cx| s.focus(window, cx));
    }

    /// Choose a model by selection key: a session target changes its model
    /// and the picker closes; either way the owner is told.
    pub(crate) fn pick(&mut self, key: String, cx: &mut Context<Self>) {
        if let Some(Target::Session(session)) = self.target.clone() {
            self.store.update(cx, |s, cx| {
                s.set_session_option(&session, "model", &key, cx)
            });
            self.open = false;
        }
        cx.emit(ModelPickerEvent::Picked { key });
        cx.notify();
    }

    /// The groups as the list shows them.
    fn groups(&self, cx: &App) -> Vec<Group> {
        let searching = !self.query.read(cx).value().trim().is_empty();
        self.sections(cx)
            .into_iter()
            .map(|(label, mut models)| {
                let shown = if searching || label == "Recent" {
                    models.len()
                } else {
                    models.len().min(GROUP_ROW_LIMIT)
                };
                let hidden = models.len() - shown;
                models.truncate(shown);
                Group {
                    label,
                    shown: models,
                    hidden,
                }
            })
            .collect()
    }

    /// The rows that can be picked, top to bottom: selection key and the
    /// index of the row among the list's children (headers and "N more" rows
    /// count there).
    fn pickable(&self, cx: &App) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        let mut child = 0;
        for group in self.groups(cx) {
            child += 1; // the group's header
            for m in &group.shown {
                out.push((m.key(), child));
                child += 1;
            }
            if group.hidden > 0 {
                child += 1;
            }
        }
        out
    }

    /// Where the keyboard is among `pickable` rows: where an arrow key put
    /// it, else on the chosen model, else on the first.
    fn cursor_at(&self, pickable: &[(String, usize)], cx: &App) -> usize {
        let Some(last) = pickable.len().checked_sub(1) else {
            return 0;
        };
        if let Some(cursor) = self.cursor {
            return cursor.min(last);
        }
        if !self.query.read(cx).value().trim().is_empty() {
            return 0;
        }
        self.agent(cx)
            .and_then(|agent| self.selected_key(&agent, cx))
            .and_then(|key| pickable.iter().position(|(k, _)| *k == key))
            .unwrap_or(0)
    }

    /// Up and Down in the search field; wraps around. False when there is
    /// nothing to move over.
    fn move_cursor(&mut self, down: bool, cx: &mut Context<Self>) -> bool {
        let pickable = self.pickable(cx);
        let len = pickable.len();
        if len == 0 {
            return false;
        }
        let at = self.cursor_at(&pickable, cx);
        let to = if down {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        self.cursor = Some(to);
        // On the first row show its group's header too.
        self.scroll
            .scroll_to_item(if to == 0 { 0 } else { pickable[to].1 });
        cx.notify();
        true
    }

    /// Enter in the search field: the row the keyboard is on.
    fn pick_cursor(&mut self, cx: &mut Context<Self>) {
        let pickable = self.pickable(cx);
        let at = self.cursor_at(&pickable, cx);
        if let Some((key, _)) = pickable.into_iter().nth(at) {
            self.pick(key, cx);
        }
    }

    /// Models for the picker: "Recent" first (when not searching), then
    /// grouped by provider (and node). While searching, models and groups
    /// follow the match order, best first.
    fn sections(&self, cx: &App) -> Vec<(String, Vec<Model>)> {
        let query = self.query.read(cx).value().trim().to_string();
        let Some(agent) = self.agent(cx) else {
            return vec![];
        };
        let store = self.store.read(cx);
        let mut sections: Vec<(String, Vec<Model>)> = Vec::new();
        let mut taken = std::collections::HashSet::new();
        if query.is_empty() {
            let recent: Vec<Model> = self
                .recent
                .clone()
                .unwrap_or_else(|| store.recent_models(&agent).into_iter().cloned().collect());
            if !recent.is_empty() {
                taken.extend(recent.iter().map(Model::key));
                sections.push(("Recent".into(), recent));
            }
        }
        let models: Vec<&Model> = store
            .models(&agent)
            .iter()
            .filter(|m| !taken.contains(&m.key()))
            .collect();
        // The model's own names lead; its id (which repeats the provider as
        // a prefix), provider and node only describe where it is served.
        let ranked = crate::search::rank_tiered(
            &query,
            &models,
            |m| vec![m.label.clone(), m.model.clone()],
            |m| {
                let mut fields = vec![m.id.clone(), m.provider.clone()];
                fields.extend(m.node.clone());
                fields
            },
        );
        for (ix, _) in ranked {
            let m = models[ix];
            let label = match &m.node {
                Some(node) => format!("{} · {node}", m.provider),
                None => m.provider.clone(),
            };
            match sections.iter_mut().find(|(l, _)| *l == label) {
                Some((_, list)) => list.push(m.clone()),
                None => sections.push((label, vec![m.clone()])),
            }
        }
        sections
    }

    /// The key shown as chosen.
    fn selected_key(&self, agent: &str, cx: &App) -> Option<String> {
        if let Some(selected) = &self.selected {
            return selected.clone();
        }
        let store = self.store.read(cx);
        match &self.target {
            Some(Target::Session(session)) => store.session_model_key(&session.read(cx).view),
            _ => store.launch_model(agent).map(Model::key),
        }
    }

    /// The picker's rows: "Recent", then models grouped by provider (and
    /// node), with context size, modalities and mesh node. Shared by the
    /// modal, setup and the delegate dialog.
    pub(crate) fn rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = self.colours(cx);
        let groups = self.groups(cx);
        let Some(agent) = self.agent(cx) else {
            return Vec::new();
        };
        let pickable = self.pickable(cx);
        let cursor = pickable
            .get(self.cursor_at(&pickable, cx))
            .map(|(key, _)| key.clone());
        let store = self.store.read(cx);
        let refreshing = store
            .runtime(&agent)
            .is_some_and(|rt| rt.pending.contains("models"));
        // Selection keys: the same model on two machines is two rows.
        let selected = self.selected_key(&agent, cx);
        let total = pickable.len();

        let mut rows: Vec<AnyElement> = Vec::new();
        let mut models_listed = 0;
        let mut sheet_cap = None;
        for Group {
            label,
            shown: models,
            hidden,
        } in groups
        {
            let recent = label == "Recent";
            let group_icon: AnyElement = if recent {
                kit::icon(IconName::Clock, 12., p.dim).into_any_element()
            } else {
                kit::provider_icon(&models[0].provider, 12., p.dim)
            };
            rows.push(
                h_flex()
                    .gap_1p5()
                    .px_2()
                    .pt_3()
                    .pb_1()
                    .text_xs()
                    .font_semibold()
                    .text_color(p.dim)
                    .child(group_icon)
                    .child(label.clone())
                    .child(
                        div()
                            .text_color(p.border_strong)
                            .child((models.len() + hidden).to_string()),
                    )
                    .into_any_element(),
            );
            for m in &models {
                rows.push(self.model_row(
                    m,
                    &label,
                    selected.as_deref(),
                    cursor.as_deref(),
                    &p,
                    cx,
                ));
                models_listed += 1;
                if models_listed == SHEET_MODEL_ROWS {
                    sheet_cap = Some(rows.len() - 1);
                }
            }
            if hidden > 0 {
                rows.push(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(p.dim)
                        .child(format!("{hidden} more — search to narrow down"))
                        .into_any_element(),
                );
            }
        }
        self.sheet_cap.set(sheet_cap);
        if total == 0 {
            rows.push(
                div()
                    .p_6()
                    .text_sm()
                    .text_color(p.dim)
                    .text_center()
                    .child(if refreshing {
                        "Loading models…"
                    } else {
                        "No models match your search."
                    })
                    .into_any_element(),
            );
        }
        rows
    }

    fn model_row(
        &self,
        m: &Model,
        label: &str,
        selected: Option<&str>,
        cursor: Option<&str>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = m.key();
        let is_selected = selected == Some(id.as_str());
        let is_cursor = cursor == Some(id.as_str());
        let hover = p.card_hover;
        // A model can be listed under Recent and under its provider.
        let row_key = format!("{label}:{id}");
        let known = !m.modalities.is_empty();
        let modalities: Vec<String> = if known {
            m.modalities.clone()
        } else {
            vec!["text".into()]
        };
        let context_tip = match m.context {
            Some(t) => format!("{t} token context window"),
            None => "Context size unknown".to_string(),
        };
        let detail_color = if is_selected { p.muted } else { p.dim };
        let p = *p;
        h_flex()
            .id(kit::keyed("model-row", row_key.clone()))
            .cursor_pointer()
            .gap_3()
            .px_2p5()
            .py_1p5()
            .rounded(px(8.))
            .cursor_pointer()
            .items_start()
            .hover(move |el| el.bg(hover))
            .when(is_selected, |el| el.bg(p.card))
            .when(is_cursor, |el| el.bg(hover))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .truncate()
                                    .when(is_selected, |el| el.text_color(p.accent))
                                    .child(m.label.clone()),
                            )
                            .when(is_selected, |el| {
                                el.child(kit::icon(IconName::Check, 12., p.accent))
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.dim)
                            .font_family(kit::MONO)
                            .truncate()
                            .child(m.id.clone()),
                    ),
            )
            .child(
                v_flex()
                    .flex_none()
                    .items_end()
                    .gap_1()
                    .text_xs()
                    .text_color(detail_color)
                    .child(
                        h_flex()
                            .gap_2p5()
                            .when_some(m.node.clone(), |el, node| {
                                el.child(
                                    h_flex()
                                        .gap_1()
                                        .child(kit::icon(IconName::Network, 11., detail_color))
                                        .child(node),
                                )
                            })
                            .child(
                                h_flex()
                                    .id(kit::keyed("model-ctx", row_key.clone()))
                                    .cursor_pointer()
                                    .gap_1()
                                    .tooltip(move |window, cx| {
                                        kit::wrap_tip(context_tip.clone()).build(window, cx)
                                    })
                                    .child(kit::icon(IconName::Gauge, 12., detail_color))
                                    .child(m.context_short()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .children(modalities.into_iter().map(|modality| {
                                let tip = if known {
                                    format!("Supports {modality} input")
                                } else {
                                    "Model modalities unknown".to_string()
                                };
                                div()
                                    .id(kit::keyed(
                                        "model-modality",
                                        format!("{row_key}:{modality}"),
                                    ))
                                    .cursor_pointer()
                                    .tooltip(move |window, cx| {
                                        kit::wrap_tip(tip.clone()).build(window, cx)
                                    })
                                    .child(kit::icon(
                                        kit::modality_icon(&modality),
                                        13.,
                                        detail_color,
                                    ))
                            })),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.pick(id.clone(), cx)))
            .into_any_element()
    }

    /// Search field with the refresh button, above the model rows.
    pub(crate) fn search(&self, agent: &str, p: &Palette, cx: &mut Context<Self>) -> Div {
        let refreshing = self
            .store
            .read(cx)
            .runtime(agent)
            .is_some_and(|rt| rt.pending.contains("models"));
        let agent = agent.to_owned();
        h_flex()
            .pl_2p5()
            .pr_1()
            .rounded(px(10.))
            .bg(p.card)
            .border_1()
            .border_color(p.border)
            // Before the field, which would take the arrows for its caret.
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                if this.move_cursor(false, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                if this.move_cursor(true, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(kit::icon(IconName::Search, 15., p.dim))
            // In the palette it is drawn in, not the app theme's: the quick
            // panel's is dark whatever the app theme.
            .when(self.palette.is_some(), |el| {
                el.child(kit::input_colours(&self.query, p))
            })
            .child(
                Input::new(&self.query)
                    .appearance(false)
                    .text_color(p.text)
                    .flex_1(),
            )
            .child(if refreshing {
                div()
                    .p_1()
                    .child(kit::spinner("models-refreshing", 14., p.muted))
                    .into_any_element()
            } else {
                let store = self.store.clone();
                kit::icon_button("model-refresh", IconName::RefreshCw, "Refresh models")
                    .on_click(move |_, _, cx| {
                        store.update(cx, |s, cx| s.refresh_models(&agent, cx))
                    })
                    .into_any_element()
            })
    }

    /// The reasoning-effort control of the target, if it has one.
    fn reasoning(&self, cx: &App) -> Option<ReasoningControl> {
        match self.target.as_ref()? {
            Target::Session(session) => {
                let v = &session.read(cx).view;
                let r = v.reasoning_option()?;
                Some(ReasoningControl {
                    config_id: Some(r.id.clone()),
                    current: r.current.clone(),
                    choices: r.choices.clone(),
                    pending: v.pending_options.contains(&r.id),
                })
            }
            Target::Launch { .. } if self.launch_reasoning => Some(ReasoningControl {
                config_id: None,
                current: self.store.read(cx).prefs.reasoning().to_string(),
                choices: LAUNCH_REASONING
                    .iter()
                    .map(|(v, l, _)| (v.to_string(), l.to_string()))
                    .collect(),
                pending: false,
            }),
            Target::Launch { .. } => None,
        }
    }

    /// The reasoning-effort footer: one pill per level, with the selected
    /// level's description underneath.
    fn reasoning_footer(
        &self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let in_session = matches!(self.target, Some(Target::Session(_)));
        let session = match &self.target {
            Some(Target::Session(session)) => Some(session.clone()),
            _ => None,
        };
        let ReasoningControl {
            config_id,
            current,
            choices,
            pending,
        } = self.reasoning(cx)?;
        let describe = LAUNCH_REASONING
            .iter()
            .find(|(v, _, _)| *v == current)
            .map(|(_, _, d)| d.to_string())
            .unwrap_or_else(|| {
                if in_session {
                    "Adjust reasoning for the next reply.".into()
                } else {
                    "Applied when the session starts.".into()
                }
            });
        let segments = choices.into_iter().enumerate().map(|(ix, (value, label))| {
            let config_id = config_id.clone();
            let session = session.clone();
            let selected = value == current;
            kit::segment(("model-reasoning", ix), selected, p)
                .flex_1()
                .label(label)
                .inactive(pending)
                .on_click(cx.listener(move |this, _, _, cx| {
                    match (&config_id, &session) {
                        (Some(config_id), Some(session)) => this.store.update(cx, |s, cx| {
                            s.set_session_option(session, config_id, &value, cx)
                        }),
                        _ => this
                            .store
                            .update(cx, |s, cx| s.set_launch_reasoning(&value, cx)),
                    }
                }))
        });
        Some(
            v_flex()
                .debug_selector(|| "model-reasoning".into())
                .mx_4()
                .pt_3()
                .gap_1p5()
                .border_t_1()
                .border_color(p.border)
                .child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .font_semibold()
                        .text_color(p.dim)
                        .child(kit::icon(IconName::Brain, 13., p.dim))
                        .child("Reasoning effort")
                        .when(pending, |el| {
                            el.child(kit::spinner("reasoning-pending", 12., p.dim))
                        }),
                )
                .child(kit::segment_bar(p).children(segments))
                .child(div().text_xs().text_color(p.muted).child(describe)),
        )
    }

    /// The picker card: search + refresh, "Recent", models grouped by
    /// provider, the reasoning-effort control and Done.
    pub(crate) fn card(
        &mut self,
        frame: PickerFrame,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        let p = self.colours(cx);
        let agent = self.agent(cx)?;
        let in_session = matches!(self.target, Some(Target::Session(_)));
        let rows = self.rows(cx);
        let search = self.search(&agent, &p, cx);
        let smooth_scroll = self.smooth_scroll.clone();
        // A sheet is the search field and the list and nothing else (design
        // D20): no title, reasoning footer, close button or Done. The model
        // and the effort change by key and in the conversation's header.
        let is_card = matches!(frame, PickerFrame::Card { .. });
        let footer = if is_card {
            self.reasoning_footer(&p, cx)
        } else {
            None
        };
        let (framed, probe) = match frame {
            PickerFrame::Card { width, max_h } => (
                v_flex()
                    .id("model-card")
                    .w(width)
                    .max_h(max_h)
                    .rounded(px(16.))
                    .bg(p.popover)
                    .border_1()
                    .border_color(p.border)
                    .when(p.overlay_shadow(), |el| el.shadow_lg()),
                None,
            ),
            // No frame of its own: it lies on the host's surface (design D17).
            PickerFrame::Sheet { probe } => (
                v_flex()
                    .id("model-card")
                    .relative()
                    .w_full()
                    .flex_1()
                    .min_h_0(),
                Some(probe),
            ),
        };
        Some(
            framed
                .on_click(|_, _, cx| cx.stop_propagation())
                .children(probe)
                .when(is_card, |el| {
                    el.child(
                        h_flex()
                            .px_4()
                            .pt_4()
                            .pb_3()
                            .gap_3()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .child(div().font_semibold().child(if in_session {
                                        "Switch model"
                                    } else {
                                        "Choose a model"
                                    }))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(p.dim)
                                            .child("Search by model, provider, or mesh node."),
                                    ),
                            )
                            .child(
                                kit::icon_button("model-close", IconName::X, "Close").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.close(cx);
                                    }),
                                ),
                            ),
                    )
                })
                // The sheet's search row takes the title row's top padding.
                .child(div().px_4().when(!is_card, |el| el.pt_4()).child(search))
                .child(
                    // The rows are the box's direct children: the keyboard
                    // cursor scrolls to them by index.
                    v_flex()
                        .id("model-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .px_2()
                        .pb(px(LIST_PAD_BOTTOM))
                        .children(rows)
                        .map(|el| smooth_scroll.region("model-list", el)),
                )
                .children(footer)
                .when(is_card, |el| {
                    el.child(
                        h_flex().px_4().py_3().justify_end().child(
                            kit::flat_btn("model-done", true, true, &p)
                                .debug_selector(|| "model-done".into())
                                .child("Done")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.close(cx);
                                })),
                        ),
                    )
                }),
        )
    }

    /// The rows' scrolling box, to see in tests whether the list scrolls.
    #[cfg(test)]
    pub(crate) fn rows_scroll_for_test(&self) -> ScrollHandle {
        self.scroll.clone()
    }

    /// How tall a sheet laid out `whole` tall needs to be, as a function of
    /// `whole`: it shows the first [`SHEET_MODEL_ROWS`] models without
    /// scrolling (or every row when there are fewer), the rest scroll.
    /// That is `whole`, less the rows' box, plus what those rows and their
    /// padding take. Read after the sheet's layout (the handle holds the
    /// bounds of the last one).
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn sheet_measure(&self) -> impl Fn(f32) -> f32 + 'static + use<> {
        let (rows, cap) = (self.scroll.clone(), self.sheet_cap.clone());
        move |whole| Self::sheet_height(&rows, cap.get(), whole)
    }

    fn sheet_height(rows: &ScrollHandle, cap: Option<usize>, whole: f32) -> f32 {
        let last = rows.children_count().checked_sub(1);
        let end = match (cap, last) {
            (Some(cap), Some(last)) => Some(cap.min(last)),
            (None, last) => last,
            (Some(_), None) => None,
        };
        let extent = match (rows.bounds_for_item(0), end.and_then(|e| rows.bounds_for_item(e))) {
            (Some(first), Some(end)) => f32::from(end.bottom() - first.top()),
            _ => 0.,
        };
        whole - f32::from(rows.bounds().size.height) + extent + LIST_PAD_BOTTOM
    }

    /// The picker over the whole window, as a modal with a scrim.
    pub(crate) fn render_modal(
        &mut self,
        radii: Corners<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.open {
            return None;
        }
        let p = self.colours(cx);
        let card = self.card(
            PickerFrame::Card {
                width: px(620.),
                max_h: px(640.),
            },
            cx,
        )?;
        Some(
            div()
                .id("scrim-model")
                .absolute()
                .inset_0()
                .rounded_tl(radii.top_left)
                .rounded_tr(radii.top_right)
                .rounded_bl(radii.bottom_left)
                .rounded_br(radii.bottom_right)
                .bg(black().opacity(if p.dark { 0.55 } else { 0.28 }))
                .occlude()
                .flex()
                .justify_center()
                .items_center()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.close(cx);
                }))
                .child(card)
                .into_any_element(),
        )
    }
}

impl Render for ModelPicker {
    /// The modal, for hosts that mount the picker as a view.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_modal(Corners::default(), cx)
            .unwrap_or_else(|| div().into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use crate::session::SessionView;
    use crate::store::AgentsStore;
    use crate::ui::{Page, QmtApp};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext, VisualTestContext};
    use serde_json::json;

    struct Rig {
        handle: AnyWindowHandle,
        app: Entity<QmtApp>,
        requests: async_channel::Receiver<String>,
    }

    /// A window on `page` with the fixture's models, the picker open.
    fn rig(
        cx: &mut TestAppContext,
        bulk: usize,
        page: Page,
        setup: impl FnOnce(&mut AgentsStore),
    ) -> Rig {
        rig_with(cx, bulk, page, setup, |_| {})
    }

    /// [`rig`], with `edit` applied to the Session page's view.
    fn rig_with(
        cx: &mut TestAppContext,
        bulk: usize,
        page: Page,
        setup: impl FnOnce(&mut AgentsStore),
        edit: impl FnOnce(&mut SessionView),
    ) -> Rig {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::launch::models_fixture(bulk);
        let (_rpc, requests) = store.rpc_for_test("test-agent");
        setup(&mut store);
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|cx| {
                let mut store = store;
                if page == Page::Session {
                    let mut view = SessionView::new("test-agent", "s", "/w", "t");
                    edit(&mut view);
                    store.install_main_for_test(view, cx);
                }
                store
            });
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.go(page, cx);
                app.aim_model_picker(cx);
                app.open_model_picker(window, cx);
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

    fn picker(cx: &mut TestAppContext, rig: &Rig) -> Entity<super::ModelPicker> {
        rig.app.read_with(cx, |app, _| app.model_picker.clone())
    }

    fn search(cx: &mut TestAppContext, rig: &Rig, text: &str) {
        let picker = picker(cx, rig);
        cx.update_window(rig.handle, |_, window, cx| {
            let query = picker.read(cx).query.clone();
            query.update(cx, |s, cx| s.set_value(text.to_string(), window, cx));
            window.render_frame(cx);
        })
        .unwrap();
    }

    fn keys(cx: &mut TestAppContext, rig: &Rig, keys: &str) {
        VisualTestContext::from_window(rig.handle, cx).simulate_keystrokes(keys);
    }

    /// Ids of the listed models, in order.
    fn listed(cx: &mut TestAppContext, rig: &Rig) -> Vec<String> {
        picker(cx, rig).read_with(cx, |picker, cx| {
            picker
                .groups(cx)
                .into_iter()
                .flat_map(|g| g.shown.into_iter().map(|m| m.id))
                .collect()
        })
    }

    fn headings(cx: &mut TestAppContext, rig: &Rig) -> Vec<String> {
        picker(cx, rig).read_with(cx, |picker, cx| {
            picker.groups(cx).into_iter().map(|g| g.label).collect()
        })
    }

    /// Position of the keyboard among the pickable rows.
    fn cursor(cx: &mut TestAppContext, rig: &Rig) -> usize {
        picker(cx, rig).read_with(cx, |picker, cx| {
            let pickable = picker.pickable(cx);
            picker.cursor_at(&pickable, cx)
        })
    }

    /// Selection key (the id: the fixture has no nodes) of the highlighted model.
    fn highlighted(cx: &mut TestAppContext, rig: &Rig) -> String {
        picker(cx, rig).read_with(cx, |picker, cx| {
            let pickable = picker.pickable(cx);
            let at = picker.cursor_at(&pickable, cx);
            pickable[at].0.clone()
        })
    }

    fn is_open(cx: &mut TestAppContext, rig: &Rig) -> bool {
        picker(cx, rig).read_with(cx, |picker, _| picker.is_open())
    }

    #[gpui_kit::test]
    fn model_search_cases(cx: &mut TestAppContext) {
        let rig = rig(cx, 0, Page::Start, |_| {});
        let first = |cx: &mut TestAppContext, q: &str| {
            search(cx, &rig, q);
            listed(cx, &rig)
        };
        assert_eq!(
            first(cx, "anthropic sonnet"),
            ["anthropic/claude-sonnet-4-5"]
        );
        assert_eq!(first(cx, "sonnet 4.5")[0], "anthropic/claude-sonnet-4-5");
        let gpt = first(cx, "gpt5");
        assert_eq!(gpt[0], "openai/gpt-5");
        assert!(!gpt.contains(&"google/gemini-2.5-pro".to_string()));
        assert_eq!(first(cx, "cl son")[0], "anthropic/claude-sonnet-4-5");
        assert_eq!(first(cx, "sonet")[0], "anthropic/claude-sonnet-4-5");
        assert!(first(cx, "zzzzzz").is_empty());
        // Groups follow their best match, and Recent is hidden while searching.
        rig.app.update(cx, |app, cx| {
            app.store.update(cx, |s, cx| {
                s.set_launch_model("test-agent", "openai/o3".into(), cx)
            })
        });
        search(cx, &rig, "");
        assert_eq!(headings(cx, &rig)[0], "Recent");
        search(cx, &rig, "o3");
        assert!(!headings(cx, &rig).contains(&"Recent".to_string()));
    }

    #[gpui_kit::test]
    fn picker_opens_on_the_current_model(cx: &mut TestAppContext) {
        let rig = rig(cx, 0, Page::Start, |s| {
            s.prefs
                .launch_models
                .insert("test-agent".into(), "openai/gpt-5-mini".into());
        });
        // Recent is empty (nothing was picked), so the model sits in its provider group.
        assert_eq!(highlighted(cx, &rig), "openai/gpt-5-mini");
    }

    /// Regression: `op` listed an openrouter model first, on the provider's
    /// name alone (`openrouter` begins with it), above `claude-opus-*`.
    #[gpui_kit::test]
    fn a_name_match_beats_a_provider_match(cx: &mut TestAppContext) {
        let rig = rig(cx, 0, Page::Start, |store| {
            crate::store::launch::add_models_for_test(
                store,
                json!([
                    { "id": "openrouter/thinkingmachines/inkling-2-mini",
                      "label": "Inkling 2 Mini", "provider": "openrouter" },
                    { "id": "anthropic/claude-opus-4",
                      "label": "Claude Opus 4", "provider": "anthropic" },
                ]),
            )
        });
        search(cx, &rig, "op");
        let found = listed(cx, &rig);
        assert_eq!(found[0], "anthropic/claude-opus-4", "{found:?}");
        // Still listed, through its provider, below the name match.
        assert!(
            found.contains(&"openrouter/thinkingmachines/inkling-2-mini".to_string()),
            "{found:?}"
        );
        // The provider alone still finds all of its models.
        search(cx, &rig, "openrouter");
        assert_eq!(
            listed(cx, &rig),
            ["openrouter/thinkingmachines/inkling-2-mini"]
        );
    }

    #[gpui_kit::test]
    fn within_a_provider_the_better_name_leads(cx: &mut TestAppContext) {
        let rig = rig(cx, 0, Page::Start, |store| {
            crate::store::launch::add_models_for_test(
                store,
                json!([
                    { "id": "openrouter/thinkingmachines/inkling-2-mini",
                      "label": "Inkling 2 Mini", "provider": "openrouter" },
                    { "id": "openrouter/anthropic/claude-opus-4",
                      "label": "Claude Opus 4", "provider": "openrouter" },
                ]),
            )
        });
        search(cx, &rig, "op");
        let found = listed(cx, &rig);
        // Both openrouter models match through the provider, but only the
        // opus has `op` in its own name: it leads its group, and the group,
        // with the best match of the list, leads the picker.
        assert_eq!(
            found[..2],
            [
                "openrouter/anthropic/claude-opus-4",
                "openrouter/thinkingmachines/inkling-2-mini"
            ],
            "{found:?}"
        );
    }

    #[gpui_kit::test]
    fn search_highlights_the_first_result(cx: &mut TestAppContext) {
        let rig = rig(cx, 0, Page::Start, |s| {
            s.prefs
                .launch_models
                .insert("test-agent".into(), "openai/gpt-5-mini".into());
        });
        keys(cx, &rig, "down");
        keys(cx, &rig, "g p t");
        let first = listed(cx, &rig)[0].clone();
        assert_eq!(highlighted(cx, &rig), first);
        assert_eq!(cursor(cx, &rig), 0);
    }

    #[gpui_kit::test]
    fn down_skips_headings_and_wraps(cx: &mut TestAppContext) {
        let rig = rig(cx, 0, Page::Start, |s| {
            s.prefs
                .launch_models
                .insert("test-agent".into(), "anthropic/claude-sonnet-4-5".into());
        });
        let all = listed(cx, &rig);
        assert_eq!(highlighted(cx, &rig), all[0]);
        // Last of the Anthropic group, then the first of the next group.
        keys(cx, &rig, "down");
        assert_eq!(highlighted(cx, &rig), all[1]);
        keys(cx, &rig, "down");
        assert_eq!(highlighted(cx, &rig), all[2]);
        // Back up and around.
        keys(cx, &rig, "up up up");
        assert_eq!(highlighted(cx, &rig), *all.last().unwrap());
        keys(cx, &rig, "down");
        assert_eq!(highlighted(cx, &rig), all[0]);
        // The arrows did not reach the text field.
        let picker = picker(cx, &rig);
        picker.read_with(cx, |picker, cx| {
            assert!(picker.query.read(cx).value().is_empty())
        });
    }

    #[gpui_kit::test]
    fn hidden_rows_are_not_reachable(cx: &mut TestAppContext) {
        let rig = rig(cx, 45, Page::Start, |_| {});
        let listed_all = listed(cx, &rig);
        let bulk = listed_all
            .iter()
            .filter(|id| id.starts_with("bulk/"))
            .count();
        assert_eq!(bulk, super::GROUP_ROW_LIMIT);
        let hidden: Vec<usize> = picker(cx, &rig).read_with(cx, |picker, cx| {
            picker.groups(cx).iter().map(|g| g.hidden).collect()
        });
        assert_eq!(hidden.iter().filter(|h| **h == 5).count(), 1);
        // Walk the whole list: only listed models are visited, then it wraps.
        let total = listed_all.len();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..total {
            seen.insert(highlighted(cx, &rig));
            keys(cx, &rig, "down");
        }
        assert_eq!(seen.len(), total);
        assert!(!seen.contains("bulk/m44"));
    }

    #[gpui_kit::test]
    fn highlight_scrolls_into_view(cx: &mut TestAppContext) {
        let rig = rig(cx, 30, Page::Start, |_| {});
        for _ in 0..20 {
            keys(cx, &rig, "down");
        }
        cx.update_window(rig.handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        picker(cx, &rig).read_with(cx, |picker, cx| {
            let pickable = picker.pickable(cx);
            let ix = pickable[picker.cursor_at(&pickable, cx)].1;
            let list = picker.scroll.bounds();
            let row = picker.scroll.bounds_for_item(ix).expect("row laid out");
            let offset = picker.scroll.offset().y;
            let (top, bottom) = (row.top() + offset, row.bottom() + offset);
            assert!(
                top >= list.top() && bottom <= list.bottom(),
                "row {row:?} at offset {offset:?} not in {list:?}"
            );
        });
    }

    #[gpui_kit::test]
    fn enter_picks_the_highlighted_model(cx: &mut TestAppContext) {
        let rig = rig(cx, 0, Page::Session, |_| {});
        search(cx, &rig, "gpt");
        keys(cx, &rig, "down enter");
        cx.run_until_parked();
        let request: serde_json::Value =
            serde_json::from_str(&rig.requests.try_recv().unwrap()).unwrap();
        assert_eq!(request["method"], "session/set_config_option");
        assert_eq!(request["params"]["configId"], "model");
        assert_eq!(request["params"]["value"], "openai/gpt-5-mini");
        assert!(!is_open(cx, &rig));
    }

    #[gpui_kit::test]
    fn enter_with_no_results_does_nothing(cx: &mut TestAppContext) {
        let rig = rig(cx, 0, Page::Session, |_| {});
        search(cx, &rig, "zzzzzz");
        keys(cx, &rig, "enter");
        cx.run_until_parked();
        assert!(rig.requests.try_recv().is_err());
        assert!(is_open(cx, &rig));
    }

    #[gpui_kit::test]
    fn enter_on_start_sets_the_launch_model(cx: &mut TestAppContext) {
        let rig = rig(cx, 0, Page::Start, |_| {});
        search(cx, &rig, "sonnet");
        keys(cx, &rig, "enter");
        rig.app.read_with(cx, |app, cx| {
            let model = app.store.read(cx).launch_model("test-agent").unwrap();
            assert_eq!(model.id, "anthropic/claude-sonnet-4-5");
        });
        assert!(!is_open(cx, &rig));
    }

    /// Whether the page's prompt (Start's or the session's) has the focus.
    fn prompt_focused(cx: &mut TestAppContext, rig: &Rig) -> bool {
        cx.update_window(rig.handle, |_, window, cx| {
            let app = rig.app.read(cx);
            let prompt = if app.page == Page::Session {
                app.pane.read(cx).prompt.clone()
            } else {
                app.prompt.clone()
            };
            gpui_kit::Focusable::focus_handle(&prompt, cx).is_focused(window)
        })
        .unwrap()
    }

    #[gpui_kit::test]
    fn picking_a_model_returns_focus_to_the_prompt(cx: &mut TestAppContext) {
        for page in [Page::Start, Page::Session] {
            let rig = rig(cx, 0, page, |_| {});
            assert!(!prompt_focused(cx, &rig), "the picker's search has focus");
            search(cx, &rig, "sonnet");
            keys(cx, &rig, "enter");
            assert!(prompt_focused(cx, &rig), "{page:?}: prompt after picking");
        }
    }

    #[gpui_kit::test]
    fn escape_closes_the_picker_and_focuses_the_prompt(cx: &mut TestAppContext) {
        for page in [Page::Start, Page::Session] {
            let rig = rig(cx, 0, page, |_| {});
            keys(cx, &rig, "escape");
            assert!(!is_open(cx, &rig));
            assert!(prompt_focused(cx, &rig), "{page:?}: prompt after Esc");
        }
    }

    #[gpui_kit::test]
    fn escape_closes_the_options_and_focuses_the_prompt(cx: &mut TestAppContext) {
        for page in [Page::Start, Page::Session] {
            let rig = rig(cx, 0, page, |_| {});
            // Closed first: its closing hands the focus to the prompt.
            rig.app.update(cx, |app, cx| {
                app.model_picker.update(cx, |picker, cx| {
                    picker.close(cx);
                })
            });
            cx.run_until_parked();
            cx.update_window(rig.handle, |_, window, cx| {
                rig.app.update(cx, |app, cx| {
                    app.options_open = page == Page::Start;
                    app.pane
                        .update(cx, |pane, _| pane.options_open = page == Page::Session);
                    // A click on the Options button leaves the shell focused.
                    app.app_focus.focus(window, cx);
                });
                window.render_frame(cx);
            })
            .unwrap();
            assert!(!prompt_focused(cx, &rig));
            keys(cx, &rig, "escape");
            rig.app.read_with(cx, |app, cx| {
                assert!(!app.options_open && !app.pane.read(cx).options_open)
            });
            assert!(prompt_focused(cx, &rig), "{page:?}: prompt after Esc");
        }
    }

    #[gpui_kit::test]
    fn a_read_only_session_keeps_focus_on_the_shell(cx: &mut TestAppContext) {
        let rig = rig_with(
            cx,
            0,
            Page::Session,
            |_| {},
            |view| {
                view.access = Some(crate::session::SessionAccess {
                    reason: None,
                    owner_session_id: None,
                })
            },
        );
        keys(cx, &rig, "escape");
        assert!(!prompt_focused(cx, &rig));
        cx.update_window(rig.handle, |_, window, cx| {
            assert!(rig.app.read(cx).app_focus.is_focused(window));
        })
        .unwrap();
    }
}
