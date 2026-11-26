//! Start page: launch a session on the connected agent, pick up recent work.
//!
//! Composer: workspace path, prompt, attachments (`+`), model picker (modal,
//! grouped by provider with recents, reasoning effort), Options (mode, profile,
//! agent).

use gpui_kit::assets::IconName;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::{Enter, IndentInline, OutdentInline, Textarea};
use gpui_kit::component::{Selectable as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::kit::ButtonExt as _;
use super::model_picker::Target;
use super::{Page, QmtApp, kit};
use crate::store::launch::WorktreeMode;
use crate::store::{Attachment, ConnState, DEFAULT_PROFILE, LAUNCH_MODES, Model, SessionSummary};
use crate::theme::{Palette, palette};
use crate::util::{now_ms, short_time, workspace_name};

/// Popover key of the Start composer's Options (see [`kit::Dismissed`]).
const START_OPTIONS: &str = "start-options";

/// Attachment limits, same as the desktop composer.
const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;
const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;
const MAX_IMAGES: usize = 8;

fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or_default().to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "txt" | "log" | "rs" | "ts" | "js" | "py" | "go" | "toml" | "yaml" | "yml" | "nix"
        | "sh" | "c" | "h" | "cpp" | "java" | "kt" | "swift" | "svelte" | "css" | "sql" | "xml" => {
            "text/plain"
        }
        _ => "application/octet-stream",
    }
}

fn attachment_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "att-{}-{}",
        now_ms() as u64,
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

fn read_attachments(paths: Vec<std::path::PathBuf>) -> (Vec<Attachment>, Vec<String>) {
    let mut ok = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                errors.push(format!(
                    "{name} is a folder; attach the files inside it instead."
                ));
                continue;
            }
            Ok(meta) if meta.len() > MAX_ATTACHMENT_BYTES => {
                errors.push(format!("{name}: attachments must be 10 MiB or smaller."));
                continue;
            }
            Err(err) => {
                errors.push(format!("{name}: {err}"));
                continue;
            }
            _ => {}
        }
        match std::fs::read(&path) {
            Ok(bytes) => ok.push(Attachment::from_bytes(
                attachment_id(),
                name.clone(),
                mime_for(&name).to_string(),
                &bytes,
            )),
            Err(err) => errors.push(format!("{name}: {err}")),
        }
    }
    (ok, errors)
}

/// Keep the candidates that fit next to `existing` under the desktop limits.
fn admit(existing: &[Attachment], candidates: Vec<Attachment>) -> (Vec<Attachment>, Vec<String>) {
    let mut total: u64 = existing.iter().map(|a| a.size).sum();
    let mut images = existing.iter().filter(|a| a.is_image()).count();
    let (mut ok, mut errors) = (Vec::new(), Vec::new());
    for a in candidates {
        if a.size > MAX_ATTACHMENT_BYTES {
            errors.push(format!(
                "{}: attachments must be 10 MiB or smaller.",
                a.name
            ));
        } else if a.is_image() && images >= MAX_IMAGES {
            errors.push(format!(
                "{}: no more than {MAX_IMAGES} images can be attached.",
                a.name
            ));
        } else if total + a.size > MAX_TOTAL_ATTACHMENT_BYTES {
            errors.push(format!(
                "{}: attachments cannot exceed 20 MiB total.",
                a.name
            ));
        } else {
            total += a.size;
            images += usize::from(a.is_image());
            ok.push(a);
        }
    }
    (ok, errors)
}

/// A surface with a composer that takes attachments: the start page, a
/// session pane.
pub(super) trait AttachmentHost: Sized + 'static {
    fn attachments(&self) -> &[Attachment];
    fn attachments_mut(&mut self) -> &mut Vec<Attachment>;
    fn set_attach_error(&mut self, error: Option<String>);
    /// Open the composer's image attachment `id` in the lightbox.
    fn open_attachment_image(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>);
}

/// Open the file picker for the host's composer.
pub(super) fn attach_files<T: AttachmentHost>(host: &mut T, cx: &mut Context<T>) {
    if cfg!(target_family = "wasm") {
        host.set_attach_error(Some(
            "Attaching files is not available in the browser yet.".to_string(),
        ));
        cx.notify();
        return;
    }
    let rx = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: true,
        prompt: Some("Attach".into()),
    });
    cx.spawn(async move |this, cx| {
        let paths = match rx.await {
            Ok(Ok(Some(paths))) => paths,
            Ok(Err(err)) => {
                let _ = this.update(cx, |this, cx| {
                    this.set_attach_error(Some(format!("Could not open the file picker: {err}")));
                    cx.notify();
                });
                return;
            }
            _ => return,
        };
        let _ = this.update(cx, |this, cx| attach_paths(this, paths, cx));
    })
    .detach();
}

/// Read files (picker, drop, copied in a file manager) off the UI thread and attach them.
pub(super) fn attach_paths<T: AttachmentHost>(
    _host: &mut T,
    paths: Vec<std::path::PathBuf>,
    cx: &mut Context<T>,
) {
    if paths.is_empty() {
        return;
    }
    cx.spawn(async move |this, cx| {
        let (loaded, errors) = cx
            .background_executor()
            .spawn(async move { read_attachments(paths) })
            .await;
        let _ = this.update(cx, |this, cx| add_attachments(this, loaded, errors, cx));
    })
    .detach();
}

