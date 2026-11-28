//! Small presentational helpers shared by the pages.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    Disableable as _, Icon, Selectable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::store::ConnState;
use crate::theme::Palette;

pub const MONO: &str = "IBM Plex Mono";
/// The platform's own UI font (GPUI's special family name).
pub const SYSTEM_UI: &str = ".SystemUIFont";

/// Single-line sidebar title that fades into its row instead of showing an
/// ellipsis. The row owns the hover group so the fade follows its background.
pub fn fading_title(
    title: impl Into<SharedString>,
    group: SharedString,
    background: Hsla,
    hovered_background: Hsla,
) -> Div {
    let fade = |color: Hsla| {
        linear_gradient(
            90.,
            linear_color_stop(color.opacity(0.), 0.),
            linear_color_stop(color, 1.),
        )
    };
    div()
        .relative()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_sm()
        .child(title.into())
        .child(
            div()
                .absolute()
                .right_0()
                .top_0()
                .bottom_0()
                .w_6()
                .bg(fade(background))
                .group_hover(group, move |el| el.bg(fade(hovered_background))),
        )
}

/// Stable identity for a repeated element, keyed by a domain id (session id,
/// agent id, attachment id, …) rather than its position, so hover, tooltip and
/// other keyed element state stays with the item when the list reorders.
pub fn keyed(name: &'static str, key: impl Into<SharedString>) -> ElementId {
    ElementId::from((ElementId::from(name), key.into()))
}

pub fn icon(name: IconName, size: f32, color: Hsla) -> Icon {
    Icon::new(name).size(px(size)).text_color(color)
}

/// Icon and colour of a plan entry by its status: shared by the title-bar
/// plan popover and the quick panel's session cards.
pub fn plan_entry_mark(status: &str, p: &Palette) -> (IconName, Hsla) {
    match status {
        "completed" => (IconName::CircleCheck, p.success),
        "in_progress" => (IconName::CircleDashed, p.running),
        _ => (IconName::Circle, p.dim),
    }
}

/// App button: gpui-component buttons keep the arrow cursor, ours show a
/// pointer. Disable through [`ButtonExt::inactive`] so the arrow comes back.
pub fn button(id: impl Into<ElementId>) -> Button {
    Button::new(id).cursor_pointer()
}

pub trait ButtonExt {
    /// `disabled(off)` plus the default cursor while disabled.
    fn inactive(self, off: bool) -> Self;
}

impl ButtonExt for Button {
    fn inactive(self, off: bool) -> Self {
        self.disabled(off).when(off, |b| b.cursor_default())
    }
}

/// Height of segmented pickers (steer/queue, mode, reasoning, …): the same as
/// the 32px default buttons they sit next to.
pub const SEGMENT_H: f32 = 32.;

/// Track of a segmented picker; fill it with [`segment`] buttons.
/// Remembers the mouse-down that dismissed a popover from outside. A popover
/// closes on mouse down, but its trigger toggles on the click that follows,
/// so without this a second click on the trigger would close and reopen it.
/// One mouse down can close several nested popovers, so all are kept.
#[derive(Default)]
pub struct Dismissed {
    at: Option<Point<Pixels>>,
    popovers: Vec<String>,
}

impl Dismissed {
    /// Call from the popover's `on_mouse_down_out`.
    pub fn record(&mut self, popover: impl Into<String>, ev: &MouseDownEvent) {
        if self.at != Some(ev.position) {
            self.at = Some(ev.position);
            self.popovers.clear();
        }
        self.popovers.push(popover.into());
    }

    /// Whether `click` on `popover`'s trigger is the click that just closed
    /// it (and so must not reopen it).
    pub fn closed_by(&mut self, popover: &str, click: &ClickEvent) -> bool {
        let at = self.at.take();
        let popovers = std::mem::take(&mut self.popovers);
        matches!(click, ClickEvent::Mouse(m) if at == Some(m.down.position))
            && popovers.iter().any(|p| p == popover)
    }
}

/// Fade durations (ms) of hover-revealed chrome.
pub const FADE_IN_MS: f64 = 100.;
pub const FADE_OUT_MS: f64 = 100.;

/// Opacity fade of hover-revealed chrome (scrollbars, copy buttons): where
/// the opacity was when the target last changed, so a change midway reverses
/// from the current value instead of jumping to an endpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fade {
    shown: bool,
    from: f32,
    at: f64,
}

impl Fade {
    /// Eased opacity at `now` (ms) and whether the fade is still running.
    /// With reduced motion the opacity snaps to the target.
    pub fn value(&self, now: f64, reduce_motion: bool) -> (f32, bool) {
        self.value_ms(now, reduce_motion, FADE_IN_MS, FADE_OUT_MS)
    }

    /// [`Fade::value`] with its own fade-in / fade-out durations (ms).
    pub fn value_ms(&self, now: f64, reduce_motion: bool, in_ms: f64, out_ms: f64) -> (f32, bool) {
        let (target, ms) = if self.shown {
            (1., in_ms)
        } else {
            (0., out_ms)
        };
        if reduce_motion {
            return (target, false);
        }
        let t = ((now - self.at) / ms).clamp(0., 1.) as f32;
        // Smoothstep: eases in and out.
        let eased = t * t * (3. - 2. * t);
        (self.from + (target - self.from) * eased, t < 1.)
    }

    /// Retarget the fade; `false` when it already heads there.
    pub fn show(&mut self, shown: bool, now: f64, reduce_motion: bool) -> bool {
        if self.shown == shown {
            return false;
        }
        self.from = self.value(now, reduce_motion).0;
        self.shown = shown;
        self.at = now;
        true
    }
}

/// Current opacity of the [`Fade`] state `fade`, asking for the next frame
/// while it runs (so only animating elements redraw).
pub fn fade_opacity(fade: &Entity<Fade>, window: &mut Window, cx: &App) -> f32 {
    fade_opacity_ms(fade, FADE_IN_MS, FADE_OUT_MS, window, cx)
}

/// [`fade_opacity`] with its own durations (ms).
pub fn fade_opacity_ms(
    fade: &Entity<Fade>,
    in_ms: f64,
    out_ms: f64,
    window: &mut Window,
    cx: &App,
) -> f32 {
    let (opacity, running) =
        fade.read(cx)
            .value_ms(crate::util::now_ms(), cx.reduce_motion(), in_ms, out_ms);
    if running {
        window.request_animation_frame();
    }
    opacity
}

#[cfg(test)]
mod fade_tests {
    use super::{FADE_IN_MS, FADE_OUT_MS, Fade};

    #[test]
    fn fade_eases_and_reverses_midway() {
        let mut fade = Fade::default();
        assert!(fade.show(true, 0., false));
        assert!(!fade.show(true, 0., false));
        assert_eq!(fade.value(0., false), (0., true));
        let (mid, running) = fade.value(FADE_IN_MS / 2., false);
        assert!((mid - 0.5).abs() < 1e-6 && running);
        // Eased: slow start.
        assert!(fade.value(FADE_IN_MS / 4., false).0 < 0.25);
        assert_eq!(fade.value(FADE_IN_MS * 2., false), (1., false));
        // Leaving midway fades out from where it was, not from fully shown.
        let mut out = fade;
        out.show(false, FADE_IN_MS / 2., false);
        assert_eq!(out.value(FADE_IN_MS / 2., false).0, mid);
        assert_eq!(out.value(FADE_IN_MS / 2. + FADE_OUT_MS, false), (0., false));
    }

    #[test]
    fn reduced_motion_snaps() {
        let mut fade = Fade::default();
        fade.show(true, 0., true);
        assert_eq!(fade.value(0., true), (1., false));
        fade.show(false, 1., true);
        assert_eq!(fade.value(1., true), (0., false));
    }
}

