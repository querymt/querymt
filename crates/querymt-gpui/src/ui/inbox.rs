//! Inbox: permission and input (elicitation) requests waiting for the user.
//! Cards render here and inline in the session view.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Selectable as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::{Map, Value};

use super::mesh::panel;
use super::{Page, QmtApp, kit};
use crate::store::{FieldKind, PendingRequest, RequestKind};
use crate::theme::palette;
use crate::util::{now_ms, workspace_name};

fn ago(ms: f64) -> String {
    let secs = ((now_ms() - ms) / 1000.).max(0.) as u64;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
}

impl QmtApp {
    fn field_input(
        &mut self,
        req: &str,
        key: &str,
        default: Option<&Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let k = format!("{req}/{key}");
        if let Some(e) = self.form_inputs.get(&k) {
            return e.clone();
        }
        let initial = default
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_default();
        let e = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        self.form_inputs.insert(k, e.clone());
        e
    }

    fn submit_elicitation(&mut self, req: &PendingRequest, cx: &mut Context<Self>) {
        let RequestKind::Elicitation { fields, .. } = &req.kind else {
            return;
        };
        let mut content = Map::new();
        for f in fields {
            let k = format!("{}/{}", req.key, f.key);
            let value = match &f.kind {
                FieldKind::Bool | FieldKind::Enum(_) => self
                    .form_values
                    .get(&k)
                    .cloned()
                    .or_else(|| f.default.clone()),
                kind => {
                    let text = self
                        .form_inputs
                        .get(&k)
                        .map(|e| e.read(cx).value().trim().to_string())
                        .unwrap_or_default();
                    if text.is_empty() {
                        None
                    } else {
                        match kind {
                            FieldKind::Integer => match text.parse::<i64>() {
                                Ok(n) => Some(Value::from(n)),
                                Err(_) => {
                                    self.form_errors.insert(
                                        req.key.clone(),
                                        format!("{} must be a whole number.", f.title),
                                    );
                                    cx.notify();
                                    return;
                                }
                            },
                            FieldKind::Number => match text.parse::<f64>() {
                                Ok(n) => Some(Value::from(n)),
                                Err(_) => {
                                    self.form_errors.insert(
                                        req.key.clone(),
                                        format!("{} must be a number.", f.title),
                                    );
                                    cx.notify();
                                    return;
                                }
                            },
                            _ => Some(Value::from(text)),
                        }
                    }
                }
            };
            match value {
                Some(v) => {
                    content.insert(f.key.clone(), v);
                }
                None if f.required => {
                    self.form_errors
                        .insert(req.key.clone(), format!("{} is required.", f.title));
                    cx.notify();
                    return;
                }
                None => {}
            }
        }
        self.clear_form(&req.key);
        let key = req.key.clone();
        self.store.update(cx, |s, cx| {
            s.answer_elicitation(&key, "accept", Some(Value::Object(content)), cx)
        });
    }

    fn clear_form(&mut self, req: &str) {
        let prefix = format!("{req}/");
        self.form_inputs.retain(|k, _| !k.starts_with(&prefix));
        self.form_values.retain(|k, _| !k.starts_with(&prefix));
        self.form_errors.remove(req);
    }