/// Attach clipboard images (screenshots), named like the browser does (`pasted-image.png`).
pub(super) fn attach_images<T: AttachmentHost>(
    host: &mut T,
    images: Vec<(Vec<u8>, &'static str, &'static str)>,
    cx: &mut Context<T>,
) {
    let count = images.len();
    let loaded = images
        .into_iter()
        .enumerate()
        .map(|(ix, (bytes, mime, ext))| {
            let name = if count > 1 {
                format!("pasted-image-{}.{ext}", ix + 1)
            } else {
                format!("pasted-image.{ext}")
            };
            Attachment::from_bytes(attachment_id(), name, mime.to_string(), &bytes)
        })
        .collect();
    add_attachments(host, loaded, Vec::new(), cx);
}

/// Apply the desktop limits (10 MiB each, 20 MiB total, 8 images) and add what fits.
fn add_attachments<T: AttachmentHost>(
    host: &mut T,
    loaded: Vec<Attachment>,
    mut errors: Vec<String>,
    cx: &mut Context<T>,
) {
    let (ok, rejected) = admit(host.attachments(), loaded);
    host.attachments_mut().extend(ok);
    errors.extend(rejected);
    host.set_attach_error((!errors.is_empty()).then(|| errors.join(" ")));
    cx.notify();
}

/// `Textarea::on_paste` hook: images and copied files become attachments;
/// anything else falls through to the normal text paste.
pub(super) fn paste_handler<T: AttachmentHost>(
    cx: &mut Context<T>,
) -> impl Fn(&ClipboardItem, &mut Window, &mut App) -> bool + 'static {
    let this = cx.entity().downgrade();
    move |item, _, cx| {
        let Some(payload) = paste_payload(item) else {
            return false;
        };
        let _ = this.update(cx, |this, cx| match payload {
            PastePayload::Images(images) => attach_images(this, images, cx),
            PastePayload::Paths(paths) => attach_paths(this, paths, cx),
        });
        true
    }
}

/// Wrap a composer so files dragged from the desktop can be dropped onto it,
/// highlighted while they hover (desktop's `composer-prompt-zone`).
pub(super) fn drop_zone<T: AttachmentHost>(
    id: &'static str,
    composer: Div,
    p: &Palette,
    cx: &mut Context<T>,
) -> Stateful<Div> {
    let (tint, ring) = (p.accent_dim, p.accent);
    div()
        .id(id)
        .w_full()
        .relative()
        .rounded(px(18.))
        .child(composer)
        // A transparent overlay (no `occlude`, so clicks still reach the
        // composer) that tints the composer's own surface while files hover.
        .child(
            div()
                .id((id, 1usize))
                .absolute()
                .inset_0()
                .rounded(px(18.))
                .border_2()
                .border_color(transparent_black())
                .drag_over::<ExternalPaths>(move |style, _, _, _| {
                    style.bg(tint.opacity(0.35)).border_color(ring)
                }),
        )
        .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
            attach_paths(this, paths.paths().to_vec(), cx);
        }))
}

impl AttachmentHost for QmtApp {
    fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    fn attachments_mut(&mut self) -> &mut Vec<Attachment> {
        &mut self.attachments
    }

    fn set_attach_error(&mut self, error: Option<String>) {
        self.attach_error = error;
    }

    fn open_attachment_image(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.open_composer_image(id, window, cx);
    }
}

/// What a paste contributes as attachments instead of text.
pub(super) enum PastePayload {
    /// Raw images (screenshots, images copied from a browser or viewer): `(bytes, mime, extension)`.
    Images(Vec<(Vec<u8>, &'static str, &'static str)>),
    /// Files copied in a file manager.
    Paths(Vec<std::path::PathBuf>),
}

/// Images or copied files in the clipboard; `None` leaves the paste to the text input.
pub(super) fn paste_payload(item: &ClipboardItem) -> Option<PastePayload> {
    let mut images = Vec::new();
    for entry in item.entries() {
        match entry {
            ClipboardEntry::ExternalPaths(paths) if !paths.paths().is_empty() => {
                return Some(PastePayload::Paths(paths.paths().to_vec()));
            }
            ClipboardEntry::Image(image) => images.push((
                image.bytes.clone(),
                image.format.mime_type(),
                image.format.extension(),
            )),
            ClipboardEntry::String(s) => {
                if let Some(paths) = file_uri_paths(s.text()) {
                    return Some(PastePayload::Paths(paths));
                }
            }
            _ => {}
        }
    }
    (!images.is_empty()).then_some(PastePayload::Images(images))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|b| hex(*b)),
                bytes.get(i + 2).and_then(|b| hex(*b)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Local paths of a `file://` URI list (`text/uri-list`, or GNOME's
/// `copy\nfile://…` text), when every line is one.
fn parse_file_uris(text: &str) -> Option<Vec<std::path::PathBuf>> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .peekable();
    if matches!(lines.peek(), Some(&"copy" | &"cut")) {
        lines.next();
    }
    let paths = lines
        .map(|l| {
            let rest = l.strip_prefix("file://")?;
            // `file:///p` or `file://localhost/p`.
            let path = if rest.starts_with('/') {
                rest
            } else {
                rest.strip_prefix("localhost")?
            };
            path.starts_with('/')
                .then(|| std::path::PathBuf::from(percent_decode(path)))
        })
        .collect::<Option<Vec<_>>>()?;
    (!paths.is_empty()).then_some(paths)
}

/// Copied files that arrive as text: only when they all exist, so pasting a
/// `file://` URL as text into the prompt still works.
fn file_uri_paths(text: &str) -> Option<Vec<std::path::PathBuf>> {
    parse_file_uris(text).filter(|paths| paths.iter().all(|p| p.exists()))
}

pub(super) fn reasoning_color(level: &str, p: &Palette) -> Hsla {
    let (light, dark) = match level.trim().to_ascii_lowercase().as_str() {
        "low" => (0x0f766e, 0x5eead4),
        "medium" => (0x1d4ed8, 0x93c5fd),
        "high" => (0x92400e, 0xfcd34d),
        "max" => (0x7e22ce, 0xd8b4fe),
        _ => (0x475569, 0xcbd5e1),
    };
    rgb(if p.dark { dark } else { light }).into()
}

/// Desktop `--mode-tone-*`: a mode's colour by its position in the agent's
/// list (an unknown id falls back to an FNV hash of it, like desktop).
pub(super) fn mode_tone<'a>(
    ids: impl IntoIterator<Item = &'a str>,
    current: &str,
    p: &Palette,
) -> Hsla {
    const LIGHT: [u32; 8] = [
        0xc05621, 0x1a5fa8, 0x1a7f47, 0x7c3aad, 0x0f766e, 0xbe185d, 0xa16207, 0x4f46e5,
    ];
    const DARK: [u32; 8] = [
        0xfda36b, 0x93c5fd, 0x86efac, 0xc4b5fd, 0x5eead4, 0xf9a8d4, 0xffd27a, 0xa5b4fc,
    ];
    let ix = ids
        .into_iter()
        .position(|id| id == current)
        .unwrap_or_else(|| {
            let mut hash: u32 = 2166136261;
            for unit in current.encode_utf16() {
                hash ^= u32::from(unit);
                hash = hash.wrapping_mul(16777619);
            }
            hash as usize
        })
        % 8;
    rgb(if p.dark { DARK[ix] } else { LIGHT[ix] }).into()
}