#[cfg(test)]
mod dismissed_tests {
    use super::Dismissed;
    use gpui_kit::{ClickEvent, MouseClickEvent, MouseDownEvent, point, px};

    fn down(x: f32) -> MouseDownEvent {
        MouseDownEvent {
            position: point(px(x), px(0.)),
            ..Default::default()
        }
    }

    fn click(x: f32) -> ClickEvent {
        ClickEvent::Mouse(MouseClickEvent {
            down: down(x),
            up: Default::default(),
        })
    }

    #[test]
    fn only_the_closing_click_is_swallowed() {
        let mut d = Dismissed::default();
        d.record("menu", &down(5.));
        d.record("panel", &down(5.));
        assert!(d.closed_by("panel", &click(5.)));
        // Consumed: the next click on the trigger toggles again.
        assert!(!d.closed_by("panel", &click(5.)));
        // A click elsewhere, or on another popover's trigger, is not swallowed.
        d.record("menu", &down(5.));
        assert!(!d.closed_by("menu", &click(9.)));
        d.record("menu", &down(5.));
        assert!(!d.closed_by("other", &click(5.)));
    }
}

/// Desktop `.action-btn` (`compact`: `.action-btn-compact`): bordered card
/// fill, stronger border + hover fill when enabled, dimmed when not. Also the
/// trigger of the app's pill selects.
pub fn action_btn(
    id: impl Into<ElementId>,
    enabled: bool,
    compact: bool,
    p: &Palette,
) -> Stateful<Div> {
    let (hover_border, hover_bg) = (p.border_strong, p.card_hover);
    btn_base(id, enabled, compact, p).when(enabled, |el| {
        el.hover(move |s| s.border_color(hover_border).bg(hover_bg))
    })
}

/// Shape, type and cursor of the app buttons, without a hover style: GPUI
/// allows one hover style per element, so each variant sets its own.
fn btn_base(id: impl Into<ElementId>, enabled: bool, compact: bool, p: &Palette) -> Stateful<Div> {
    h_flex()
        .id(id)
        .flex_none()
        .justify_center()
        .gap(px(if compact { 6.4 } else { 8. }))
        .h(px(if compact { 32. } else { 36. }))
        .px(px(if compact { 12. } else { 15.2 }))
        .rounded(px(8.))
        .border_1()
        .border_color(p.border)
        .bg(p.card)
        .text_color(p.text)
        .text_size(px(if compact { 12. } else { 13.1 }))
        .font_weight(FontWeight(550.))
        .map(|el| {
            if enabled {
                el.cursor_pointer()
            } else {
                el.opacity(0.5)
            }
        })
}

/// Borderless app button: [`action_btn`] sizing on a soft fill, the style of
/// the flat buttons used across the app (Settings, Agents, Mesh).
pub fn flat_btn(
    id: impl Into<ElementId>,
    enabled: bool,
    compact: bool,
    p: &Palette,
) -> Stateful<Div> {
    flat_tone(id, enabled, compact, p, flat_fill(p), p.text)
}

fn flat_tone(
    id: impl Into<ElementId>,
    enabled: bool,
    compact: bool,
    p: &Palette,
    (fill, fill_hover): (Hsla, Hsla),
    text: Hsla,
) -> Stateful<Div> {
    btn_base(id, enabled, compact, p)
        .border_color(transparent_black())
        .bg(fill)
        .text_color(text)
        .when(enabled, |el| el.hover(move |s| s.bg(fill_hover)))
}

/// [`flat_btn`] in a soft accent tint: the confirm action of a dialog
/// (desktop `.app-dialog .action-btn-accent`).
pub fn soft_accent_btn(id: impl Into<ElementId>, enabled: bool, p: &Palette) -> Stateful<Div> {
    flat_tone(
        id,
        enabled,
        true,
        p,
        (p.accent_dim.opacity(0.8), p.accent_dim),
        p.accent,
    )
}

/// Border of a focused dialog field (desktop `--subtle-border-strong`):
/// faint, never a coloured ring.
pub fn field_focus_border(p: &Palette) -> Hsla {
    if p.dark {
        hsla(0., 0., 1., 0.13)
    } else {
        Hsla::from(rgb(0x241f31)).opacity(0.14)
    }
}

/// Frame of a dialog text field (desktop `.app-dialog .input-shell`): the
/// [`flat_btn`] fill, no border until focused. Put an `Input` with
/// `appearance(false)` inside.
pub fn field_shell(focused: bool, p: &Palette) -> Div {
    h_flex()
        .px(px(10.))
        .gap_2()
        .rounded(px(8.))
        .border_1()
        .border_color(if focused {
            field_focus_border(p)
        } else {
            transparent_black()
        })
        .bg(flat_fill(p).0)
}

/// Rest and hover fills of [`flat_btn`].
pub fn flat_fill(p: &Palette) -> (Hsla, Hsla) {
    if p.dark {
        (hsla(0., 0., 1., 0.045), hsla(0., 0., 1., 0.09))
    } else {
        (p.muted.opacity(0.07), p.muted.opacity(0.13))
    }
}

/// [`flat_btn`] in the destructive tone (Clear API key, Reset app data).
pub fn danger_btn(id: impl Into<ElementId>, enabled: bool, p: &Palette) -> Stateful<Div> {
    let danger = p.danger;
    flat_tone(
        id,
        enabled,
        true,
        p,
        (danger.opacity(0.1), danger.opacity(0.16)),
        danger,
    )
}

/// [`action_btn`] in the accent fill: the one primary action of a dialog or
/// panel (desktop `.action-btn-accent`).
pub fn accent_btn(
    id: impl Into<ElementId>,
    enabled: bool,
    compact: bool,
    p: &Palette,
) -> Stateful<Div> {
    flat_tone(
        id,
        enabled,
        compact,
        p,
        (p.accent, p.accent_hover),
        p.on_accent,
    )
}

/// Square, icon-only [`flat_btn`] with a tooltip.
pub fn flat_icon_btn(
    id: impl Into<ElementId>,
    name: IconName,
    tooltip: &'static str,
    p: &Palette,
) -> Stateful<Div> {
    flat_btn(id, true, true, p)
        .w(px(32.))
        .px_0()
        .child(icon(name, 15., p.text))
        .tooltip(move |window, cx| wrap_tip(tooltip).build(window, cx))
}

/// Track of a segmented picker. The fill is a translucent overlay rather than
/// an opaque palette color, so the track stays visible on every background:
/// popovers (`surface`) as well as settings cards (`card`), where an opaque
/// `card` fill would vanish and leave the options looking like loose text.
pub fn segment_bar(p: &Palette) -> Div {
    h_flex()
        .flex_none()
        .h(px(SEGMENT_H))
        .p_0p5()
        .gap_0p5()
        .rounded(px(8.))
        .bg(p.chip)
}

/// One option of a segmented picker. The selected option gets a clearly
/// raised fill and full-strength text; the others use muted text.
pub fn segment(id: impl Into<ElementId>, selected: bool, p: &Palette) -> Button {
    let (fill, text, muted) = (
        if p.dark { p.border_strong } else { p.surface },
        p.text,
        p.muted,
    );
    button(id)
        .small()
        .ghost()
        .h(px(SEGMENT_H - 4.))
        .selected(selected)
        .map(|b| {
            if selected {
                b.bg(fill).text_color(text).shadow_xs()
            } else {
                b.text_color(muted)
            }
        })
}

/// Widest a tooltip grows before its text wraps onto more lines.
pub const TOOLTIP_MAX_W: f32 = 320.;

