//! Inbox: permission and input (elicitation) requests. Cards render here and
//! inline in the session view, following the desktop `InboxRequestCard`:
//! radio / checkbox option rows with descriptions, "Custom answer…", resolved
//! cards that keep the submitted answer, offline and rejected-answer states.

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::{Value, json};

use std::collections::HashMap;

use super::mesh::panel;
use super::{Page, QmtApp, kit};
use crate::session::open::{SessionKey, SessionRef};
use crate::store::{
    AgentsStore, FieldDraft, FieldKind, FieldOption, FormField, PendingRequest, RequestKind,
    RequestStatus,
};
use crate::theme::{Palette, palette};
use crate::util::{now_ms, workspace_name};

fn ago(ms: f64) -> String {
    let secs = ((now_ms() - ms) / 1000.).max(0.) as u64;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
}

/// Desktop surface tokens the palette has no slot for.
#[derive(Clone, Copy)]
struct Tones {
    pill: Hsla,
    pill_hover: Hsla,
    row_hover: Hsla,
    selected: Hsla,
    selected_border: Hsla,
    subtle: Hsla,
    danger_bg: Hsla,
    danger_border: Hsla,
}

fn tones(p: &Palette) -> Tones {
    let ink = if p.dark { white() } else { p.text };
    let neutral = if p.dark { white() } else { p.muted };
    Tones {
        pill: neutral.opacity(if p.dark { 0.045 } else { 0.07 }),
        pill_hover: neutral.opacity(if p.dark { 0.09 } else { 0.13 }),
        row_hover: ink.opacity(0.055),
        selected: p.accent.opacity(if p.dark { 0.10 } else { 0.13 }),
        selected_border: p.accent.opacity(if p.dark { 0.28 } else { 0.32 }),
        subtle: ink.opacity(if p.dark { 0.075 } else { 0.09 }),
        danger_bg: p.danger.opacity(if p.dark { 0.08 } else { 0.10 }),
        danger_border: p.danger.opacity(0.24),
    }
}

/// Desktop `.badge` (`flat`: the Inbox page's faint-fill variant).
fn badge(text: impl Into<SharedString>, flat: bool, p: &Palette, t: &Tones) -> Div {
    h_flex()
        .flex_none()
        .px(px(10.4))
        .py(px(3.2))
        .rounded_full()
        .border_1()
        .text_xs()
        .map(|el| {
            if flat {
                el.border_color(transparent_black())
                    .bg(t.pill_hover)
                    .text_color(p.dim)
            } else {
                el.border_color(p.border)
                    .bg(p.status_muted.opacity(if p.dark { 0.045 } else { 0.08 }))
                    .text_color(p.muted)
            }
        })
        .child(text.into())
}

/// Desktop `.action-btn` (`primary`: accent fill; `flat`: Inbox page fill).
fn card_btn(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    primary: bool,
    flat: bool,
    enabled: bool,
    p: &Palette,
    t: &Tones,
) -> Stateful<Div> {
    let (bg, border, fg, hover_bg, hover_border) = if primary {
        (p.accent, p.accent, white(), p.accent_hover, p.accent_hover)
    } else if flat {
        (
            t.pill,
            transparent_black(),
            p.text,
            t.pill_hover,
            transparent_black(),
        )
    } else {
        (p.card, p.border, p.text, p.card_hover, p.border_strong)
    };
    h_flex()
        .id(id)
        .flex_none()
        .justify_center()
        .h(px(36.))
        .px(px(15.2))
        .rounded(px(8.))
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .text_size(px(13.1))
        .font_weight(FontWeight(550.))
        .child(label.into())
        .map(|el| {
            if enabled {
                el.cursor_pointer()
                    .hover(move |s| s.bg(hover_bg).border_color(hover_border))
            } else {
                el.opacity(0.55)
            }
        })
}