pub(super) fn toolbar_button(id: &'static str, p: &Palette) -> Stateful<Div> {
    let hover = p.card_hover;
    h_flex()
        .id(id)
        .flex_none()
        .gap(px(6.72))
        .h(px(32.))
        .px(px(8.8))
        .border_1()
        .border_color(transparent_black())
        .rounded(px(8.))
        .bg(p.card)
        .hover(move |s| s.bg(hover))
        .cursor_pointer()
        .text_size(px(12.))
        .line_height(px(19.2))
        .font_weight(FontWeight(550.))
}

/// Desktop's split model pill: provider well, divider, label, optional mesh indicator.
pub(super) fn model_toolbar_button(
    id: &'static str,
    label: String,
    model: Option<&Model>,
    pending: bool,
    p: &Palette,
) -> Stateful<Div> {
    toolbar_button(id, p)
        .max_w(px(352.))
        .min_w_0()
        .h(px(31.44))
        .gap_0()
        .pl(px(4.8))
        .pr(px(9.6))
        .line_height(px(16.))
        .font_medium()
        .child(
            h_flex()
                .flex_none()
                .size(px(23.68))
                .justify_center()
                .child(if pending {
                    kit::spinner((id, 0usize), 14., p.muted).into_any_element()
                } else {
                    kit::provider_icon(model.map_or("", |m| m.provider.as_str()), 14., p.muted)
                }),
        )
        .child(
            div()
                .flex_none()
                .w(px(1.))
                .h(px(17.28))
                .ml(px(3.2))
                .mr(px(6.4))
                .bg(p.border.opacity(0.55)),
        )
        .child(
            h_flex()
                .min_w_0()
                .gap(px(4.8))
                .child(div().min_w_0().truncate().child(label))
                .when(model.is_some_and(|m| m.node.is_some()), |el| {
                    el.child(kit::icon(IconName::Network, 12., p.dim))
                }),
        )
}

impl QmtApp {
    // ---- actions ------------------------------------------------------------

    /// Tab / Shift+Tab in the Start composer: next / previous launch mode.
    pub(super) fn cycle_launch_mode(&mut self, back: bool, cx: &mut Context<Self>) {
        let n = LAUNCH_MODES.len();
        let current = self.store.read(cx).prefs.mode().to_string();
        let at = LAUNCH_MODES
            .iter()
            .position(|(id, _, _)| *id == current)
            .unwrap_or(0);
        let next = if back { (at + n - 1) % n } else { (at + 1) % n };
        self.store
            .update(cx, |s, cx| s.set_launch_mode(LAUNCH_MODES[next].0, cx));
    }

