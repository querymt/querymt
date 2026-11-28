//! Updates in the UI: the card in Settings > General, and the sidebar's
//! "Update available" item.
//!
//! The card answers four questions in reading order: which version runs on
//! which channel, whether to look automatically, what the last check found, and
//! (only when a newer version exists) what can be done about it. Builds that
//! cannot update themselves show one line and no controls.

use gpui_kit::assets::IconName;
use gpui_kit::component::h_flex;
use gpui_kit::*;

use super::QmtApp;
use super::kit::{self, flat_btn};
use super::settings::{settings_card, simple_row};
use crate::theme::Palette;
use crate::update::Channel;
use crate::update::check::UpdateState;
use crate::update::install_kind::NotifyReason;

/// Why this install shows the new version without installing it.
pub(super) fn notify_copy(reason: NotifyReason) -> &'static str {
    match reason {
        NotifyReason::Translocated => {
            "Move QueryMT Desktop to Applications to get automatic updates."
        }
        NotifyReason::ReadOnly => {
            "This copy is in a folder the app cannot change. Download the new version to update it."
        }
        NotifyReason::Unmanaged => {
            "This install is not updated by the app. Update it with your package manager, or download the new version."
        }
    }
}

/// "3m ago" for something that happened `secs` seconds before `now`.
fn ago(now: u64, then: u64) -> String {
    match now.saturating_sub(then) {
        0..60 => "just now".into(),
        s @ 60..3600 => format!("{}m ago", s / 60),
        s @ 3600..86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The status row: a short title, and the detail under it. When nothing
/// else is going on, the detail also names the running version.
fn status_lines(
    state: &UpdateState,
    checking: bool,
    last_check: Option<u64>,
    version: &str,
) -> (String, String) {
    let last = match last_check {
        Some(t) => format!("Last checked {}.", ago(now_secs(), t)),
        None => "Not checked yet.".to_owned(),
    };
    if checking {
        return ("Checking".into(), last);
    }
    let running = format!("QueryMT Desktop {version}. {last}");
    match state {
        UpdateState::Idle => ("Updates".into(), running),
        UpdateState::UpToDate => ("Up to date".into(), running),
        UpdateState::Available(offer) => (
            "Update available".into(),
            format!("Version {} is available.", offer.version),
        ),
        UpdateState::Downloading(progress) => {
            let percent = (progress.received * 100)
                .checked_div(progress.total)
                .unwrap_or(0);
            ("Downloading".into(), format!("{percent}% of the update."))
        }
        UpdateState::Installing => ("Installing".into(), "Replacing the installed files.".into()),
        UpdateState::ReadyToRestart { version } => (
            "Restart to finish updating".into(),
            format!("Version {version} is installed. It starts the next time you open the app."),
        ),
        UpdateState::Failed(why) => ("Update failed".into(), why.clone()),
    }
}

impl QmtApp {
    /// The Updates card (Settings > General).
    pub(super) fn settings_updates(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let store = self.store.read(cx);
        let version = store.running_version().to_owned();
        if !store.updates_available() {
            return settings_card(p).child(simple_row(
                "Updates",
                format!("QueryMT Desktop {version}. This build does not check for updates."),
                div().debug_selector(|| "update-unavailable".into()),
                true,
                p,
            ));
        }
        let channel = store.shown_update_channel();
        let auto = store.prefs.updates.auto_check;
        let last_check = store.prefs.updates.last_check;
        let checking = store.updates.checking();
        let busy = store.update_busy();
        let state = store.updates.state.clone();

        let channels = kit::segment_bar(p).children([Channel::Stable, Channel::Nightly].map(|c| {
            let (id, label) = match c {
                Channel::Stable => ("update-channel-stable", "Stable"),
                Channel::Nightly => ("update-channel-nightly", "Nightly"),
            };
            div().debug_selector(move || id.into()).child(
                kit::segment(id, channel == c, p)
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.store.update(cx, |s, cx| s.set_update_channel(c, cx))
                    })),
            )
        }));

        let (title, detail) = status_lines(&state, checking, last_check, &version);
        // The action of the status row: restarting once an install is done,
        // looking again otherwise (not while a download or install runs).
        let action = if matches!(state, UpdateState::ReadyToRestart { .. }) {
            kit::accent_btn("update-restart", true, true, p)
                .debug_selector(|| "update-restart".into())
                .child("Restart now")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.store.update(cx, |s, cx| s.restart_to_update(cx))
                }))
                .into_any_element()
        } else {
            flat_btn("update-check", !checking && !busy, true, p)
                .debug_selector(|| "update-check".into())
                .child("Check now")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.store.update(cx, |s, cx| {
                        s.check_updates(true, cx);
                    })
                }))
                .into_any_element()
        };

        let mut card = settings_card(p)
            .child(simple_row(
                "Update channel",
                "Stable gets releases. Nightly gets a build every night.",
                channels,
                true,
                p,
            ))
            .child(simple_row(
                "Check automatically",
                "Look for a new version shortly after start and every 6 hours.",
                kit::switch("sw-update-auto")
                    .checked(auto)
                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                        let v = *v;
                        this.store.update(cx, |s, cx| s.set_auto_check(v, cx));
                    })),
                false,
                p,
            ))
            .child(
                div()
                    .debug_selector(|| "update-state".into())
                    .child(simple_row(title, detail, action, false, p)),
            );
        if let UpdateState::Available(offer) = &state {
            card = card.child(self.update_offer_row(offer, p, cx));
        }
        card
    }

    /// What can be done about the offered version.
    fn update_offer_row(
        &self,
        offer: &crate::update::check::Offer,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let kind = self.store.update(cx, |s, _| s.offer_action());
        use crate::store::updates::OfferAction;
        let (reason, closes_app) = match kind {
            OfferAction::NotifyOnly(reason) => (Some(reason), false),
            OfferAction::Install { closes_app } => (None, closes_app),
        };
        let url = offer.release_page.clone();
        let release = div()
            .id("update-release-link")
            .debug_selector(|| "update-release-link".into())
            .flex_none()
            .cursor_pointer()
            .text_size(kit::rpx(12.))
            .text_color(p.accent)
            .underline()
            .child("Release page")
            .on_click(move |_, _, cx| cx.open_url(&url));
        let skip = flat_btn("update-skip", true, true, p)
            .debug_selector(|| "update-skip".into())
            .child("Skip this version")
            .on_click(
                cx.listener(|this, _, _, cx| this.store.update(cx, |s, cx| s.skip_version(cx))),
            );
        let desc = match (reason, closes_app) {
            (Some(reason), _) => notify_copy(reason).to_owned(),
            (None, true) => {
                "QueryMT Desktop closes while the update installs, then starts again.".to_owned()
            }
            (None, false) => "The release notes are on the release page.".to_owned(),
        };
        // The default commit of the row is the install; it is the accent
        // button, last. Install kinds that only notify have no such action.
        let install =
            reason.is_none().then(|| {
                kit::accent_btn("update-install", true, true, p)
                    .debug_selector(|| "update-install".into())
                    .child(if closes_app {
                        "Install and restart"
                    } else {
                        "Install update"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.store.update(cx, |s, cx| s.install_update(cx))
                    }))
            });
        let row = simple_row(
            format!("Version {}", offer.version),
            desc,
            h_flex()
                .gap_3()
                .items_center()
                .child(release)
                .child(skip)
                .children(install),
            false,
            p,
        );
        // The move hint is the row itself when the app is translocated.
        let row = if reason == Some(NotifyReason::Translocated) {
            div()
                .debug_selector(|| "update-move-hint".into())
                .child(row)
        } else {
            div().child(row)
        };
        div().debug_selector(|| "update-offer".into()).child(row)
    }

    /// The sidebar's "Update available" item, when there is something to do.
    pub(super) fn update_nav(
        &self,
        rail: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let (label, tip) = match &store.updates.state {
            UpdateState::Available(offer) => (
                "Update available",
                format!("Version {} is available", offer.version),
            ),
            UpdateState::ReadyToRestart { version } => (
                "Restart to update",
                format!("Version {version} is installed"),
            ),
            _ => return None,
        };
        let open = |this: &mut Self, cx: &mut Context<Self>| {
            this.settings_tab = super::settings::SettingsTab::General;
            this.go(super::Page::Settings, cx);
        };
        let item = if rail {
            kit::rail_button("update-nav", false, p)
                .tooltip(move |window, cx| kit::wrap_tip(tip.clone()).build(window, cx))
                .child(kit::icon(IconName::CircleArrowUp, 18., p.accent))
                .on_click(cx.listener(move |this, _, _, cx| open(this, cx)))
                .into_any_element()
        } else {
            let hover_bg = p.card_hover;
            h_flex()
                .id("update-nav")
                .gap_2p5()
                .px_2()
                .h(px(32.))
                .rounded(px(8.))
                .cursor_pointer()
                .text_sm()
                .text_color(p.text)
                .hover(move |el| el.bg(hover_bg))
                .tooltip(move |window, cx| kit::wrap_tip(tip.clone()).build(window, cx))
                .child(kit::icon(IconName::CircleArrowUp, 16., p.accent))
                .child(div().flex_1().child(label))
                .on_click(cx.listener(move |this, _, _, cx| open(this, cx)))
                .into_any_element()
        };
        Some(
            div()
                .debug_selector(|| "update-nav".into())
                .child(item)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: it would glob-import gpui's `test` macro over the
    // built-in `#[test]` that `#[gpui_kit::test]` expands to.
    use super::{Channel, NotifyReason, UpdateState, notify_copy};
    use crate::store::automations::schedule_ui_fixture;
    use crate::ui::{Page, QmtApp, settings::SettingsTab};
    use crate::update::BuildChannel;
    use crate::update::check::Offer;
    use crate::update::install_kind::InstallKind;
    use gpui_kit::AppContext as _;
    use gpui_kit::test::TestWindowExt as _;

    fn offer() -> Offer {
        Offer {
            version: "0.7.0".into(),
            channel: Channel::Stable,
            asset: None,
            release_page: "https://github.com/querymt/querymt/releases/tag/v0.7.0".into(),
        }
    }

    /// Settings > General of a release build that found a newer version and
    /// is installed the way `kind` says.
    fn open_general(
        kind: InstallKind,
        cx: &mut gpui_kit::TestAppContext,
    ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<QmtApp>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
        });
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| schedule_ui_fixture());
            store.update(cx, |s, _| {
                s.configure_updates_for_test(BuildChannel::Stable, kind);
                s.updates.state = UpdateState::Available(offer());
            });
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.go(Page::Settings, cx);
                app.settings_tab = SettingsTab::General;
            });
            window.render_frame(cx);
        })
        .unwrap();
        (handle, app)
    }

    #[gpui_kit::test]
    fn notify_only_shows_release_link(cx: &mut gpui_kit::TestAppContext) {
        // Installed from the deb: the version and a link, and no install.
        let (handle, app) = open_general(InstallKind::NotifyOnly(NotifyReason::Unmanaged), cx);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(
            vcx.debug_bounds("update-offer").is_some(),
            "the offer is shown"
        );
        assert!(vcx.debug_bounds("update-release-link").is_some());
        assert!(vcx.debug_bounds("update-install").is_none());
        assert!(vcx.debug_bounds("update-move-hint").is_none());
        let page = cx.update(|cx| match &app.read(cx).store.read(cx).updates.state {
            UpdateState::Available(o) => o.release_page.clone(),
            other => panic!("{other:?}"),
        });
        assert!(page.ends_with("/releases/tag/v0.7.0"), "{page}");
        assert!(notify_copy(NotifyReason::Unmanaged).contains("package manager"));
    }

    #[gpui_kit::test]
    fn translocated_shows_move_hint(cx: &mut gpui_kit::TestAppContext) {
        let (handle, _app) = open_general(InstallKind::NotifyOnly(NotifyReason::Translocated), cx);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("update-move-hint").is_some());
        assert!(vcx.debug_bounds("update-release-link").is_some());
        assert!(vcx.debug_bounds("update-install").is_none());
        assert!(
            notify_copy(NotifyReason::Translocated)
                .contains("Move QueryMT Desktop to Applications")
        );
    }

    #[gpui_kit::test]
    fn development_build_shows_updates_unavailable(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
        });
        // This test binary is a development build: no controls.
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| schedule_ui_fixture());
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.go(Page::Settings, cx);
                app.settings_tab = SettingsTab::General;
            });
            window.render_frame(cx);
        })
        .unwrap();
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("update-unavailable").is_some());
        assert!(vcx.debug_bounds("update-check").is_none());
        assert!(vcx.debug_bounds("sw-update-auto").is_none());
    }

    #[gpui_kit::test]
    fn the_card_controls_update_the_preferences(cx: &mut gpui_kit::TestAppContext) {
        let (handle, app) = open_general(InstallKind::NotifyOnly(NotifyReason::Unmanaged), cx);
        let store = cx.update(|cx| app.read(cx).store.clone());
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        let nightly = vcx.debug_bounds("update-channel-nightly").expect("channel");
        vcx.simulate_click(nightly.center(), gpui_kit::Modifiers::none());
        assert_eq!(
            store.read_with(cx, |s, _| s.prefs.updates.channel),
            Some(Channel::Nightly)
        );
    }

    #[gpui_kit::test]
    fn the_sidebar_item_opens_the_updates_card(cx: &mut gpui_kit::TestAppContext) {
        let (handle, app) = open_general(InstallKind::NotifyOnly(NotifyReason::Unmanaged), cx);
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| app.go(Page::Start, cx));
            window.render_frame(cx);
        })
        .unwrap();
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        let item = vcx
            .debug_bounds("update-nav")
            .expect("Update available item");
        vcx.simulate_click(item.center(), gpui_kit::Modifiers::none());
        assert!(app.read_with(cx, |app, _| app.page == Page::Settings
            && app.settings_tab == SettingsTab::General));
    }

    #[gpui_kit::test]
    fn the_install_button_is_not_shown_for_notify_only_installs(cx: &mut gpui_kit::TestAppContext) {
        // A copy in a folder the app cannot change: the version and a link,
        // and nothing to install.
        let (handle, _app) = open_general(InstallKind::NotifyOnly(NotifyReason::ReadOnly), cx);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("update-offer").is_some());
        assert!(vcx.debug_bounds("update-release-link").is_some());
        assert!(vcx.debug_bounds("update-install").is_none());
        assert!(vcx.debug_bounds("update-restart").is_none());
    }

    #[gpui_kit::test]
    fn an_installable_offer_has_an_install_button(cx: &mut gpui_kit::TestAppContext) {
        let (handle, app) = open_general(
            InstallKind::AppImage {
                path: "/home/me/QueryMT.AppImage".into(),
            },
            cx,
        );
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("update-install").is_some());
        assert!(vcx.debug_bounds("update-skip").is_some());
        assert!(vcx.debug_bounds("update-release-link").is_some());

        // The offer in this test has no installer for the platform, so the
        // click fails visibly instead of downloading anything.
        let install = vcx.debug_bounds("update-install").unwrap();
        vcx.simulate_click(install.center(), gpui_kit::Modifiers::none());
        let state = app.read_with(&vcx, |app, cx| app.store.read(cx).updates.state.clone());
        match state {
            UpdateState::Failed(why) => assert!(why.contains("no installer"), "{why}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[gpui_kit::test]
    fn ready_to_restart_offers_restart_now(cx: &mut gpui_kit::TestAppContext) {
        let (handle, app) = open_general(
            InstallKind::MacApp {
                bundle: "/Applications/QueryMT Desktop.app".into(),
            },
            cx,
        );
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.store.update(cx, |s, _| {
                    s.updates.state = UpdateState::ReadyToRestart {
                        version: "0.7.0".into(),
                    }
                })
            });
            window.render_frame(cx);
        })
        .unwrap();
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        // Restarting replaces "Check now" as the row's action, and the offer
        // row is gone. (Not clicked: it would start the new version.)
        assert!(vcx.debug_bounds("update-restart").is_some());
        assert!(vcx.debug_bounds("update-check").is_none());
        assert!(vcx.debug_bounds("update-offer").is_none());
    }
}