/// One choice row: radio (`multi == false`) or checkbox, label and description
/// (desktop `.elicitation-option-row`).
#[allow(clippy::too_many_arguments)]
fn option_row(
    id: ElementId,
    multi: bool,
    checked: bool,
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    custom: bool,
    locked: bool,
    flat: bool,
    p: &Palette,
    t: &Tones,
) -> Stateful<Div> {
    let indicator = div()
        .flex_none()
        .mt(px(2.))
        .size(px(16.))
        .flex()
        .items_center()
        .justify_center()
        .border_1()
        .map(|el| {
            if multi {
                el.rounded(px(5.6))
                    .border_color(if checked { p.accent } else { p.border_strong })
                    .bg(if checked { p.accent } else { p.card })
                    .when(checked, |el| {
                        el.child(kit::icon(IconName::Check, 12., white()))
                    })
            } else {
                el.rounded_full()
                    .border_color(if checked { p.accent } else { p.border_strong })
                    .bg(p.card)
                    .when(checked, |el| {
                        el.child(div().size(px(8.)).rounded_full().bg(p.accent))
                    })
            }
        });
    let (checked_bg, checked_border) = if flat {
        (t.pill_hover, transparent_black())
    } else {
        (t.selected, t.selected_border)
    };
    let hover_bg = if flat { t.pill_hover } else { t.row_hover };
    h_flex()
        .id(id)
        .w_full()
        .min_w_0()
        .items_start()
        .gap(px(9.6))
        .px(px(9.6))
        .py(px(6.))
        .rounded(px(8.))
        .border_1()
        .border_color(transparent_black())
        .when(checked, |el| el.bg(checked_bg).border_color(checked_border))
        .map(|el| {
            if locked {
                el.opacity(0.55)
            } else if checked {
                el.cursor_pointer()
            } else {
                el.cursor_pointer().hover(move |s| s.bg(hover_bg))
            }
        })
        .child(indicator)
        .child(
            v_flex()
                .min_w_0()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(if custom {
                            FontWeight(450.)
                        } else {
                            FontWeight::MEDIUM
                        })
                        .text_color(p.text)
                        .child(label.into()),
                )
                .when_some(description, |el, d| {
                    el.child(div().text_xs().text_color(p.dim).child(d))
                }),
        )
}

/// The `— OR —` divider above "Custom answer…".
fn or_divider(t: &Tones, p: &Palette) -> Div {
    h_flex()
        .w_full()
        .gap(px(10.4))
        .px(px(9.6))
        .child(div().flex_1().h(px(1.)).bg(t.subtle))
        .child(
            div()
                .text_size(px(11.2))
                .font_medium()
                .text_color(p.dim)
                .child("OR"),
        )
        .child(div().flex_1().h(px(1.)).bg(t.subtle))
}

/// The text an input shows for a draft value.
fn value_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| i.to_string())
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// Which requests a surface lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RequestFilter {
    /// Every request, newest first (the Inbox page).
    All,
    /// The pending requests of one session (a session pane).
    Session(SessionKey),
}

/// What the cards ask of the surface that shows them.
#[derive(Clone, Debug)]
pub(super) enum RequestCardsEvent {
    /// "Open session" on an Inbox card.
    OpenSession(SessionRef),
}

/// The state of the request cards of one surface: the form inputs the user
/// types into and which cards are folded. The Inbox page and every session
/// pane own one, so they never share a half-typed answer's input.
pub(super) struct RequestCards {
    store: Entity<AgentsStore>,
    form_inputs: HashMap<(String, usize, bool), (Entity<InputState>, Subscription)>,
    /// Cards the user expanded / collapsed (others use the default).
    collapsed: HashMap<String, bool>,
    _store: Subscription,
}

impl EventEmitter<RequestCardsEvent> for RequestCards {}

impl RequestCards {
    pub(super) fn new(store: Entity<AgentsStore>, cx: &mut Context<Self>) -> Self {
        let observed = cx.observe(&store, |this, store, cx| {
            // Drop inputs of requests that left the inbox.
            let s = store.read(cx);
            this.form_inputs
                .retain(|(k, _, _), _| s.requests.iter().any(|r| &r.key == k));
            this.collapsed
                .retain(|k, _| s.requests.iter().any(|r| &r.key == k));
            cx.notify();
        });
        Self {
            store,
            form_inputs: HashMap::new(),
            collapsed: HashMap::new(),
            _store: observed,
        }
    }