/// Text tooltip that wraps long text at [`TOOLTIP_MAX_W`] (stock tooltips
/// keep everything on one line). Short text still sizes to its content.
pub fn wrap_tip(text: impl Into<SharedString>) -> gpui_kit::component::tooltip::Tooltip {
    let text: SharedString = text.into();
    gpui_kit::component::tooltip::Tooltip::element(move |_, _| {
        div().max_w(px(TOOLTIP_MAX_W)).child(text.clone())
    })
}

/// The key bound without a key context to `action`, as a keystroke. When
/// several keys are bound it is the highest-precedence one.
fn global_keystroke(action: &dyn Action, window: &Window) -> Option<Keystroke> {
    let binding =
        window.highest_precedence_binding_for_action_in_context(action, KeyContext::default())?;
    let first = binding.keystrokes().first()?;
    Some(Keystroke {
        modifiers: *first.modifiers(),
        key: first.key().into(),
        key_char: None,
    })
}

/// The shortcut of `action` in the platform's notation (⌘K on macOS, Ctrl+K
/// elsewhere), read from the keymap so a rebinding cannot leave a stale hint.
/// `None` when the action has no global binding. Shows the highest-precedence
/// binding when there are several.
#[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
pub fn shortcut_label(action: &dyn Action, window: &Window) -> Option<SharedString> {
    let stroke = global_keystroke(action, window)?;
    Some(gpui_kit::component::kbd::Kbd::format(&stroke).into())
}

/// A [`wrap_tip`] that also shows the key bound to `action`.
pub fn tip_with_key(
    text: impl Into<SharedString>,
    action: &dyn Action,
    window: &Window,
) -> gpui_kit::component::tooltip::Tooltip {
    let kbd = global_keystroke(action, window).map(gpui_kit::component::kbd::Kbd::new);
    wrap_tip(text).key_binding(kbd)
}

/// A [`wrap_tip`] that shows `stroke` (for example `"tab"`) in the platform's
/// notation, for a key that is not an action of the keymap.
pub fn tip_with_stroke(
    text: impl Into<SharedString>,
    stroke: &'static str,
) -> gpui_kit::component::tooltip::Tooltip {
    let kbd = Keystroke::parse(stroke)
        .ok()
        .map(gpui_kit::component::kbd::Kbd::new);
    wrap_tip(text).key_binding(kbd)
}

/// The mode's colour by its place in the agent's list (see `start::mode_tone`).
#[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
pub fn mode_tone<'a>(ids: impl IntoIterator<Item = &'a str>, current: &str, p: &Palette) -> Hsla {
    super::start::mode_tone(ids, current, p)
}

/// How big a badge's tags are, and where they sit on the control's top edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BadgeSize {
    /// Height of a tag.
    pub height: f32,
    /// Size of its text.
    pub text: f32,
    /// Offset of the tags' top from the control's top edge (negative: above).
    pub top: f32,
}

impl BadgeSize {
    /// The main window's: a 14 px tag for a 12 px toolbar, notched into the
    /// pill's top edge (desktop's `.composer-badge`).
    pub const TOOLBAR: Self = Self {
        height: 14.,
        text: 8.25,
        top: -6.,
    };
}

/// A badge in `color`: the text in it over a 19% tint of it on the card fill,
/// cut out of the control with the palette's `surface`.
pub fn badged_tone(el: impl IntoElement, text: &str, color: Hsla, p: &Palette) -> Div {
    badged(
        el,
        badge_tag(
            text,
            color,
            p.card.blend(color.opacity(0.19)),
            Some(p.surface),
            BadgeSize::TOOLBAR,
        )
        .into_any_element(),
        BadgeSize::TOOLBAR,
    )
}

/// One tag of a badge: `text` in `fg` on `bg`, lowercased. `cut_out` is the
/// colour of the ring that cuts the tag out of the control (the colour the
/// control sits on); none leaves the tag filled over the control's edge. The
/// caller may give it an id and handlers.
pub fn badge_tag(text: &str, fg: Hsla, bg: Hsla, cut_out: Option<Hsla>, size: BadgeSize) -> Div {
    div()
        .h(px(size.height))
        .px(px(4.))
        .rounded(px(4.))
        .bg(bg)
        .when_some(cut_out, |el, surface| {
            el.shadow(vec![BoxShadow {
                color: surface,
                offset: point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(2.),
                inset: false,
            }])
        })
        .text_size(px(size.text))
        .line_height(px(size.height))
        .font_semibold()
        .text_color(fg)
        .text_center()
        .truncate()
        .child(text.to_lowercase())
}

/// `el` with a tag at its top-right edge (build it with [`badge_tag`]).
pub fn badged(el: impl IntoElement, tag: AnyElement, size: BadgeSize) -> Div {
    let row = h_flex()
        .absolute()
        // CSS positions from inside the pill's 1px border; this wrapper is outside it.
        .top(px(size.top))
        .left(px(6.))
        .right(px(6.))
        .h(px(size.height))
        .justify_end()
        // The tag truncates at half the control's width.
        .child(div().min_w_0().max_w(relative(0.5)).child(tag));
    div().relative().flex_none().child(el).child(row)
}

/// Title-bar icon items share the session-details/plan look: a 30px square
/// with 6px corners, `card` background on hover or while its popover is open.
pub const TITLE_BTN: f32 = 30.;
pub const TITLE_ICON: f32 = 15.;

/// Empty title-bar chip (shape, size, hover background); add the icon yourself.
pub fn title_chip(id: impl Into<ElementId>, p: &Palette) -> Stateful<Div> {
    let hover = p.card;
    div()
        .id(id)
        .flex_none()
        .size(px(TITLE_BTN))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .on_mouse_down(MouseButton::Left, claim_press)
}

/// Mouse-down handler for custom title-bar controls: claims the press so the
/// bar does not start a window drag, without stopping propagation (the same
/// convention as GPUI Kit's `Button`).
pub fn claim_press(_: &MouseDownEvent, window: &mut Window, _: &mut App) {
    window.prevent_default();
}

/// Title-bar icon button: [`title_chip`] with a muted icon and a tooltip.
pub fn title_icon_button(
    id: impl Into<ElementId>,
    name: IconName,
    tip: &'static str,
    p: &Palette,
) -> Stateful<Div> {
    title_chip(id, p)
        .child(icon(name, TITLE_ICON, p.muted))
        .tooltip(move |window, cx| wrap_tip(tip).build(window, cx))
}

/// Left-aligned popover menu row: icon + label, the theme's selection colour
/// on hover (a menu is a popover: `card` can equal its own background).
/// (`Button` centres its content in an inner row, so `justify_start` can't.)
pub fn menu_item(
    id: impl Into<ElementId>,
    name: IconName,
    label: impl Into<SharedString>,
    p: &Palette,
) -> Stateful<Div> {
    let hover = p.selected;
    h_flex()
        .id(id)
        .w_full()
        .h(px(28.))
        .px_2()
        .gap_2()
        .rounded(px(6.))
        .cursor_pointer()
        .text_sm()
        .text_color(p.text)
        .hover(move |s| s.bg(hover))
        .child(icon(name, 14., p.muted))
        .child(div().min_w_0().truncate().child(label.into()))
}

/// Widest page column.
pub const PAGE_MAX_W: f32 = 1040.;

/// Header every page shares: the title with an optional badge beside it,
/// an optional description, and the page's actions on the right (refresh,
/// when the page has one, is the last of them).
pub fn page_header(
    title: &'static str,
    badge: Option<AnyElement>,
    desc: Option<&'static str>,
    actions: Vec<AnyElement>,
    p: &Palette,
) -> Div {
    h_flex()
        .gap_3()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().text_2xl().font_bold().child(title))
                        .children(badge),
                )
                .children(desc.map(|d| div().text_sm().text_color(p.muted).child(d))),
        )
        .when(!actions.is_empty(), |el| {
            el.child(h_flex().flex_none().gap_1().children(actions))
        })
}