    /// Card for one pending request. `with_context` adds the session line (Inbox page).
    pub(super) fn request_card(
        &mut self,
        key: &str,
        with_context: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let p = *palette(cx);
        let store = self.store.read(cx);
        let req = store.requests.iter().find(|r| r.key == key)?.clone();
        let agent_name = store
            .config(&req.agent_id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let summary = req
            .session_id
            .as_deref()
            .and_then(|s| store.session_summary(&req.agent_id, s))
            .cloned();
        let multi_agent = store.configs.len() > 1;

        let context = with_context.then(|| {
            let summary = summary.clone();
            let (agent, sid) = (req.agent_id.clone(), req.session_id.clone());
            h_flex()
                .gap_2()
                .text_xs()
                .text_color(p.dim)
                .when_some(sid.clone(), |el, s| el.child(kit::identicon(&s, 14.)))
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
                .when(multi_agent, |el| el.child("·").child(agent_name.clone()))
                .child("·")
                .child(ago(req.received_ms))
                .child(div().flex_1())
                .when_some(sid, |el, sid| {
                    el.child(
                        kit::button(SharedString::from(format!("open-{}", req.key)))
                            .xsmall()
                            .ghost()
                            .icon(IconName::ExternalLink)
                            .label("Open session")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let (cwd, title) = summary
                                    .as_ref()
                                    .map(|s| (s.cwd.clone(), s.display_title()))
                                    .unwrap_or_default();
                                this.open_session(&agent, &sid, &cwd, &title, cx);
                            })),
                    )
                })
        });

        let body = match &req.kind {
            RequestKind::Permission {
                title,
                summary,
                kind,
                options,
            } => {
                let buttons = options.iter().enumerate().map(|(ix, o)| {
                    let (k, oid) = (req.key.clone(), o.id.clone());
                    let b = kit::button(SharedString::from(format!("perm-{}-{ix}", req.key)))
                        .small()
                        .label(o.name.clone());
                    let b = match o.kind.as_str() {
                        "allow_once" => b.primary(),
                        "reject_once" | "reject_always" => b.ghost(),
                        _ => b,
                    };
                    b.on_click(cx.listener(move |this, _, _, cx| {
                        this.store
                            .update(cx, |s, cx| s.answer_permission(&k, &oid, cx))
                    }))
                });
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(kit::icon(IconName::ShieldAlert, 16., p.warn))
                            .child(div().text_sm().font_semibold().child(format!(
                                "Allow {}?",
                                title.strip_prefix("Run ").unwrap_or(title)
                            )))
                            .when(!kind.is_empty(), |el| el.child(kit::chip(kind.clone(), &p))),
                    )
                    .when(!summary.is_empty(), |el| {
                        el.child(
                            div()
                                .px_3()
                                .py_2()
                                .rounded(px(8.))
                                .bg(p.bg)
                                .border_1()
                                .border_color(p.border)
                                .font_family(kit::MONO)
                                .text_xs()
                                .child(summary.clone()),
                        )
                    })
                    .child(h_flex().gap_2().flex_wrap().children(buttons))
                    .into_any_element()
            }
            RequestKind::Elicitation {
                message,
                fields,
                url,
            } => {
                let mut rows: Vec<AnyElement> = Vec::new();
                for (fix, f) in fields.iter().enumerate() {
                    let fk = format!("{}/{}", req.key, f.key);
                    let control: AnyElement = match &f.kind {
                        FieldKind::Bool => {
                            let on = self
                                .form_values
                                .get(&fk)
                                .or(f.default.as_ref())
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            let fk = fk.clone();
                            Switch::new(SharedString::from(format!("sw-{fk}")))
                                .checked(on)
                                .on_click(cx.listener(move |this, v: &bool, _, cx| {
                                    this.form_values.insert(fk.clone(), Value::from(*v));
                                    cx.notify();
                                }))
                                .into_any_element()
                        }
                        FieldKind::Enum(choices) => {
                            let current = self
                                .form_values
                                .get(&fk)
                                .or(f.default.as_ref())
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                            h_flex()
                                .gap_1()
                                .flex_wrap()
                                .children(choices.iter().enumerate().map(
                                    |(cix, (value, label))| {
                                        let (fk, value) = (fk.clone(), value.clone());
                                        kit::button(SharedString::from(format!("en-{fk}-{cix}")))
                                            .xsmall()
                                            .label(label.clone())
                                            .selected(current.as_deref() == Some(value.as_str()))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.form_values
                                                    .insert(fk.clone(), Value::from(value.clone()));
                                                cx.notify();
                                            }))
                                    },
                                ))
                                .into_any_element()
                        }
                        _ => {
                            let input =
                                self.field_input(&req.key, &f.key, f.default.as_ref(), window, cx);
                            Input::new(&input).small().into_any_element()
                        }
                    };
                    rows.push(
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .text_xs()
                                    .font_medium()
                                    .child(f.title.clone())
                                    .when(f.required, |el| {
                                        el.child(div().text_color(p.danger).child("*"))
                                    }),
                            )
                            .child(control)
                            .when(!f.description.is_empty(), |el| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(p.dim)
                                        .child(f.description.clone()),
                                )
                            })
                            .id(SharedString::from(format!("field-{}-{fix}", req.key)))
                            .into_any_element(),
                    );
                }
                let error = self.form_errors.get(&req.key).cloned();
                let (k1, k2) = (req.key.clone(), req.key.clone());
                let req2 = req.clone();
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(kit::icon(IconName::Hand, 16., p.accent))
                            .child(div().text_sm().font_semibold().child("Input requested")),
                    )
                    .when(!message.is_empty(), |el| {
                        el.child(div().text_sm().child(message.clone()))
                    })
                    .when_some(url.clone(), |el, url| {
                        el.child(
                            kit::button(SharedString::from(format!("url-{}", req.key)))
                                .small()
                                .icon(IconName::ExternalLink)
                                .label("Open link")
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    })
                    .children(rows)
                    .when_some(error, |el, e| {
                        el.child(div().text_xs().text_color(p.danger).child(e))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                kit::button(SharedString::from(format!("submit-{}", req.key)))
                                    .small()
                                    .primary()
                                    .label("Submit")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.submit_elicitation(&req2, cx)
                                    })),
                            )
                            .child(
                                kit::button(SharedString::from(format!("decline-{}", req.key)))
                                    .small()
                                    .ghost()
                                    .label("Decline")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.clear_form(&k1);
                                        this.store.update(cx, |s, cx| {
                                            s.answer_elicitation(&k1, "decline", None, cx)
                                        });
                                    })),
                            )
                            .child(
                                kit::button(SharedString::from(format!("cancel-{}", req.key)))
                                    .small()
                                    .ghost()
                                    .label("Cancel")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.clear_form(&k2);
                                        this.store.update(cx, |s, cx| {
                                            s.answer_elicitation(&k2, "cancel", None, cx)
                                        });
                                    })),
                            ),
                    )
                    .into_any_element()
            }
        };

        Some(
            v_flex()
                .w_full()
                .gap_2()
                .p_4()
                .rounded(px(12.))
                .bg(p.surface)
                .border_1()
                .border_color(p.accent.opacity(0.45))
                .children(context)
                .child(body)
                .into_any_element(),
        )
    }

    pub(super) fn render_inbox(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = *palette(cx);
        let store = self.store.read(cx);
        let mut keys: Vec<(f64, String)> = store
            .requests
            .iter()
            .map(|r| (r.received_ms, r.key.clone()))
            .collect();
        keys.sort_by(|a, b| b.0.total_cmp(&a.0));
        let disconnected = store.connected_ids().is_empty();

        let header = v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_2xl().font_bold().child("Inbox"))
                    .when(!keys.is_empty(), |el| {
                        el.child(kit::tone_chip(keys.len().to_string(), p.accent))
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(p.muted)
                    .child("Permission and input requests waiting for your response."),
            );

        let body =
            if disconnected && keys.is_empty() {
                panel(&p)
                .p_4()
                .child(
                    h_flex()
                        .gap_3()
                        .child(kit::icon(IconName::Bot, 18., p.dim))
                        .child(
                            v_flex()
                                .flex_1()
                                .child(div().text_sm().font_semibold().child("No agent connected"))
                                .child(div().text_xs().text_color(p.muted).child(
                                    "Requests from agents show up here once they are connected.",
                                )),
                        )
                        .child(
                            kit::button("inbox-agents")
                                .label("Open Agents")
                                .on_click(cx.listener(|this, _, _, cx| this.go(Page::Agents, cx))),
                        ),
                )
                .into_any_element()
            } else if keys.is_empty() {
                panel(&p)
                    .p_4()
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                div()
                                    .size(px(32.))
                                    .rounded(px(8.))
                                    .bg(p.card)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(kit::icon(IconName::CircleCheck, 16., p.success)),
                            )
                            .child(
                                v_flex()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_semibold()
                                            .child("No requests need attention"),
                                    )
                                    .child(div().text_xs().text_color(p.muted).child(
                                        "New permission and input requests will appear here.",
                                    )),
                            ),
                    )
                    .into_any_element()
            } else {
                let cards: Vec<AnyElement> = keys
                    .iter()
                    .filter_map(|(_, k)| self.request_card(k, true, window, cx))
                    .collect();
                v_flex().gap_3().children(cards).into_any_element()
            };

        div().w_full().flex().justify_center().px_8().py_6().child(
            v_flex()
                .w_full()
                .max_w(px(1040.))
                .gap_5()
                .child(header)
                .child(body),
        )
    }
}