    /// Keys of the requests `filter` lists.
    pub(super) fn keys(&self, filter: &RequestFilter, cx: &App) -> Vec<String> {
        let store = self.store.read(cx);
        match filter {
            RequestFilter::All => {
                let mut keys: Vec<(f64, String)> = store
                    .requests
                    .iter()
                    .map(|r| (r.received_ms, r.key.clone()))
                    .collect();
                keys.sort_by(|a, b| b.0.total_cmp(&a.0));
                keys.into_iter().map(|(_, key)| key).collect()
            }
            RequestFilter::Session(session) => store
                .requests
                .iter()
                .filter(|r| {
                    r.is_pending()
                        && r.agent_id == session.agent.as_str()
                        && r.session_id.as_deref() == Some(session.session.as_str())
                })
                .map(|r| r.key.clone())
                .collect(),
        }
    }

    /// Text input of one field (`custom`: its "Custom answer…" input). Typing
    /// updates the request's draft in the store, so nothing is lost when the
    /// card re-renders or the question is re-delivered.
    fn draft_input(
        &mut self,
        req: &PendingRequest,
        field: usize,
        custom: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let slot = (req.key.clone(), field, custom);
        if let Some((e, _)) = self.form_inputs.get(&slot) {
            return e.clone();
        }
        let kind = req.fields().get(field).map(|f| f.kind);
        let draft = req.drafts.get(field).cloned().unwrap_or_default();
        let initial = if custom {
            draft.custom.unwrap_or_default()
        } else {
            value_text(&draft.value)
        };
        let e = cx.new(|cx| {
            let state = InputState::new(window, cx).default_value(initial);
            if custom {
                state.placeholder("Custom answer…")
            } else {
                state
            }
        });
        let key = req.key.clone();
        let sub = cx.subscribe_in(&e, window, move |this, state, ev: &InputEvent, _, cx| {
            match ev {
                InputEvent::Change => {
                    let text = state.read(cx).value().to_string();
                    let array = kind == Some(FieldKind::Array);
                    let draft = if custom {
                        FieldDraft {
                            value: if array { json!([]) } else { Value::from("") },
                            custom: Some(text),
                        }
                    } else if array {
                        FieldDraft {
                            value: Value::Array(
                                text.split(',')
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                    .map(Value::from)
                                    .collect(),
                            ),
                            custom: None,
                        }
                    } else {
                        FieldDraft {
                            value: Value::from(text),
                            custom: None,
                        }
                    };
                    this.store
                        .update(cx, |s, cx| s.set_draft(&key, field, draft, cx));
                }
                // Enter in a custom answer submits, as in the desktop app.
                InputEvent::PressEnter { shift: false, .. } if custom => {
                    this.store
                        .update(cx, |s, cx| s.answer_elicitation(&key, "accept", cx));
                }
                _ => {}
            }
        });
        self.form_inputs.insert(slot, (e.clone(), sub));
        e
    }

    /// Run a card button: a permission option, or accept / decline / cancel.
    pub(super) fn request_action(&mut self, key: &str, action: &str, cx: &mut Context<Self>) {
        let Some(req) = self.store.read(cx).requests.iter().find(|r| r.key == key) else {
            return;
        };
        match &req.kind {
            RequestKind::Permission { .. } => {
                self.store
                    .update(cx, |s, cx| s.answer_permission(key, action, cx));
            }
            RequestKind::Elicitation { url, .. } => {
                if action == "accept"
                    && let Some(url) = url.clone()
                {
                    cx.open_url(&url);
                }
                self.store
                    .update(cx, |s, cx| s.answer_elicitation(key, action, cx));
            }
        }
    }