/// Centered, padded page column holding `content` (header first).
pub fn page(content: impl IntoIterator<Item = AnyElement>) -> Div {
    div().w_full().flex().justify_center().px_8().py_6().child(
        v_flex()
            .w_full()
            .max_w(px(PAGE_MAX_W))
            .gap_5()
            .children(content),
    )
}

/// Desktop `--subtle-border` / `--subtle-surface`.
pub fn subtle(p: &Palette) -> (Hsla, Hsla) {
    if p.dark {
        // Desktop's `#222222` sits halfway between the built-in dark `bg`
        // (#1b1b1b) and `card` (#292929); deriving it keeps other themes'
        // tiles and row hovers in their own hue instead of neutral grey.
        (
            hsla(0., 0., 1., 0.075),
            crate::theme::midpoint(p.bg, p.card),
        )
    } else {
        (
            Hsla::from(rgb(0x241f31)).opacity(0.09),
            hsla(0., 0., 1., 0.72),
        )
    }
}

/// Rounded card grouping [`card_row`]s, as the Settings lists do.
pub fn card(p: &Palette) -> Div {
    v_flex()
        .min_w_0()
        .rounded(px(12.))
        .overflow_hidden()
        .bg(p.card)
}

/// One row of a [`card`], divided from the row above unless `first`.
pub fn card_row(first: bool, p: &Palette) -> Div {
    let (divider, _) = subtle(p);
    h_flex()
        .min_h(px(59.))
        .gap_4()
        .px(px(14.4))
        .py(px(11.5))
        .when(!first, |el| el.border_t_1().border_color(divider))
}

/// Last child of a [`card`] whose rows tint on hover: GPUI clips children to
/// rectangles, so a tinted first or last row would show square corners past
/// the card's rounding. Paints the page background over them.
pub fn card_mask(p: &Palette) -> impl IntoElement {
    corner_mask(12., p.bg)
}

/// Switch with the pointer cursor, like the app's buttons.
pub fn switch(id: impl Into<ElementId>) -> gpui_kit::component::switch::Switch {
    gpui_kit::component::switch::Switch::new(id).cursor_pointer()
}

/// Copy-to-clipboard icon button that turns into a check once copied.
pub fn copy_button(
    id: impl Into<ElementId>,
    value: impl Into<SharedString>,
    tooltip: &'static str,
) -> gpui_kit::component::clipboard::Clipboard {
    gpui_kit::component::clipboard::Clipboard::new(id)
        .value(value)
        .tooltip(tooltip)
        .accessibility_label(tooltip)
        .xsmall()
}

/// QR code of `text`, dark modules on a light tile with a quiet zone so it
/// scans in either theme.
pub fn qr_code(text: &str, max: f32) -> Option<impl IntoElement> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let width = code.width();
    let dark: Vec<bool> = code
        .to_colors()
        .into_iter()
        .map(|c| c == qrcode::Color::Dark)
        .collect();
    // Four-module quiet zone on every side.
    let cells = width + 8;
    let module = (max / cells as f32).floor().max(1.);
    let side = module * cells as f32;
    Some(
        canvas(
            |_, _, _| {},
            move |b, _, window, _| {
                window.paint_quad(fill(
                    Bounds::new(b.origin, size(px(side), px(side))),
                    hsla(0., 0., 1., 1.),
                ));
                let ink = hsla(0., 0., 0.08, 1.);
                for y in 0..width {
                    for x in 0..width {
                        if dark[y * width + x] {
                            let o = point(
                                b.origin.x + px((x + 4) as f32 * module),
                                b.origin.y + px((y + 4) as f32 * module),
                            );
                            window.paint_quad(fill(
                                Bounds::new(o, size(px(module), px(module))),
                                ink,
                            ));
                        }
                    }
                }
            },
        )
        .flex_none()
        .size(px(side)),
    )
}

/// Title and help of a page section, with the section's action on the right.
pub fn section_header(
    title: &'static str,
    desc: &'static str,
    action: Option<AnyElement>,
    p: &Palette,
) -> Div {
    h_flex()
        .gap_3()
        .items_end()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().font_semibold().child(title))
                .child(div().text_sm().text_color(p.muted).child(desc)),
        )
        .children(action)
}

/// Square tile holding an object's icon, as Settings' state panels.
pub fn icon_tile(name: IconName, active: bool, p: &Palette) -> Div {
    let (_, surface) = subtle(p);
    div()
        .relative()
        .flex_none()
        .size(px(34.4))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .bg(if active { p.accent_dim } else { surface })
        .child(icon(name, 17., if active { p.accent } else { p.muted }))
}

pub fn dot(color: Hsla) -> Div {
    div().flex_none().size(px(8.)).rounded_full().bg(color)
}

pub fn status_color(state: &ConnState, p: &Palette) -> Hsla {
    match state {
        ConnState::Connected => p.success,
        ConnState::Connecting => p.running,
        ConnState::Failed(_) => p.warn,
        ConnState::Idle => p.status_muted,
    }
}

pub fn state_label(state: &ConnState) -> &'static str {
    match state {
        ConnState::Connected => "connected",
        ConnState::Connecting => "connecting",
        ConnState::Failed(_) => "failed",
        ConnState::Idle => "stopped",
    }
}

pub fn chip(text: impl Into<SharedString>, p: &Palette) -> Div {
    div()
        .flex_none()
        .px_2()
        .py(px(1.))
        .rounded_full()
        .bg(p.chip)
        .text_xs()
        .text_color(p.muted)
        .child(text.into())
}

pub fn tone_chip(text: impl Into<SharedString>, fg: Hsla) -> Div {
    div()
        .flex_none()
        .px_2()
        .py(px(1.))
        .rounded_full()
        .bg(fg.opacity(0.14))
        .text_xs()
        .font_medium()
        .text_color(fg)
        .child(text.into())
}

/// [`chip`] with a leading icon: neutral metadata (fork, branch, machine).
pub fn icon_chip(name: IconName, text: impl Into<SharedString>, p: &Palette) -> Div {
    h_flex()
        .flex_none()
        .gap_1()
        .px_2()
        .py(px(1.))
        .rounded_full()
        .bg(p.chip)
        .text_xs()
        .text_color(p.muted)
        .child(icon(name, 11., p.muted))
        .child(text.into())
}

/// [`tone_chip`] with a leading icon: state that needs a colour (warnings, resolved).
pub fn icon_tone_chip(name: IconName, text: impl Into<SharedString>, fg: Hsla) -> Div {
    h_flex()
        .flex_none()
        .gap_1()
        .px_2()
        .py(px(1.))
        .rounded_full()
        .bg(fg.opacity(0.14))
        .text_xs()
        .font_medium()
        .text_color(fg)
        .child(icon(name, 11., fg))
        .child(text.into())
}

/// Icon button for list rows and cards: square, transparent at rest, and a
/// fill on hover that stays visible over a row's own hover tint (the kit's
/// ghost hover blends into `card`).
pub fn row_icon_button(
    id: impl Into<ElementId>,
    name: IconName,
    tooltip: impl Into<SharedString>,
    p: &Palette,
    cx: &App,
) -> Button {
    let (hover, active) = if p.dark {
        (hsla(0., 0., 1., 0.1), hsla(0., 0., 1., 0.14))
    } else {
        (p.muted.opacity(0.14), p.muted.opacity(0.2))
    };
    button(id)
        .custom(
            gpui_kit::component::button::ButtonCustomVariant::new(cx)
                .foreground(p.text)
                .hover(hover)
                .active(active),
        )
        .small()
        .px_0()
        .icon(name)
        .tooltip(tooltip)
}

pub fn icon_button(
    id: impl Into<ElementId>,
    name: IconName,
    tooltip: impl Into<SharedString>,
) -> Button {
    button(id).ghost().small().icon(name).tooltip(tooltip)
}