    pub(super) fn open_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.options_open = false;
        let Some(target) = self.picker_target(cx) else {
            return;
        };
        self.model_picker
            .update(cx, |picker, cx| picker.open(target, window, cx));
    }

    /// The start composer's model pill: the chosen model (with the mesh icon
    /// when another machine serves it), the reasoning badge, and the picker
    /// on click.
    pub(super) fn model_picker_button(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let agent = self.picker_agent(cx);
        let store = self.store.read(cx);
        let has_models = agent
            .as_deref()
            .is_some_and(|a| !store.models(a).is_empty());
        let model = agent
            .as_deref()
            .and_then(|a| store.launch_model(a))
            .cloned();
        let label = match (&model, has_models, agent.is_some()) {
            (Some(m), _, _) => m.label.clone(),
            (None, true, _) => "Choose model".into(),
            (None, false, true) => "Loading models…".into(),
            _ => "No model".into(),
        };
        let level = store.prefs.reasoning().to_string();
        let tip = model.as_ref().map(Model::describe);
        let button = model_toolbar_button("model-button", label, model.as_ref(), false, p)
            .when_some(tip, |el, tip| {
                el.tooltip(move |window, cx| kit::wrap_tip(tip.clone()).build(window, cx))
            })
            .when(has_models, |el| {
                el.on_click(cx.listener(|this, _, window, cx| this.open_model_picker(window, cx)))
            });
        kit::badged_tone(button, &level, reasoning_color(&level, p), p).into_any_element()
    }

    /// What the model picker applies to: the open session on the Session
    /// page, the models of the open delegate's owner while a route is being
    /// chosen, else a new session on the primary agent.
    pub(super) fn picker_target(&self, cx: &App) -> Option<Target> {
        if self.page == Page::Session {
            if self.delegates.picking.is_some() {
                // The route is reported, not applied to the session.
                let owner = self.main_key(cx)?.agent.to_string();
                return Some(Target::Launch { agent: owner });
            }
            return self.pane.read(cx).session().cloned().map(Target::Session);
        }
        self.store
            .read(cx)
            .primary_agent()
            .map(|agent| Target::Launch { agent })
    }

    /// Agent whose models the picker lists.
    pub(super) fn picker_agent(&self, cx: &App) -> Option<String> {
        self.picker_target(cx).map(|target| target.agent(cx))
    }

    /// Point the picker at what this frame shows: its target, and a
    /// delegate's own recent models and chosen route while one is open.
    pub(super) fn aim_model_picker(&mut self, cx: &mut Context<Self>) {
        let target = self.picker_target(cx);
        let recent = self.delegate_recent_models(cx);
        let selected = self.delegate_selected_key(cx);
        self.model_picker.update(cx, |picker, _| {
            picker.set_target(target);
            picker.set_overrides(recent, selected);
        });
    }

    /// A model was chosen in the picker. A session was changed by the picker;
    /// a delegate's row routes that delegate, and a launch choice becomes
    /// the launch preference of the start page.
    pub(super) fn on_model_picked(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.route_delegate_model(key, cx) {
            return;
        }
        if self.page != Page::Session
            && let Some(agent) = self.store.read(cx).primary_agent()
        {
            self.store
                .update(cx, |s, cx| s.set_launch_model(&agent, key.to_owned(), cx));
            self.model_picker.update(cx, |picker, cx| {
                picker.close(cx);
            });
        }
        cx.notify();
    }

    // ---- page ---------------------------------------------------------------

    pub(super) fn render_start(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *palette(cx);
        let cwd = self.cwd.read(cx).value().to_string();
        let prompt_empty = self.prompt.read(cx).value().trim().is_empty();
        let store = self.store.read(cx);

        let connected = store.connected_ids();
        let total = store.configs.len();
        let agent = store.primary_agent();
        let host = agent
            .as_deref()
            .and_then(|a| store.runtime(a)?.mesh.as_ref()?.hostname.clone());
        let connecting: Vec<String> = store
            .configs
            .iter()
            .filter(|c| store.state(&c.id) == ConnState::Connecting)
            .map(|c| c.name.clone())
            .collect();
        let failed: Vec<(String, String)> = store
            .configs
            .iter()
            .filter_map(|c| match store.state(&c.id) {
                ConnState::Failed(reason) => Some((c.name.clone(), reason)),
                _ => None,
            })
            .collect();
        let launch_busy = self.launch.busy;
        let launch_error = self.launch.error.clone();
        let started = self.launch.last_started.is_some();
        let mode = store.prefs.mode().to_string();
        let agent_choices: Vec<(String, String)> = connected
            .iter()
            .filter_map(|id| store.config(id).map(|c| (id.clone(), c.name.clone())))
            .collect();
        // Like the desktop Options button: name a non-default profile next to it.
        // Like the desktop Options button: name what is not the default
        // (peer, worktree, profile) next to it.
        let target_label = agent.as_deref().and_then(|a| {
            let node = store.launch.target.as_ref()?;
            store
                .launch_targets(a)
                .into_iter()
                .find(|t| t.node_id.as_ref() == Some(node))
                .map(|t| t.label)
        });
        let profile_summary = {
            let profile = agent.as_deref().and_then(|a| {
                if let Some(node) = &store.launch.target {
                    let id = store.launch.remote_profile.as_ref()?;
                    return Some(
                        store
                            .peer_profiles(a, node)
                            .and_then(|p| p.profiles.iter().find(|p| &p.id == id))
                            .map_or_else(|| id.clone(), |p| p.name.clone()),
                    );
                }
                let id = store.launch_profile(a);
                store
                    .profiles(a)
                    .iter()
                    .find(|p| p.id == id && id != DEFAULT_PROFILE)
                    .map(|p| p.name.clone())
            });
            let worktree = agent
                .as_deref()
                .map(|a| store.launch_worktree(a))
                .filter(|m| *m != WorktreeMode::Local)
                .map(|m| m.label().to_string());
            let parts: Vec<String> = [target_label.clone(), worktree, profile]
                .into_iter()
                .flatten()
                .collect();
            (!parts.is_empty()).then(|| parts.join(" · "))
        };

        let sessions = store.all_sessions();
        let running = sessions.iter().filter(|s| s.root && s.running()).count();
        let open_count = sessions.iter().filter(|s| s.root && s.open).count();
        let mut recent: Vec<SessionSummary> = sessions
            .into_iter()
            .filter(|s| s.root && s.open)
            .cloned()
            .collect();
        recent.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        recent.truncate(4);

        let has_input = !prompt_empty || !self.attachments.is_empty();
        let can_start = agent.is_some() && has_input && !launch_busy && !cwd.trim().is_empty();

        // ---- heading
        let heading = v_flex()
            .items_center()
            .gap_2()
            .child(div().mb_1().child(kit::boot_mark(88., &p)))
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(p.muted)
                    .child("NEW SESSION"),
            )
            .child(
                div()
                    .text_3xl()
                    .font_bold()
                    .child("What should QueryMT do?"),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .text_color(p.muted)
                    .child(kit::icon(IconName::FolderOpen, 15., p.muted))
                    .child("Working in")
                    .child(div().font_semibold().text_color(p.text).child(
                        if cwd.trim().is_empty() {
                            "Choose workspace".to_string()
                        } else {
                            workspace_name(&cwd)
                        },
                    )),
            );

        // ---- setup left to finish; it covers a missing agent too
        let checklist = self.setup_checklist(&p, cx);
        // ---- no agent yet (while agents are still connecting)
        let no_agent = (agent.is_none() && checklist.is_none()).then(|| {
            let (title, detail) = if !connecting.is_empty() {
                (
                    format!("Connecting to {}…", connecting.join(", ")),
                    "Sessions can start as soon as an agent is ready.".to_string(),
                )
            } else if let Some((name, reason)) = failed.first() {
                (format!("{name} is not reachable"), reason.clone())
            } else if total == 0 {
                (
                    "No agents configured".to_string(),
                    "Add a local qmtcode or connect to a remote ACP WebSocket server.".to_string(),
                )
            } else {
                (
                    "No agent connected".to_string(),
                    "Start or connect one of your agents to begin.".to_string(),
                )
            };
            h_flex()
                .w_full()
                .gap_3()
                .p_4()
                .rounded(px(14.))
                .bg(p.surface)
                .border_1()
                .border_color(if failed.is_empty() {
                    p.border
                } else {
                    p.warn.opacity(0.5)
                })
                .child(kit::icon(
                    if failed.is_empty() {
                        IconName::Bot
                    } else {
                        IconName::TriangleAlert
                    },
                    18.,
                    if failed.is_empty() { p.accent } else { p.warn },
                ))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_semibold().child(title))
                        .child(div().text_sm().text_color(p.muted).truncate().child(detail)),
                )
                .child(
                    kit::button("open-agents")
                        .primary()
                        .small()
                        .label(if total == 0 {
                            "Add agent"
                        } else {
                            "Open Agents"
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.go(Page::Agents, cx))),
                )
        });

        // ---- toolbar
        let plus = toolbar_button("attach", &p)
            .w(px(32.))
            .px_0()
            .justify_center()
            .rounded_full()
            .tooltip(|window, cx| kit::wrap_tip("Attach files").build(window, cx))
            .child(kit::icon(IconName::Plus, 17., p.muted))
            .on_click(cx.listener(|this, _, _, cx| attach_files(this, cx)));

        let model_button = self.model_picker_button(&p, cx);

        let options_button = toolbar_button("options-button", &p)
            .child(kit::icon(IconName::Settings2, 14., p.muted))
            .child("Options")
            .children(profile_summary.map(|name| {
                div()
                    .max_w(px(220.))
                    .truncate()
                    .text_xs()
                    .text_color(p.dim)
                    .child(name)
            }))
            .child(kit::icon(IconName::ChevronDown, 13., p.dim))
            .on_click(cx.listener(|this, ev: &ClickEvent, window, cx| {
                if this.dismissed.closed_by(START_OPTIONS, ev) {
                    return;
                }
                this.close_option_menu(window, cx);
                this.options_open = !this.options_open;
                if !this.options_open {
                    this.restore_focus(window, cx);
                }
                // A peer chosen before a reconnect: list its profiles again.
                let store = this.store.read(cx);
                if this.options_open
                    && let Some(agent) = store.primary_agent()
                    && let Some(node) = store.launch.target.clone()
                    && store.peer_profiles(&agent, &node).is_none()
                {
                    this.store
                        .update(cx, |s, cx| s.load_peer_profiles(&agent, &node, cx));
                }
                cx.notify();
            }));
        let options = div()
            .relative()
            .child(kit::badged_tone(
                options_button,
                &mode,
                mode_tone(LAUNCH_MODES.iter().map(|m| m.0), &mode, &p),
                &p,
            ))
            .when(self.options_open, |el| {
                el.child(
                    deferred(self.render_options_panel(
                        &mode,
                        &agent_choices,
                        agent.as_deref(),
                        &p,
                        cx,
                    ))
                    .with_priority(1),
                )
            });

        let attachments = super::preview::attachment_strip(self, &p, cx).map(|s| s.pt_1p5());

        let composer =
            v_flex()
                .w_full()
                .p_3()
                .gap_2()
                .rounded(px(14.))
                .bg(p.surface)
                .border_1()
                .border_color(p.border)
                .shadow_sm()
                .child(self.workspace.clone())
                .child(div().px_1().text_xs().text_color(p.dim).child(
                    match (&target_label, &host) {
                        (Some(peer), _) => {
                            format!("Use an absolute path on {peer}, where the session will run.")
                        }
                        (None, Some(host)) => {
                            format!("Use an absolute path on {host}, where the agent runs.")
                        }
                        (None, None) => {
                            "Use an absolute path on the machine where the agent runs.".to_string()
                        }
                    },
                ))
                // Tab / Shift+Tab cycle the launch mode instead of indenting.
                .child(
                    div()
                        .min_h(px(96.))
                        .px_1()
                        .capture_action(cx.listener(|this, a: &Enter, window, cx| {
                            let send = this.store.read(cx).prefs.send_shortcut;
                            if send.submits(a.shift, a.secondary) {
                                cx.stop_propagation();
                                this.start_session(window, cx);
                            }
                        }))
                        .capture_action(cx.listener(|this, _: &IndentInline, _, cx| {
                            cx.stop_propagation();
                            this.cycle_launch_mode(false, cx);
                        }))
                        .capture_action(cx.listener(|this, _: &OutdentInline, _, cx| {
                            cx.stop_propagation();
                            this.cycle_launch_mode(true, cx);
                        }))
                        .child(
                            Textarea::new(&self.prompt)
                                .appearance(false)
                                .on_paste(paste_handler(cx)),
                        ),
                )
                .children(attachments)
                .when_some(self.attach_error.clone(), |el, err| {
                    el.child(
                        h_flex()
                            .gap_1p5()
                            .text_xs()
                            .text_color(p.danger)
                            .child(kit::icon(IconName::CircleAlert, 13., p.danger))
                            .child(err),
                    )
                })
                .child(kit::divider(&p))
                .child(
                    h_flex()
                        .gap_2()
                        .child(plus)
                        .child(model_button)
                        .child(options)
                        .child(div().flex_1())
                        .child(
                            kit::button("start")
                                .primary()
                                .w(px(120.))
                                .when(!launch_busy, |button| button.icon(IconName::SendHorizontal))
                                .label(if launch_busy { "Starting…" } else { "Start" })
                                .map(|button| {
                                    kit::loading_button(button, "start-spinner", launch_busy, 16.)
                                })
                                .inactive(!can_start)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.start_session(window, cx)
                                })),
                        ),
                );

        let banner = launch_error.map(|err| {
            h_flex()
                .w_full()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(px(10.))
                .bg(p.danger.opacity(0.1))
                .border_1()
                .border_color(p.danger.opacity(0.3))
                .text_sm()
                .child(kit::icon(IconName::CircleAlert, 15., p.danger))
                .child(div().flex_1().min_w_0().child(err))
                .child(
                    kit::icon_button("dismiss-error", IconName::X, "Dismiss").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.launch.error = None;
                            cx.notify();
                        }),
                    ),
                )
        });
        let started_note = started.then(|| {
            h_flex()
                .gap_2()
                .text_sm()
                .text_color(p.muted)
                .child(kit::dot(p.success))
                .child("Session started. It runs on the agent and shows up under Open sessions.")
        });

        // ---- continue where you left off
        let recent_section = (!recent.is_empty()).then(|| {
            v_flex()
                .w_full()
                .gap_0p5()
                .child(
                    div()
                        .px_2()
                        .pb_1()
                        .text_xs()
                        .font_semibold()
                        .text_color(p.muted)
                        .child("Continue where you left off"),
                )
                .children(recent.into_iter().map(|s| {
                    let hover = p.card;
                    let open_bg = p.card_hover;
                    let s2 = s.clone();
                    h_flex()
                        .id(kit::keyed("recent", s.id.clone()))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.open_summary(&s2, cx)))
                        .gap_3()
                        .px_2()
                        .py_1p5()
                        .rounded(px(10.))
                        .hover(move |el| el.bg(hover))
                        .child(kit::identicon(&s.id, 18.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .font_medium()
                                .truncate()
                                .child(s.display_title()),
                        )
                        .when(s.running(), |el| {
                            el.child(kit::spinner(
                                kit::keyed("recent-spin", s.id.clone()),
                                14.,
                                p.success,
                            ))
                        })
                        .child(
                            div()
                                .flex_none()
                                .max_w(px(160.))
                                .truncate()
                                .text_xs()
                                .text_color(p.dim)
                                .child(workspace_name(&s.cwd)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(36.))
                                .text_right()
                                .text_xs()
                                .text_color(p.dim)
                                .child(short_time(&s.updated_at)),
                        )
                        .child(
                            div()
                                .id(kit::keyed("recent-open", s.id.clone()))
                                .flex_none()
                                .cursor_pointer()
                                .px_2p5()
                                .py_0p5()
                                .rounded_full()
                                .bg(p.card)
                                .hover(move |el| el.bg(open_bg))
                                .text_xs()
                                .font_medium()
                                .child("Open")
                                .on_click({
                                    let s = s.clone();
                                    cx.listener(move |this, _, _, cx| this.open_summary(&s, cx))
                                }),
                        )
                }))
        });

        let pills = h_flex()
            .gap_3()
            .justify_center()
            .flex_wrap()
            .child(kit::count_pill(
                running.to_string(),
                if running == 1 {
                    "session running"
                } else {
                    "sessions running"
                },
                &p,
            ))
            .child(
                div()
                    .id("pill-agents")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.go(Page::Agents, cx)))
                    .child(kit::count_pill(
                        format!("{}/{}", connected.len(), total),
                        if total == 1 {
                            "agent connected"
                        } else {
                            "agents connected"
                        },
                        &p,
                    )),
            )
            .child(
                div()
                    .id("pill-sessions")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.go(Page::Sessions, cx)))
                    .child(kit::count_pill(
                        open_count.to_string(),
                        if open_count == 1 {
                            "open session"
                        } else {
                            "open sessions"
                        },
                        &p,
                    )),
            );

        div()
            .w_full()
            .min_h_full()
            .flex()
            .justify_center()
            .px_6()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(880.))
                    .min_h_full()
                    .py_6()
                    .items_center()
                    .gap_6()
                    // Equal flexible regions keep the composer centered, independently
                    // of the heading and recent-session heights, as on desktop/web.
                    .child(
                        v_flex()
                            .w_full()
                            .flex_1()
                            .justify_end()
                            .items_center()
                            .gap_6()
                            .child(heading)
                            .children(checklist)
                            .children(no_agent)
                            .children(banner)
                            .children(started_note),
                    )
                    .child(drop_zone("start-drop", composer, &p, cx).flex_none())
                    .child(
                        v_flex()
                            .w_full()
                            .flex_1()
                            .gap_6()
                            .children(recent_section)
                            .child(pills),
                    ),
            )
    }

    fn render_options_panel(
        &self,
        mode: &str,
        agents: &[(String, String)],
        agent: Option<&str>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let section = |title: &'static str| {
            div()
                .text_xs()
                .font_semibold()
                .text_color(p.dim)
                .child(title)
        };
        let describe = |list: &[(&str, &str, &str)], id: &str| {
            list.iter()
                .find(|(v, _, _)| *v == id)
                .map(|(_, _, d)| d.to_string())
                .unwrap_or_default()
        };
        let segment = |items: Vec<AnyElement>| kit::segment_bar(p).children(items);

        let modes: Vec<AnyElement> = LAUNCH_MODES
            .iter()
            .enumerate()
            .map(|(ix, (id, label, _))| {
                let id = id.to_string();
                kit::segment(("opt-mode", ix), mode == id, p)
                    .flex_1()
                    .label(*label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.store.update(cx, |s, cx| s.set_launch_mode(&id, cx));
                        this.restore_focus(window, cx);
                    }))
                    .into_any_element()
            })
            .collect();

        let profile_picker = self.profile_picker(p, cx);
        let peer_note = self.peer_profiles_note(p, cx);
        let target = self.target_picker(p, cx);
        let worktree = self.worktree_section(agent, p, cx);

        v_flex()
            .id("options-panel")
            .absolute()
            .top(px(40.))
            .left_0()
            .w(px(340.))
            .p_3()
            .gap_2()
            .rounded(px(12.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .occlude()
            .on_mouse_down_out(cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                // A select menu overhangs the panel: a click in it is not outside.
                if this.option_menu.is_some()
                    && this.profile_menu_bounds.get().contains(&ev.position)
                {
                    return;
                }
                this.dismissed.record(START_OPTIONS, ev);
                this.close_option_menu(window, cx);
                this.options_open = false;
                this.restore_focus(window, cx);
                cx.notify();
            }))
            .child(section("Mode"))
            .child(segment(modes))
            .child(
                div()
                    .text_xs()
                    .text_color(p.muted)
                    .child(describe(&LAUNCH_MODES, mode)),
            )
            .when_some(profile_picker, |el, picker| {
                el.child(div().pt_1().child(kit::divider(p)))
                    .child(section("Profile"))
                    .child(picker)
                    .children(peer_note)
            })
            .when_some(target, |el, target| {
                el.child(div().pt_1().child(kit::divider(p)))
                    .child(section("Target"))
                    .child(target)
            })
            .when_some(worktree, |el, worktree| {
                el.child(div().pt_1().child(kit::divider(p)))
                    .child(section("Workspace"))
                    .child(worktree)
            })
            .when(agents.len() > 1, |el| {
                el.child(div().pt_1().child(kit::divider(p)))
                    .child(section("Agent"))
                    .child(v_flex().gap_0p5().children(agents.iter().map(|(id, name)| {
                        let id = id.clone();
                        kit::button(kit::keyed("opt-agent", id.clone()))
                            .xsmall()
                            .ghost()
                            .w_full()
                            .justify_start()
                            .icon(IconName::Bot)
                            .label(name.clone())
                            .selected(agent == Some(id.as_str()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |s, cx| s.set_primary_agent(&id, cx))
                            }))
                    })))
            })
    }

    /// Checkout or a new git worktree (desktop Workspace), for a local
    /// target on an agent with managed worktrees.
    fn worktree_section(
        &self,
        agent: Option<&str>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let store = self.store.read(cx);
        let agent = agent?;
        if !store.can_use_worktree(agent) {
            return None;
        }
        let current = store.launch_worktree(agent);
        let segments = WorktreeMode::ALL
            .iter()
            .enumerate()
            .map(|(ix, (mode, label, _))| {
                let mode = *mode;
                kit::segment(("opt-worktree", ix), current == mode, p)
                    .flex_1()
                    .label(*label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.store
                            .update(cx, |s, cx| s.set_launch_worktree(mode, cx))
                    }))
            });
        let description = WorktreeMode::ALL
            .iter()
            .find(|(m, ..)| *m == current)
            .map_or("", |(.., d)| d);
        Some(
            v_flex()
                .gap_2()
                .child(kit::segment_bar(p).children(segments))
                .child(div().text_xs().text_color(p.muted).child(description)),
        )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the gpui glob brings a `test` macro that shadows `#[test]`.
    use super::{Attachment, admit, file_uri_paths, parse_file_uris, paste_payload};
    use gpui_kit::ClipboardItem;

    fn att(name: &str, mime: &str, size: u64) -> Attachment {
        Attachment {
            id: name.into(),
            name: name.into(),
            mime: mime.into(),
            size,
            data: String::new(),
            image: None,
        }
    }

    #[test]
    fn parses_copied_file_uris() {
        let paths =
            parse_file_uris("copy\nfile:///home/me/My%20Shot.png\nfile://localhost/tmp/a.txt\n")
                .unwrap();
        assert_eq!(
            paths,
            vec![
                std::path::PathBuf::from("/home/me/My Shot.png"),
                std::path::PathBuf::from("/tmp/a.txt")
            ]
        );
        assert!(parse_file_uris("# comment\r\nfile:///x\r\n").is_some());
        assert!(parse_file_uris("see file:///x").is_none());
        assert!(parse_file_uris("https://example.com").is_none());
        assert!(parse_file_uris("file://remote-host/x").is_none());
        assert!(parse_file_uris("").is_none());
    }

    #[test]
    fn text_with_missing_files_stays_text() {
        assert!(file_uri_paths("file:///definitely/not/here.png").is_none());
        let item = ClipboardItem::new_string("hello".into());
        assert!(paste_payload(&item).is_none());
    }

    #[test]
    fn admits_within_desktop_limits() {
        let existing: Vec<_> = (0..7)
            .map(|i| att(&format!("i{i}"), "image/png", 1))
            .collect();
        let (ok, errors) = admit(
            &existing,
            vec![
                att("a", "image/png", 1),
                att("b", "image/png", 1),
                att("t", "text/plain", 1),
            ],
        );
        assert_eq!(
            ok.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["a", "t"]
        );
        assert!(errors[0].contains("no more than 8 images"));
        let mib = 1024 * 1024;
        let (ok, errors) = admit(
            &[att("big", "text/plain", 9 * mib)],
            vec![
                att("x", "text/plain", 11 * mib),
                att("y", "text/plain", 9 * mib),
                att("z", "text/plain", 3 * mib),
            ],
        );
        assert_eq!(
            ok.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["y"]
        );
        assert!(errors[0].contains("10 MiB"));
        assert!(errors[1].contains("20 MiB total"));
    }
}