    /// The control and description of one form field.
    #[allow(clippy::too_many_arguments)]
    fn field_block(
        &mut self,
        req: &PendingRequest,
        fix: usize,
        field: &FormField,
        locked: bool,
        flat: bool,
        p: &Palette,
        t: &Tones,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let draft = req.drafts.get(fix).cloned().unwrap_or_default();
        let custom_active = draft.custom.is_some();
        let key = req.key.clone();
        let submitted = req.submitted();
        let set = move |this: &mut Self, draft: FieldDraft, cx: &mut Context<Self>| {
            this.store
                .update(cx, |s, cx| s.set_draft(&key, fix, draft, cx));
        };

        let label = {
            let text = format!("{}{}", field.label, if field.required { " *" } else { "" });
            if flat {
                div()
                    .text_size(px(12.8))
                    .font_semibold()
                    .text_color(p.text)
                    .child(text)
            } else {
                div()
                    .text_xs()
                    .text_color(p.muted)
                    .child(text.to_uppercase())
            }
        };

        let control: AnyElement = match field.kind {
            FieldKind::Boolean => {
                let on = draft.value.as_bool().unwrap_or(false);
                let set = set.clone();
                option_row(
                    kit::keyed("req-bool", format!("{}/{fix}", req.key)),
                    true,
                    on,
                    if on { "Yes" } else { "No" },
                    None,
                    false,
                    locked,
                    flat,
                    p,
                    t,
                )
                .when(!locked, |el| {
                    el.on_click(cx.listener(move |this, _, _, cx| {
                        set(
                            this,
                            FieldDraft {
                                value: Value::Bool(!on),
                                custom: None,
                            },
                            cx,
                        )
                    }))
                })
                .into_any_element()
            }
            _ if !field.options.is_empty() => {
                let multi = field.kind == FieldKind::Array;
                let selected: Vec<String> = if custom_active {
                    Vec::new()
                } else if multi {
                    draft
                        .value
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                } else {
                    draft
                        .value
                        .as_str()
                        .map(str::to_owned)
                        .into_iter()
                        .collect()
                };
                // A submitted card shows only what was chosen.
                let visible: Vec<&FieldOption> = field
                    .options
                    .iter()
                    .filter(|o| !submitted || selected.contains(&o.value))
                    .collect();
                let mut rows: Vec<AnyElement> = Vec::new();
                for o in visible {
                    let checked = selected.contains(&o.value);
                    let (set, value, current) = (set.clone(), o.value.clone(), selected.clone());
                    rows.push(
                        option_row(
                            kit::keyed("req-opt", format!("{}/{fix}/{}", req.key, o.value)),
                            multi,
                            checked,
                            o.label.clone(),
                            o.description.clone().map(Into::into),
                            false,
                            locked,
                            flat,
                            p,
                            t,
                        )
                        .when(!locked, |el| {
                            el.on_click(cx.listener(move |this, _, _, cx| {
                                let value = if multi {
                                    let mut next = current.clone();
                                    if checked {
                                        next.retain(|v| *v != value);
                                    } else {
                                        next.push(value.clone());
                                    }
                                    json!(next)
                                } else {
                                    Value::from(value.clone())
                                };
                                set(
                                    this,
                                    FieldDraft {
                                        value,
                                        custom: None,
                                    },
                                    cx,
                                );
                            }))
                        })
                        .into_any_element(),
                    );
                }
                if field.allow_custom && (!locked || custom_active) {
                    let (set, req2) = (set.clone(), req.clone());
                    let empty = if multi { json!([]) } else { Value::from("") };
                    rows.push(
                        v_flex()
                            .gap(px(4.5))
                            .when(!locked, |el| el.child(or_divider(t, p)))
                            .child(
                                option_row(
                                    kit::keyed("req-custom", format!("{}/{fix}", req.key)),
                                    multi,
                                    custom_active,
                                    "Custom answer…",
                                    Some("Type a different response.".into()),
                                    true,
                                    locked,
                                    flat,
                                    p,
                                    t,
                                )
                                .when(!locked, |el| {
                                    el.on_click(cx.listener(move |this, _, window, cx| {
                                        if custom_active && multi {
                                            set(
                                                this,
                                                FieldDraft {
                                                    value: json!([]),
                                                    custom: None,
                                                },
                                                cx,
                                            );
                                            return;
                                        }
                                        if custom_active {
                                            return;
                                        }
                                        let input = this.draft_input(&req2, fix, true, window, cx);
                                        let text = input.read(cx).value().to_string();
                                        set(
                                            this,
                                            FieldDraft {
                                                value: empty.clone(),
                                                custom: Some(text),
                                            },
                                            cx,
                                        );
                                        input.update(cx, |s, cx| s.focus(window, cx));
                                    }))
                                }),
                            )
                            .into_any_element(),
                    );
                }
                let list = v_flex().gap(px(2.)).min_w_0().children(rows);
                if custom_active {
                    let input = self.draft_input(req, fix, true, window, cx);
                    v_flex()
                        .gap_2()
                        .child(list)
                        .child(Input::new(&input).disabled(locked))
                        .into_any_element()
                } else {
                    list.into_any_element()
                }
            }
            _ => {
                let input = self.draft_input(req, fix, false, window, cx);
                Input::new(&input).disabled(locked).into_any_element()
            }
        };

        let description = field
            .description
            .clone()
            .filter(|d| d.trim() != req.detail.trim());
        v_flex()
            .gap_2()
            .min_w_0()
            .child(label)
            .child(control)
            .when_some(description, |el, d| {
                el.child(div().text_xs().text_color(p.muted).child(d))
            })
            .into_any_element()
    }