/// Size of a collapsed-sidebar tile.
pub const RAIL_TILE: Pixels = px(38.);

/// Flat ghost tile for the collapsed sidebar's navigation, sessions, and expand
/// control (mirrors the desktop app's `.app-icon-link`): no border, faint fill
/// on hover, and the same faint fill when it is the current page.
pub fn rail_button(id: impl Into<ElementId>, active: bool, p: &Palette) -> Stateful<Div> {
    let hover = p.card_hover;
    h_flex()
        .id(id)
        .relative()
        .flex_none()
        .size(RAIL_TILE)
        .justify_center()
        .rounded(px(8.))
        .cursor_pointer()
        .when(active, |el| el.bg(p.card))
        .hover(move |el| el.bg(hover))
}

/// `N label` pill with the number emphasized.
pub fn count_pill(
    count: impl Into<SharedString>,
    label: impl Into<SharedString>,
    p: &Palette,
) -> Div {
    h_flex()
        .gap_1()
        .px_3()
        .py_1()
        .rounded_full()
        .bg(p.card)
        .text_sm()
        .text_color(p.muted)
        .child(div().font_semibold().text_color(p.text).child(count.into()))
        .child(label.into())
}

pub fn stat_card(
    label: &'static str,
    value: impl Into<SharedString>,
    value_color: Hsla,
    p: &Palette,
) -> Div {
    div()
        .flex_1()
        .min_w(px(120.))
        .px_4()
        .py_3()
        .rounded(px(12.))
        .bg(p.surface)
        .border_1()
        .border_color(p.border)
        .child(div().text_xs().text_color(p.dim).child(label))
        .child(
            div()
                .mt_1()
                .text_lg()
                .font_semibold()
                .text_color(value_color)
                .child(value.into()),
        )
}

pub fn divider(p: &Palette) -> Div {
    div().h(px(1.)).w_full().bg(p.border)
}

// Idle BootMark geometry from the desktop app's lib/design/boot-mark.ts.
// First seven cells form the Q; the remaining cells are its surrounding peers.
const BOOT_MARK_CELLS: [(i8, i8); 18] = [
    (1, -1),
    (0, -1),
    (-1, 0),
    (-1, 1),
    (0, 1),
    (1, 0),
    (1, 1),
    (0, 2),
    (-1, 2),
    (-2, 2),
    (-2, 1),
    (-2, 0),
    (-1, -1),
    (0, -2),
    (1, -2),
    (2, -2),
    (2, -1),
    (2, 0),
];
const BOOT_MARK_LOGO_HEX: [(f32, f32); 6] = [
    (27.2, -15.7),
    (27.2, 15.7),
    (0., 31.4),
    (-27.2, 15.7),
    (-27.2, -15.7),
    (0., -31.4),
];
const BOOT_MARK_PEER_HEX: [(f32, f32); 6] = [
    (17.3, -10.),
    (17.3, 10.),
    (0., 20.),
    (-17.3, 10.),
    (-17.3, -10.),
    (0., -20.),
];

fn boot_mark_nodes() -> [Point<f32>; 18] {
    let row = 64. * 3_f32.sqrt() / 2.;
    BOOT_MARK_CELLS
        .map(|(q, r)| point(64. * (f32::from(q) + f32::from(r) / 2.), row * f32::from(r)))
}

/// The desktop Start page's static mark, with themed peers and an accent-colored Q.
/// One paint layer of the boot mark, served as an SVG asset under `path`.
/// GPUI draws SVGs as alpha masks in a single tint, so the multi-colour mark
/// is a stack of masks painted bottom-up (the order of `ALL`).
#[derive(Clone, Copy)]
enum BootMarkLayer {
    Links,
    Peers,
    PeerEdges,
    Logo,
}

impl BootMarkLayer {
    const ALL: [Self; 4] = [Self::Links, Self::Peers, Self::PeerEdges, Self::Logo];

    fn path(self) -> &'static str {
        match self {
            Self::Links => "brand/boot-mark-links.svg",
            Self::Peers => "brand/boot-mark-peers.svg",
            Self::PeerEdges => "brand/boot-mark-peer-edges.svg",
            Self::Logo => "brand/boot-mark-logo.svg",
        }
    }

    fn tint(self, p: &Palette) -> Hsla {
        match self {
            Self::Links => p.border_strong.opacity(0.6),
            Self::Peers => p.card_hover,
            Self::PeerEdges => p.border_strong,
            Self::Logo => p.accent,
        }
    }

    /// The layer's SVG: same viewbox, rotation and geometry as BootMark.svelte.
    fn svg(self) -> String {
        use std::fmt::Write as _;
        let nodes = boot_mark_nodes();
        let polygon = |out: &mut String, center: Point<f32>, hex: &[(f32, f32); 6]| {
            let points: Vec<String> = hex
                .iter()
                .map(|(x, y)| format!("{:.3},{:.3}", center.x + x, center.y + y))
                .collect();
            let _ = write!(out, r#"<polygon points="{}"/>"#, points.join(" "));
        };
        let mut body = String::new();
        let attrs = match self {
            Self::Links => {
                for (i, a) in nodes.iter().enumerate() {
                    for b in nodes[i + 1..]
                        .iter()
                        .filter(|b| (a.x - b.x).hypot(a.y - b.y) < 65.)
                    {
                        let _ = write!(
                            body,
                            r#"<line x1="{:.3}" y1="{:.3}" x2="{:.3}" y2="{:.3}"/>"#,
                            a.x, a.y, b.x, b.y
                        );
                    }
                }
                r##"fill="none" stroke="#fff" stroke-width="1.4""##
            }
            Self::Peers | Self::PeerEdges => {
                for center in &nodes[7..] {
                    polygon(&mut body, *center, &BOOT_MARK_PEER_HEX);
                }
                if matches!(self, Self::Peers) {
                    r##"fill="#fff""##
                } else {
                    r##"fill="none" stroke="#fff" stroke-width="1""##
                }
            }
            Self::Logo => {
                for center in &nodes[..7] {
                    polygon(&mut body, *center, &BOOT_MARK_LOGO_HEX);
                }
                r##"fill="#fff""##
            }
        };
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="-164 -164 328 328"><g transform="rotate(14.90684242)" {attrs}>{body}</g></svg>"#
        )
    }
}

/// Bytes of a boot-mark layer asset, for the app's `AssetSource`.
pub fn boot_mark_asset(path: &str) -> Option<&'static [u8]> {
    static LAYERS: std::sync::LazyLock<[(&'static str, String); 4]> =
        std::sync::LazyLock::new(|| BootMarkLayer::ALL.map(|layer| (layer.path(), layer.svg())));
    LAYERS
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, svg)| svg.as_bytes())
}

/// The desktop Start page's static mark, with themed peers and an accent-colored Q.
/// Rasterised from SVG layers (like the mobile app) rather than painted as
/// canvas paths, so edges are antialiased and the hairline links stay visible.
pub fn boot_mark(size: f32, p: &Palette) -> impl IntoElement {
    div()
        .flex_none()
        .relative()
        .size(px(size))
        .children(BootMarkLayer::ALL.map(|layer| {
            svg()
                .path(layer.path())
                .absolute()
                .top_0()
                .left_0()
                .size(px(size))
                .text_color(layer.tint(p))
        }))
}

// ---------------------------------------------------------------------------
// Session identicon: port of the Svelte app's hex identicon (same seed -> same glyph).

const IDENTICON_PALETTE: [u32; 8] = [
    0x3B82F6, 0x10B981, 0xF59E0B, 0xEF4444, 0x8B5CF6, 0xEC4899, 0x06B6D4, 0xF97316,
];
const IDENTICON_SIZE: f32 = 40.;

