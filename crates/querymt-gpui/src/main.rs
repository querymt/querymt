//! QueryMT desktop — GPUI prototype.
//!
//! One UI, two targets:
//! - native: `main` (Wayland/X11/macOS/Windows window)
//! - web:    `web::start`, compiled to wasm32 and rendered into a <canvas>.
//!
//! Scope so far: the QueryMT shell, the Start page and the Agents page, wired
//! to real agents over ACP (local `qmtcode --acp` process or a local/remote
//! `qmtcode` WebSocket server).

// The browser starts through wasm-bindgen, not Rust's executable entry point.
#![cfg_attr(target_family = "wasm", no_main)]

mod highlight;
mod rpc;
mod session;
mod store;
mod theme;
mod transport;
mod ui;
mod usage;
mod util;

use std::borrow::Cow;

use gpui_kit::*;

#[cfg(not(target_family = "wasm"))]
fn main() {
    let app = gpui_kit::application().with_assets(AppAssets(gpui_kit::assets::AllAssets));
    run(app);
}

gpui_kit::actions!(querymt, [Quit]);

const BRAND_SVG: &[u8] = include_bytes!("../assets/brand/querymt-stencil.svg");
const BRAND_DARK_SVG: &[u8] = include_bytes!("../assets/brand/querymt-stencil-dark.svg");

/// Provider logos (`providers/<id>.svg`), split from the desktop app's sprite.
macro_rules! provider_icons {
    ($($id:literal),* $(,)?) => {
        const PROVIDER_SVGS: &[(&str, &[u8])] = &[
            $(($id, include_bytes!(concat!("../assets/providers/", $id, ".svg")))),*
        ];
    };
}
provider_icons!(
    "openai", "anthropic", "google", "mistral", "groq", "xai", "deepseek", "openrouter", "alibaba",
    "moonshot", "kimi", "zai", "codex", "ollama", "llama-cpp", "mistral-rs", "izwi", "vllm-cpp",
);

/// Asset path of a provider's logo, if we ship one (runtime ids map onto sprite ids).
pub fn provider_icon_path(provider: &str) -> Option<String> {
    let key = provider.trim().to_lowercase();
    let id = match key.as_str() {
        "kimi-code" => "kimi",
        "llama.cpp" => "llama-cpp",
        "mrs" | "mistral.rs" => "mistral-rs",
        "z.ai" => "zai",
        "vllm.cpp" => "vllm-cpp",
        other => other,
    };
    PROVIDER_SVGS.iter().any(|(k, _)| *k == id).then(|| format!("providers/{id}.svg"))
}

/// Icon assets plus app-specific assets (`brand/*`).
pub struct AppAssets<A: AssetSource>(pub A);

impl<A: AssetSource> AssetSource for AppAssets<A> {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match path {
            "brand/querymt.svg" => return Ok(Some(Cow::Borrowed(BRAND_SVG))),
            "brand/querymt-dark.svg" => return Ok(Some(Cow::Borrowed(BRAND_DARK_SVG))),
            _ => {}
        }
        if let Some(id) = path.strip_prefix("providers/").and_then(|p| p.strip_suffix(".svg")) {
            if let Some((_, bytes)) = PROVIDER_SVGS.iter().find(|(k, _)| *k == id) {
                return Ok(Some(Cow::Borrowed(bytes)));
            }
        }
        self.0.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        self.0.list(path)
    }
}

fn install_fonts(cx: &mut App) {
    const FONTS: &[&[u8]] = &[
        include_bytes!(concat!(env!("OUT_DIR"), "/fonts/IBMPlexSans-Regular.ttf")),
        include_bytes!(concat!(env!("OUT_DIR"), "/fonts/IBMPlexSans-Medium.ttf")),
        include_bytes!(concat!(env!("OUT_DIR"), "/fonts/IBMPlexSans-SemiBold.ttf")),
        include_bytes!(concat!(env!("OUT_DIR"), "/fonts/IBMPlexSans-Bold.ttf")),
        include_bytes!(concat!(env!("OUT_DIR"), "/fonts/IBMPlexMono-Regular.ttf")),
    ];
    cx.text_system()
        .add_fonts(FONTS.iter().map(|f| Cow::Borrowed(*f)).collect())
        .expect("failed to load embedded fonts");
}