    /// Card for one request. `page`: the Inbox page (collapsible, session line,
    /// Open session); otherwise the compact card in the session view.
    pub(super) fn request_card(
        &mut self,
        key: &str,
        page: bool,
        collapsed_default: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let p = *palette(cx);
        let t = tones(&p);
        let store = self.store.read(cx);
        let req = store.requests.iter().find(|r| r.key == key)?.clone();
        let show_agent = {
            let mut agents: Vec<&str> =
                store.requests.iter().map(|r| r.agent_id.as_str()).collect();
            agents.sort_unstable();
            agents.dedup();
            agents.len() > 1
        };
        let agent_name = store
            .config(&req.agent_id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| req.agent_id.clone());
        let summary = req
            .session_id
            .as_deref()
            .and_then(|s| store.session_summary(&req.agent_id, s))
            .cloned();

        let collapsed = page
            && self
                .collapsed
                .get(key)
                .copied()
                .unwrap_or(collapsed_default);
        let pending = req.is_pending();
        let offline = req.offline && pending;
        let locked = !req.answerable();
        let flat = page;

        // ---- header
        let chevron = page.then(|| {
            let k = key.to_string();
            div()
                .id(kit::keyed("req-chevron", key.to_string()))
                .flex_none()
                .size(px(19.2))
                .mt(px(if collapsed { 0. } else { 2. }))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .cursor_pointer()
                .hover(move |s| s.bg(t.row_hover))
                .child(kit::icon(
                    if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    },
                    14.,
                    p.muted,
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let c = this.collapsed.entry(k.clone()).or_insert(collapsed_default);
                    *c = !*c;
                    cx.notify();
                }))
        });
        let copy = if collapsed {
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(p.muted)
                .line_clamp(2)
                .child(req.detail.clone())
                .into_any_element()
        } else {
            v_flex()
                .flex_1()
                .min_w_0()
                .child(div().text_sm().font_semibold().child(req.title.clone()))
                .child(
                    div()
                        .mt_1()
                        .text_sm()
                        .text_color(p.muted)
                        .child(req.detail.clone()),
                )
                .into_any_element()
        };
        let marker = if req.submitted() {
            Some((IconName::CircleCheckBig, p.success, "Submitted"))
        } else if pending {
            Some((IconName::CircleDot, p.running, "Pending"))
        } else {
            None
        };
        let resolution = match &req.status {
            RequestStatus::Resolved(r) if marker.is_none() => Some(r.clone()),
            _ => None,
        };
        let open_session = page.then(|| req.session_id.clone()).flatten().map(|sid| {
            let (agent, summary) = (req.agent_id.clone(), summary.clone());
            let tip = sid.clone();
            h_flex()
                .id(kit::keyed("req-open", key.to_string()))
                .flex_none()
                .gap(px(6.4))
                .h(px(32.))
                .px(px(12.))
                .rounded(px(8.))
                .bg(t.pill)
                .text_size(px(12.))
                .font_weight(FontWeight(550.))
                .text_color(p.text)
                .cursor_pointer()
                .hover(move |s| s.bg(t.pill_hover))
                .child("Open session")
                .child(kit::icon(IconName::ArrowUpRight, 12., p.text))
                .tooltip(move |window, cx| kit::wrap_tip(tip.clone()).build(window, cx))
                .on_click(cx.listener(move |_, _, _, cx| {
                    let (cwd, title) = summary
                        .as_ref()
                        .map(|s| (s.cwd.clone(), s.display_title()))
                        .unwrap_or_default();
                    cx.emit(RequestCardsEvent::OpenSession(SessionRef {
                        key: SessionKey::new(&agent, &sid),
                        cwd,
                        title,
                    }));
                }))
        });
        let end = h_flex()
            .flex_none()
            .ml_auto()
            .gap_2()
            .when(page && !collapsed && show_agent, |el| {
                el.child(badge(agent_name.clone(), flat, &p, &t))
            })
            .when_some(marker, |el, (icon, color, label)| {
                el.child(
                    div()
                        .id(kit::keyed("req-marker", key.to_string()))
                        .flex_none()
                        .child(kit::icon(icon, 13., color))
                        .tooltip(move |window, cx| kit::wrap_tip(label).build(window, cx)),
                )
            })
            .when_some(resolution, |el, r| el.child(badge(r, flat, &p, &t)))
            .when(offline, |el| {
                el.child(badge("Temporarily unavailable", flat, &p, &t))
            })
            .children(open_session);
        let header = h_flex()
            .w_full()
            .gap(px(8.8))
            .map(|el| {
                if collapsed {
                    el.items_center()
                } else {
                    el.items_start()
                }
            })
            .children(chevron)
            .child(copy)
            .child(end);

        // ---- session line (Inbox page)
        let context = (page && !collapsed).then(|| {
            h_flex()
                .gap_2()
                .min_w_0()
                .text_xs()
                .text_color(p.dim)
                .when_some(req.session_id.clone(), |el, s| {
                    el.child(kit::identicon(&s, 14.))
                })
                .child(
                    div().font_medium().text_color(p.muted).truncate().child(
                        summary
                            .as_ref()
                            .map(|s| s.display_title())
                            .unwrap_or_else(|| "Session".into()),
                    ),
                )
                .when_some(summary.as_ref().map(|s| workspace_name(&s.cwd)), |el, w| {
                    el.child("·").child(w)
                })
                .child("·")
                .child(ago(req.received_ms))
        });

        // ---- body
        let mut body: Vec<AnyElement> = Vec::new();
        if !collapsed {
            if let RequestKind::Permission { summary, .. } = &req.kind
                && !summary.is_empty()
            {
                body.push(
                    div()
                        .mt_3()
                        .px_3()
                        .py_2()
                        .rounded(px(8.))
                        .bg(p.bg)
                        .border_1()
                        .border_color(p.border)
                        .font_family(kit::MONO)
                        .text_xs()
                        .child(summary.clone())
                        .into_any_element(),
                );
            }
            let fields = req.fields().to_vec();
            if !fields.is_empty() && (pending || req.submitted()) {
                let blocks: Vec<AnyElement> = fields
                    .iter()
                    .enumerate()
                    .map(|(fix, f)| {
                        self.field_block(&req, fix, f, locked, flat, &p, &t, window, cx)
                    })
                    .collect();
                body.push(v_flex().mt_4().gap_3().children(blocks).into_any_element());
            }
            if offline {
                body.push(
                    h_flex()
                        .mt_4()
                        .gap_2()
                        .items_start()
                        .text_sm()
                        .text_color(p.muted)
                        .child(div().flex_none().mt(px(2.)).child(kit::icon(
                            IconName::PlugZap,
                            14.,
                            p.muted,
                        )))
                        .child(div().flex_1().min_w_0().child(
                            "Waiting to reconnect. This question is temporarily unavailable, not \
                             cancelled; your draft is kept and you can answer once the connection \
                             recovers.",
                        ))
                        .into_any_element(),
                );
            }
            if let Some(error) = req.error.clone() {
                body.push(
                    div()
                        .mt_4()
                        .px_4()
                        .py_3()
                        .rounded(px(12.))
                        .border_1()
                        .border_color(t.danger_border)
                        .bg(t.danger_bg)
                        .text_sm()
                        .text_color(p.danger)
                        .child(error)
                        .into_any_element(),
                );
            }
            if pending {
                let buttons: Vec<AnyElement> = req
                    .actions()
                    .into_iter()
                    .map(|a| {
                        let (k, id) = (key.to_string(), a.id.clone());
                        card_btn(
                            kit::keyed("req-action", format!("{key}/{}", a.id)),
                            a.label,
                            a.primary,
                            flat,
                            !locked,
                            &p,
                            &t,
                        )
                        .when(!locked, |el| {
                            el.on_click(
                                cx.listener(move |this, _, _, cx| this.request_action(&k, &id, cx)),
                            )
                        })
                        .into_any_element()
                    })
                    .collect();
                body.push(
                    h_flex()
                        .mt_4()
                        .gap_2()
                        .flex_wrap()
                        .children(buttons)
                        .into_any_element(),
                );
            }
        }

        Some(
            v_flex()
                .w_full()
                .min_w_0()
                .p(px(if !page || collapsed { 12. } else { 16. }))
                .rounded(px(12.))
                .map(|el| {
                    if flat {
                        el.bg(t.pill)
                    } else {
                        el.bg(p.card).border_1().border_color(p.border)
                    }
                })
                .when_some(context, |el, c| el.child(c.mb_3()))
                .child(header)
                .children(body)
                .into_any_element(),
        )
    }
}