fn identicon_cells(seed: &str) -> Vec<(f32, f32, Hsla)> {
    // FNV-1a over UTF-16 code units (JS `charCodeAt`), then a murmur-style mix.
    let mut h: u32 = 2166136261;
    for unit in seed.encode_utf16() {
        h ^= unit as u32;
        h = h.wrapping_mul(16777619);
    }
    let mut bytes = [0u8; 16];
    for b in bytes.iter_mut() {
        h = (h ^ (h >> 16)).wrapping_mul(0x85eb_ca6b);
        h = (h ^ (h >> 13)).wrapping_mul(0xc2b2_ae35);
        *b = ((h >> 16) & 0xff) as u8;
    }
    let color_count = (bytes[0] % 2) as usize + 1;
    let colors: Vec<Hsla> = (0..color_count)
        .map(|i| {
            rgb(IDENTICON_PALETTE[(bytes[1 + i] as usize + i * 3) % IDENTICON_PALETTE.len()]).into()
        })
        .collect();
    let (c, hex_r) = (IDENTICON_SIZE / 2., IDENTICON_SIZE * 0.082);
    let spacing = hex_r * 2.05;
    let mut positions = vec![(c, c)];
    for ring in 1..=2 {
        let count = 6 * ring;
        for i in 0..count {
            let ang = std::f32::consts::TAU * i as f32 / count as f32;
            positions.push((
                c + spacing * ring as f32 * ang.cos(),
                c + spacing * ring as f32 * ang.sin(),
            ));
        }
    }
    let mut cells: Vec<(f32, f32, Hsla)> = Vec::new();
    let mut put = |x: f32, y: f32, color: Hsla| {
        let key = |a: f32| (a * 10.).round() as i32;
        match cells
            .iter_mut()
            .find(|(cx, cy, _)| key(*cx) == key(x) && key(*cy) == key(y))
        {
            Some(cell) => cell.2 = color,
            None => cells.push((x, y, color)),
        }
    };
    for (i, (x, y)) in positions.into_iter().enumerate() {
        let b = bytes[i % bytes.len()] as usize;
        let thresh = 127 + (i / 6) * 12;
        if i == 0 || b > thresh {
            let color = colors[b % colors.len()];
            put(x, y, color);
            if (x - c).abs() > 0.5 {
                put(2. * c - x, y, color);
            }
        }
    }
    cells
}

/// Deterministic hexagon glyph for a session id.
/// Deterministic hexagon glyph for a session id.
/// Bring a virtual list from items `old` to `new` (one signature per item:
/// a changed signature may mean a changed height). Unchanged items keep
/// their measurements, so the list keeps its scroll position.
pub fn sync_list_state(state: &ListState, old: &[u64], new: &[u64]) {
    if state.item_count() != old.len() {
        state.reset(new.len());
    } else if old.len() == new.len() {
        for (ix, (a, b)) in old.iter().zip(new).enumerate() {
            if a != b {
                state.remeasure_items(ix..ix + 1);
            }
        }
    } else {
        let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        let max_suffix = old.len().min(new.len()) - prefix;
        let suffix = old
            .iter()
            .rev()
            .zip(new.iter().rev())
            .take(max_suffix)
            .take_while(|(a, b)| a == b)
            .count();
        state.splice(prefix..old.len() - suffix, new.len() - prefix - suffix);
    }
}

pub fn identicon(seed: &str, size: f32) -> impl IntoElement {
    let paths = identicon_paths(seed, size);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            for (corners, color) in paths.iter() {
                if let Some(path) = hexagon_at(corners, bounds.origin) {
                    window.paint_path(path, *color);
                }
            }
        },
    )
    .flex_none()
    .size(px(size))
}

/// The six corners of one hexagon, relative to the identicon's origin.
type Hexagon = [Point<Pixels>; 6];

/// The hexagons of one identicon at the origin, with their colors.
type IdenticonPaths = std::rc::Rc<Vec<(Hexagon, Hsla)>>;

/// Identicons kept before the cache starts over (a few per session).
const IDENTICON_CACHE_MAX: usize = 4096;

thread_local! {
    /// (seed, size in 1/100 px) → hexagons. Hashing the seed and placing the
    /// cells on every paint was a measurable part of a long session list's
    /// frame. A built `Path` can no longer be moved (gpui-pre 0.3.14), so
    /// the corners are kept and the path is built where it is painted.
    static IDENTICONS: std::cell::RefCell<std::collections::HashMap<(String, u32), IdenticonPaths>> =
        std::cell::RefCell::default();
}

fn identicon_paths(seed: &str, size: f32) -> IdenticonPaths {
    let key = (seed.to_string(), (size * 100.).round() as u32);
    IDENTICONS.with(|cache| {
        if let Some(paths) = cache.borrow().get(&key) {
            return paths.clone();
        }
        let paths = std::rc::Rc::new(build_identicon(seed, size));
        let mut cache = cache.borrow_mut();
        if cache.len() >= IDENTICON_CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, paths.clone());
        paths
    })
}

fn build_identicon(seed: &str, size: f32) -> Vec<(Hexagon, Hsla)> {
    let scale = size / IDENTICON_SIZE;
    let hex_r = IDENTICON_SIZE * 0.082 * scale;
    identicon_cells(seed)
        .into_iter()
        .map(|(x, y, color)| {
            let (cx, cy) = (x * scale, y * scale);
            let corners = std::array::from_fn(|i| {
                let ang = std::f32::consts::FRAC_PI_3 * i as f32 - std::f32::consts::FRAC_PI_6;
                point(px(cx + hex_r * ang.cos()), px(cy + hex_r * ang.sin()))
            });
            (corners, color)
        })
        .collect()
}

/// The hexagon with `corners`, moved by `by`, as a filled path.
fn hexagon_at(corners: &Hexagon, by: Point<Pixels>) -> Option<Path> {
    let mut path = PathBuilder::fill();
    path.move_to(corners[0] + by);
    for &corner in &corners[1..] {
        path.line_to(corner + by);
    }
    path.close();
    path.build().ok()
}

/// The colour of a context ring `fraction` full: warning above 70%, danger
/// above 90%, the accent otherwise.
pub fn context_colour(fraction: f32, p: &Palette) -> Hsla {
    if fraction > 0.9 {
        p.danger
    } else if fraction > 0.7 {
        p.warn
    } else {
        p.accent
    }
}

/// What the context ring's tooltip says: the tokens used, the size and the
/// percentage.
pub fn context_tip(used: u64, size: u64) -> String {
    format!(
        "Context: {} of {} tokens ({:.0}%)",
        short_num(used),
        short_num(size),
        used as f64 * 100. / size as f64
    )
}

/// The context ring alone, 18 px, in its threshold colour; `None` when the
/// size is unknown (0).
pub fn context_ring(used: u64, size: u64, p: &Palette) -> Option<impl IntoElement> {
    (size > 0).then(|| {
        let fraction = used as f32 / size as f32;
        ring(fraction, 18., context_colour(fraction, p), p.border)
    })
}

/// The context ring with its tooltip, for a surface that has no details panel
/// to hang them on; `None` when the size is unknown (0).
#[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
pub fn context_meter(used: u64, size: u64, p: &Palette) -> Option<Stateful<Div>> {
    let ring = context_ring(used, size, p)?;
    let tip = context_tip(used, size);
    Some(
        div()
            .id("context-meter")
            .flex_none()
            .child(ring)
            .tooltip(move |window, cx| wrap_tip(tip.clone()).build(window, cx)),
    )
}