/// Shared app bootstrap for every target.
pub fn run(app: Application) {
    app.run(|cx: &mut App| {
        // Must run before any gpui-component widget is used.
        gpui_kit::init(cx);
        // IBM Plex is the QueryMT typeface; embed it so both targets match.
        install_fonts(cx);

        let store = cx.new(store::AgentsStore::new);
        #[cfg(target_family = "wasm")]
        let forced = web_sys::window()
            .and_then(|w| w.location().search().ok())
            .and_then(|s| if s.contains("theme=dark") { Some(true) } else if s.contains("theme=light") { Some(false) } else { None });
        #[cfg(not(target_family = "wasm"))]
        let forced: Option<bool> = std::env::var("QMT_THEME").ok().map(|t| t == "dark");
        let dark = forced.or(store.read(cx).prefs.dark).unwrap_or_else(|| {
            matches!(cx.window_appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark)
        });
        let accent = store.read(cx).prefs.accent.clone();
        theme::apply(dark, &accent, cx);

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("ctrl-b", ui::ToggleSidebar, None),
            KeyBinding::new("cmd-b", ui::ToggleSidebar, None),
        ]);

        let options = if cfg!(target_family = "wasm") {
            WindowOptions::default()
        } else {
            let bounds = Bounds::centered(None, size(px(1320.), px(860.)), cx);
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.), px(560.))),
                // Keep our custom header; the upstream Linux frame needs a
                // transparent surface around it for its rounded corners and shadow.
                titlebar: Some(TitlebarOptions {
                    title: Some("QueryMT".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.), px(15.))),
                }),
                app_owns_titlebar_drag: true,
                window_decorations: Some(WindowDecorations::Client),
                window_background: if cfg!(target_os = "linux") {
                    WindowBackgroundAppearance::Transparent
                } else {
                    WindowBackgroundAppearance::Opaque
                },
                app_id: Some("querymt-gpui".into()),
                ..Default::default()
            }
        };

        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| ui::QmtApp::new(store, window, cx)))
            .expect("failed to open window");
        cx.activate(true);
    });
}

/// Browser entry point. `wasm-bindgen` calls this when the module loads.
#[cfg(target_family = "wasm")]
mod web {
    use wasm_bindgen::prelude::*;

    // Embed exactly the icons the app and the components it uses need: the
    // stock wasm loader fetches lazily over HTTP and never redraws.
    gpui_kit::assets::icon_assets!(WebIcons, [
        // app
        LayoutDashboard, Inbox, MessagesSquare, Bot, Network, Settings, Sun, Moon, FolderOpen,
        TriangleAlert, CircleAlert, SendHorizontal, X, Plus, RefreshCw, Globe, Terminal, Info,
        Pencil, Square, Play, ArrowLeft, ArrowUp, CircleStop, CircleCheck, CircleX, CircleDashed, Circle,
        Wrench, FilePen, FileSearch, SquareTerminal, ListChecks, ShieldAlert, Hand, MessageSquare, ExternalLink, Brain, RotateCcw, ToggleLeft, ToggleRight, Trash,
        WindowMinimize, WindowMaximize, WindowRestore, WindowClose,
        PanelLeftOpen, PanelLeftClose, Palette, Keyboard, KeyRound, Ticket,
        SlidersHorizontal, Paperclip, Pin, PinOff, File, FileText, Image, Clock, Archive, Brain, Ellipsis,
        // session view + composer
        EllipsisVertical, Sparkles, FilePenLine, ListTodo, MessageCircleQuestionMark, Navigation, Settings2,
        Diff, ArrowDown,
        // model picker
        Gauge, Type, AudioLines, Video,
        // used internally by gpui-component widgets
        Check, ChevronDown, ChevronUp, ChevronLeft, ChevronRight, ChevronsUpDown, Close, CircleX,
        Search, Copy, Eye, EyeOff, Loader, LoaderCircle, Minus, CircleCheck,
    ]);

    #[wasm_bindgen(start)]
    pub fn start() {
        gpui_kit::platform::web_init();
        // Single-threaded: works on any static server, no COOP/COEP headers.
        let app = gpui_kit::platform::single_threaded_web().with_assets(super::AppAssets(WebIcons));
        super::run(app);
    }
}