impl QmtApp {
    pub(super) fn render_inbox(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = *palette(cx);
        let t = tones(&p);
        let store = self.store.read(cx);
        // Newest first; answered cards stay listed below the open ones' time order.
        let keys = self.inbox_cards.read(cx).keys(&RequestFilter::All, cx);
        let pending = store.pending_request_count();
        let disconnected = store.connected_ids().is_empty();

        let header = kit::page_header(
            "Inbox",
            (pending > 0).then(|| kit::tone_chip(pending.to_string(), p.accent).into_any_element()),
            Some("Permission and input requests waiting for your response."),
            Vec::new(),
            &p,
        );

        let body = if disconnected && keys.is_empty() {
            panel(&p)
                .p_4()
                .child(
                    h_flex()
                        .gap_3()
                        .child(kit::icon(IconName::PlugZap, 18., p.dim))
                        .child(
                            v_flex()
                                .flex_1()
                                .child(div().text_sm().font_semibold().child("No agents connected"))
                                .child(div().text_xs().text_color(p.muted).child(
                                    "Connect an agent to receive permission and input requests.",
                                )),
                        )
                        .child(
                            card_btn("inbox-agents", "Open agents", true, true, true, &p, &t)
                                .on_click(cx.listener(|this, _, _, cx| this.go(Page::Agents, cx))),
                        ),
                )
                .into_any_element()
        } else if keys.is_empty() {
            h_flex()
                .gap_3()
                .px_4()
                .py(px(14.4))
                .rounded(px(12.))
                .bg(t.pill)
                .child(kit::icon(IconName::CircleCheck, 16., p.success))
                .child(
                    v_flex()
                        .child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .child("No requests need attention"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.muted)
                                .child("New permission and input requests will appear here."),
                        ),
                )
                .into_any_element()
        } else {
            let cards: Vec<AnyElement> = keys
                .iter()
                .enumerate()
                .filter_map(|(ix, k)| {
                    self.inbox_cards.update(cx, |cards, cx| {
                        cards.request_card(k, true, ix >= 3, window, cx)
                    })
                })
                .collect();
            v_flex().gap_3().children(cards).into_any_element()
        };

        kit::page([header.into_any_element(), body])
    }
}