/// Thin progress ring (context usage), like the desktop header dial.
pub fn ring(fraction: f32, size: f32, color: Hsla, track: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let c = bounds.center();
            let r = f32::from(bounds.size.width) / 2. - 1.5;
            let radii = point(px(r), px(r));
            let mut path = PathBuilder::stroke(px(2.));
            path.move_to(point(c.x + px(r), c.y));
            path.arc_to(radii, px(0.), true, true, point(c.x - px(r), c.y));
            path.arc_to(radii, px(0.), true, true, point(c.x + px(r), c.y));
            if let Ok(path) = path.build() {
                window.paint_path(path, track);
            }
            let frac = fraction.clamp(0., 0.999);
            if frac > 0.01 {
                let ang = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * frac;
                let mut path = PathBuilder::stroke(px(2.));
                path.move_to(point(c.x, c.y - px(r)));
                path.arc_to(
                    radii,
                    px(0.),
                    frac > 0.5,
                    true,
                    point(c.x + px(r * ang.cos()), c.y + px(r * ang.sin())),
                );
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .flex_none()
    .size(px(size))
}

/// Clockwise stroked arc from angle `a0` to `a1` (radians, 0 = 3 o'clock).
fn paint_arc(
    window: &mut Window,
    c: Point<Pixels>,
    r: f32,
    a0: f32,
    a1: f32,
    width: f32,
    color: Hsla,
) {
    let at = |a: f32| point(c.x + px(r * a.cos()), c.y + px(r * a.sin()));
    let radii = point(px(r), px(r));
    let mut path = PathBuilder::stroke(px(width));
    path.move_to(at(a0));
    // Split anything past a half turn so the small-arc flag stays unambiguous.
    if a1 - a0 > std::f32::consts::PI {
        let mid = (a0 + a1) / 2.;
        path.arc_to(radii, px(0.), false, true, at(mid));
    }
    path.arc_to(radii, px(0.), false, true, at(a1));
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

/// Paints the parent's background over the four corners outside a rounded
/// rect of `radius`. GPUI clips children to rectangles only, so tinted child
/// rows would otherwise poke square corners past a card's rounded border.
/// Add as the card's last child; the card's own border still paints on top.
pub fn corner_mask(radius: f32, color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |b, _, window, _| {
            // A see-through colour (a card on a translucent material) cannot
            // cover anything: it would only square the card's own corners.
            if color.a < 1. {
                return;
            }
            let (x0, y0, x1, y1) = (b.origin.x, b.origin.y, b.right(), b.bottom());
            let r = px(radius);
            // Cubic handle length for a quarter circle.
            let c = px(radius * 0.5523);
            let corners = [
                // (corner, edge point A, curve end B, control near A, control near B)
                (
                    point(x0, y0),
                    point(x0 + r, y0),
                    point(x0, y0 + r),
                    point(x0 + r - c, y0),
                    point(x0, y0 + r - c),
                ),
                (
                    point(x1, y0),
                    point(x1, y0 + r),
                    point(x1 - r, y0),
                    point(x1, y0 + r - c),
                    point(x1 - r + c, y0),
                ),
                (
                    point(x1, y1),
                    point(x1 - r, y1),
                    point(x1, y1 - r),
                    point(x1 - r + c, y1),
                    point(x1, y1 - r + c),
                ),
                (
                    point(x0, y1),
                    point(x0, y1 - r),
                    point(x0 + r, y1),
                    point(x0, y1 - r + c),
                    point(x0 + r - c, y1),
                ),
            ];
            for (corner, a, b, ca, cb) in corners {
                let mut path = PathBuilder::fill();
                path.move_to(corner);
                path.line_to(a);
                path.cubic_bezier_to(b, ca, cb);
                path.close();
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .absolute()
    .inset_0()
}

/// Segmented ring (usage breakdown): `(share 0..=1, color)` clockwise from 12
/// o'clock over a track; a dashed track marks proportions of reported tokens only.
pub fn donut(
    segments: Vec<(f32, Hsla)>,
    size: f32,
    stroke: f32,
    track: Hsla,
    dashed: bool,
) -> impl IntoElement {
    use std::f32::consts::{FRAC_PI_2, TAU};
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let c = bounds.center();
            let r = f32::from(bounds.size.width) / 2. - stroke / 2.;
            if dashed {
                let n = 36;
                let step = TAU / n as f32;
                for i in 0..n {
                    let a = i as f32 * step;
                    paint_arc(window, c, r, a, a + step * 0.5, stroke, track);
                }
            } else {
                paint_arc(window, c, r, 0., TAU, stroke, track);
            }
            let mut start = -FRAC_PI_2;
            for (share, color) in &segments {
                let sweep = share.clamp(0., 1.) * TAU;
                if sweep > 0.001 {
                    paint_arc(window, c, r, start, start + sweep, stroke, *color);
                }
                start += sweep;
            }
        },
    )
    .flex_none()
    .size(px(size))
}

struct SecondClock {
    _ticker: Task<()>,
}

impl SecondClock {
    fn new(cx: &mut Context<Self>) -> Self {
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self { _ticker: ticker }
    }
}

#[derive(IntoElement)]
struct LiveDuration {
    id: ElementId,
    settled_ms: f64,
    started_at: Option<f64>,
}

impl RenderOnce for LiveDuration {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Keyed state ticks once a second only while the stretch is live, and
        // drops its timer when the label leaves the tree.
        if self.started_at.is_some() {
            let clock = window.use_keyed_state(self.id, cx, |_, cx| SecondClock::new(cx));
            let _ = clock.read(cx);
        }
        let live = self
            .started_at
            .map_or(0., |s| (crate::util::now_ms() - s).max(0.));
        div().child(crate::usage::format_duration(self.settled_ms + live))
    }
}

/// `17m 15s` of active work, counting up each second while `started_at` is set.
pub fn live_duration(
    id: impl Into<ElementId>,
    settled_ms: f64,
    started_at: Option<f64>,
) -> impl IntoElement {
    LiveDuration {
        id: id.into(),
        settled_ms,
        started_at,
    }
}

/// `12.3k` style token counts.
pub fn short_num(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{:.1}k", n as f64 / 1000.),
        _ => format!("{:.2}M", n as f64 / 1_000_000.),
    }
}

/// Provider logo, falling back to the generic bot icon.
pub fn provider_icon(provider: &str, size: f32, color: Hsla) -> AnyElement {
    match crate::provider_icon_path(provider) {
        Some(path) => svg()
            .path(path)
            .flex_none()
            .size(px(size))
            .text_color(color)
            .into_any_element(),
        None => icon(IconName::Bot, size, color).into_any_element(),
    }
}

/// Icon for a model input modality (`text`, `image`, `pdf`, `audio`, `video`).
pub fn modality_icon(modality: &str) -> IconName {
    match modality {
        "text" => IconName::Type,
        "image" => IconName::Image,
        "pdf" => IconName::FileText,
        "audio" => IconName::AudioLines,
        "video" => IconName::Video,
        _ => IconName::Paperclip,
    }
}

struct SpinnerClock {
    phase: f32,
    _ticker: Task<()>,
}

impl SpinnerClock {
    fn new(cx: &mut Context<Self>) -> Self {
        let started = cx.background_executor().now();
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(33))
                    .await;
                if this
                    .update(cx, |clock, cx| {
                        clock.phase = spinner_phase(cx.background_executor().now() - started);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            phase: 0.,
            _ticker: ticker,
        }
    }
}

fn spinner_phase(elapsed: std::time::Duration) -> f32 {
    // Reduce before converting to f32 so long-running sessions keep smooth motion.
    (elapsed.as_nanos() % 900_000_000) as f32 / 900_000_000.
}

#[derive(IntoElement)]
struct ActivitySpinner {
    id: ElementId,
    size: f32,
    color: Option<Hsla>,
}