#[cfg(test)]
mod launch_option_tests {
    use super::{Page, QmtApp};
    use crate::store::launch::WorktreeMode;

    #[gpui_kit::test]
    fn options_offer_target_worktree_and_peer_profiles(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use gpui_kit::test::TestWindowExt as _;

        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| crate::store::launch::peer_fixture());
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.go(Page::Start, cx);
                app.options_open = true;
            });
            window.render_frame(cx);
            app.update(cx, |app, cx| {
                let p = *crate::theme::palette(cx);
                assert!(app.worktree_section(Some("test-agent"), &p, cx).is_some());
                app.store.update(cx, |s, cx| {
                    s.set_launch_worktree(WorktreeMode::Worktree, cx)
                });
            });
            // Target is a searchable select: open it, search, Enter picks.
            // (Its trigger is an app button, which the test observer does not
            // register; open it as the click does.)
            app.update(cx, |app, cx| {
                app.open_option_menu(super::super::profile_select::OptionMenu::Target, window, cx)
            });
            window.render_frame(cx);
            app.update(cx, |app, cx| {
                assert!(app.option_menu.is_some());
                app.profile_query
                    .update(cx, |s, cx| s.set_value("tho", window, cx));
            });
            window.render_frame(cx);
            app.update(cx, |app, cx| app.pick_option_at_cursor(window, cx));
            window.render_frame(cx);
            app.update(cx, |app, cx| {
                assert!(app.option_menu.is_none());
                assert_eq!(app.store.read(cx).launch.target.as_deref(), Some("thor-id"));
            });
            app.update(cx, |app, cx| {
                let p = *crate::theme::palette(cx);
                // A peer: no worktrees, and its own profiles.
                assert!(app.worktree_section(Some("test-agent"), &p, cx).is_none());
                let store = app.store.read(cx);
                assert_eq!(store.launch_worktree("test-agent"), WorktreeMode::Local);
                assert!(store.peer_profiles("test-agent", "thor-id").is_some());
            });
        })
        .unwrap();
    }
    /// The start page's launch shows the new session in this window, as it
    /// did when the store owned the main window's session.
    #[gpui_kit::test]
    fn start_page_launch_shows_the_session_in_the_main_window(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use serde_json::json;

        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        let (rpc, requests) = store.rpc_for_test("test-agent");
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.go(Page::Start, cx);
                app.prompt
                    .update(cx, |s, cx| s.set_value("hello", window, cx));
                app.cwd.update(cx, |s, cx| s.set_value("/work", window, cx));
                app.start_session(window, cx);
                assert!(app.launch.busy);
                assert_eq!(app.page, Page::Session);
                assert!(app.pane.read(cx).session().is_none());
            });
        })
        .unwrap();

        // The agent creates the session and answers its setup.
        loop {
            cx.run_until_parked();
            let Ok(text) = requests.try_recv() else { break };
            let request: serde_json::Value = serde_json::from_str(&text).unwrap();
            let result = if request["method"] == "session/new" {
                json!({ "sessionId": "s-new" })
            } else {
                json!({})
            };
            rpc.handle(&json!({ "id": request["id"], "result": result }).to_string());
        }
        app.read_with(cx, |app, cx| {
            assert!(!app.launch.busy && app.launch.error.is_none());
            let key = crate::session::open::SessionKey::new("test-agent", "s-new");
            assert_eq!(app.main_key(cx), Some(key));
        });
    }
}