impl RenderOnce for ActivitySpinner {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Activity indicators must remain distinguishable from idle icons even
        // under reduced motion. Do not change the app's decorative-motion policy.
        // Keyed state drops the timer when the spinner leaves the rendered tree.
        let clock = window.use_keyed_state(self.id, cx, |_, cx| SpinnerClock::new(cx));
        svg()
            .path("icons/loader-circle.svg")
            .flex_none()
            .size(px(self.size))
            .text_color(self.color.unwrap_or_else(|| window.text_style().color))
            .with_transformation(Transformation::rotate(percentage(clock.read(cx).phase)))
    }
}

/// Rotating activity indicator, including when the system reduces decorative motion.
pub fn spinner(id: impl Into<ElementId>, size: f32, color: Hsla) -> impl IntoElement {
    ActivitySpinner {
        id: id.into(),
        size,
        color: Some(color),
    }
}

/// Loading content for a button: disable repeat actions and inherit its foreground.
pub fn loading_button(
    button: Button,
    id: impl Into<ElementId>,
    loading: bool,
    size: f32,
) -> Button {
    button.when(loading, |button| {
        button.inactive(true).child(ActivitySpinner {
            id: id.into(),
            size,
            color: None,
        })
    })
}

pub fn loading_icon_button(
    id: &'static str,
    name: IconName,
    tooltip: &'static str,
    loading: bool,
    size: f32,
) -> Button {
    let button = button(id)
        .ghost()
        .small()
        .when(size < 24., |button| button.xsmall())
        .size(px(size))
        .px_0()
        .when(!loading, |button| button.icon(name))
        .tooltip(tooltip);
    loading_button(
        button,
        (ElementId::from(id), "spinner"),
        loading,
        size * 0.75,
    )
}

#[cfg(test)]
mod tests {
    use super::{boot_mark_nodes, context_colour, context_meter, context_tip, spinner_phase};

    // The one place the context ring's thresholds live, for both windows.
    #[gpui_kit::test]
    fn the_context_ring_turns_warning_above_70_and_danger_above_90(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let p = cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
            *crate::theme::palette(cx)
        });
        assert_eq!(context_colour(0.69, &p), p.accent);
        assert_eq!(context_colour(0.71, &p), p.warn);
        assert_eq!(context_colour(0.91, &p), p.danger);
        assert_ne!(p.accent, p.warn);
        assert_ne!(p.warn, p.danger);
        // An unknown size draws nothing.
        assert!(context_meter(10, 0, &p).is_none());
        assert!(context_meter(10, 100, &p).is_some());
    }

    #[test]
    fn the_context_tip_gives_tokens_size_and_percentage() {
        let tip = context_tip(50_000, 100_000);
        assert!(tip.starts_with("Context: "), "{tip}");
        assert!(tip.ends_with("(50%)"), "{tip}");
    }

    #[test]
    fn spinner_advances_and_wraps_without_losing_long_running_precision() {
        use std::time::Duration;
        for (ms, expected) in [
            (0, 0.),
            (225, 0.25),
            (450, 0.5),
            (675, 0.75),
            (900, 0.),
            (1125, 0.25),
        ] {
            assert!((spinner_phase(Duration::from_millis(ms)) - expected).abs() < 1e-6);
        }
        // A late frame samples the current phase instead of replaying missed ticks.
        let long = Duration::from_secs(900 * 1_000_000);
        assert_eq!(spinner_phase(long + Duration::from_millis(225)), 0.25);
        assert!(spinner_phase(Duration::from_millis(33)) > 0.);
    }

    #[test]
    fn boot_mark_matches_desktop_topology() {
        let nodes = boot_mark_nodes();
        // The six-hex ring has an empty counter, followed by the Q's tail.
        assert!(
            nodes[..6]
                .iter()
                .all(|p| (p.x.hypot(p.y) - 64.).abs() < 0.001)
        );
        assert_eq!(nodes[6].x, 96.);
        assert!((nodes[6].y - 55.425_625).abs() < 0.001);
        let degrees: Vec<_> = nodes
            .iter()
            .enumerate()
            .map(|(i, a)| {
                nodes
                    .iter()
                    .enumerate()
                    .filter(|(j, b)| i != *j && (a.x - b.x).hypot(a.y - b.y) < 65.)
                    .count()
            })
            .collect();
        // Same seven logo nodes, eleven peers, and 36 links as BootMark.svelte.
        assert_eq!(
            degrees,
            [5, 5, 5, 5, 5, 5, 4, 3, 4, 3, 4, 3, 4, 3, 4, 3, 4, 3]
        );
        assert_eq!(degrees.iter().sum::<usize>() / 2, 36);
    }

    #[test]
    fn boot_mark_layers_are_served_as_svg_assets() {
        let text = |path: &str| {
            std::str::from_utf8(super::boot_mark_asset(path).expect(path))
                .unwrap()
                .to_owned()
        };
        let count = |svg: &str, tag: &str| svg.matches(tag).count();
        assert_eq!(count(&text("brand/boot-mark-links.svg"), "<line "), 36);
        assert_eq!(count(&text("brand/boot-mark-peers.svg"), "<polygon "), 11);
        assert_eq!(
            count(&text("brand/boot-mark-peer-edges.svg"), "<polygon "),
            11
        );
        assert_eq!(count(&text("brand/boot-mark-logo.svg"), "<polygon "), 7);
        assert!(super::boot_mark_asset("brand/boot-mark-nope.svg").is_none());
    }

    #[test]
    fn subtle_surface_keeps_the_desktop_grey_and_follows_other_themes() {
        let dark = crate::theme::Palette::dark();
        let (_, surface) = super::subtle(&dark);
        let expected: gpui_kit::Hsla = gpui_kit::rgb(0x222222).into();
        let (a, b) = (
            gpui_kit::Rgba::from(surface),
            gpui_kit::Rgba::from(expected),
        );
        assert!(
            (a.r - b.r).abs() < 0.003 && (a.g - b.g).abs() < 0.003 && (a.b - b.b).abs() < 0.003
        );
        // A tinted theme's surface is tinted, not the built-in grey.
        let teal = crate::theme::Palette {
            bg: gpui_kit::rgb(0x002b36).into(),
            card: gpui_kit::rgb(0x073642).into(),
            ..dark
        };
        let (_, surface) = super::subtle(&teal);
        let s = gpui_kit::Rgba::from(surface);
        assert!(s.b > s.r + 0.1, "{s:?}");
    }
}

/// The colours a text field's editor paints with in `p`: typed text, the
/// placeholder in `muted`, the selection and the caret in the accent. The
/// kit's own come from the app theme, which is wrong on a palette of its own
/// (the quick panel's is dark whatever the app theme).
pub(crate) fn editor_style(p: &Palette, _cx: &App) -> base::input::InputEditorStyle {
    base::input::InputEditorStyle {
        foreground: p.text,
        muted_foreground: p.muted,
        background: transparent_black(),
        border: p.border,
        selection: p.accent.opacity(0.32),
        caret: p.accent,
        highlight_styles: crate::theme::syntax(p),
        ..Default::default()
    }
}

/// Place before the input of `state`, in the same parent: paints that input
/// with [`editor_style`] of `p`.
///
/// gpui-kit 0.11 has no setting for these colours: `Input::render` writes the
/// app theme's into the state on every frame. Both renders run before any
/// element is prepainted, and the input reads the style while prepainting and
/// painting, so a canvas before it that writes ours while prepainting is the
/// last word. Remove once the kit takes the colours (task 7.29).
pub(crate) fn input_colours<M: base::input::InputModeKind>(
    state: &Entity<base::input::InputBaseState<M>>,
    p: &Palette,
) -> impl IntoElement + use<M> {
    let (state, p) = (state.clone(), *p);
    canvas(
        move |_, _, cx| {
            let style = editor_style(&p, cx);
            state.update(cx, |s, _| s.set_editor_style(style));
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_0()
}
