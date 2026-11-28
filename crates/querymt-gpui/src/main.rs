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

#[cfg(test)]
mod film;
mod highlight;
#[cfg(not(target_family = "wasm"))]
mod local_agent;
mod profile_templates;
#[cfg(not(target_family = "wasm"))]
mod quick;
mod rpc;
mod rpc_log;
mod search;
mod session;
mod store;
mod telemetry;
mod theme;
mod transport;
mod ui;
mod update;
mod usage;
mod util;

use std::borrow::Cow;

use gpui_kit::*;

#[cfg(not(target_family = "wasm"))]
fn main() {
    // `--quick` toggles the quick panel of the running instance, or starts the
    // app with only the panel. Without the panel enabled it is a normal start.
    let quick = std::env::args().skip(1).any(|arg| arg == "--quick");
    let panel_enabled = store::persisted_quick_prefs().enabled;
    if panel_enabled {
        let command = if quick {
            quick::instance::Command::Quick
        } else {
            quick::instance::Command::Show
        };
        if quick::instance::forward(&quick::instance::Endpoint::for_user(), command) {
            return;
        }
    }
    telemetry::init();
    let app = gpui_kit::application().with_assets(AppAssets(gpui_kit::assets::AllAssets));
    run(app, quick && panel_enabled);
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
    "openai",
    "anthropic",
    "google",
    "mistral",
    "groq",
    "xai",
    "deepseek",
    "openrouter",
    "alibaba",
    "moonshot",
    "kimi",
    "zai",
    "codex",
    "ollama",
    "llama-cpp",
    "mistral-rs",
    "izwi",
    "vllm-cpp",
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
    PROVIDER_SVGS
        .iter()
        .any(|(k, _)| *k == id)
        .then(|| format!("providers/{id}.svg"))
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
        if let Some(bytes) = ui::kit::boot_mark_asset(path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        if let Some(id) = path
            .strip_prefix("providers/")
            .and_then(|p| p.strip_suffix(".svg"))
            && let Some((_, bytes)) = PROVIDER_SVGS.iter().find(|(k, _)| *k == id)
        {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        self.0.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        self.0.list(path)
    }
}

pub(crate) fn install_fonts(cx: &mut App) {
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
/// `panel_only`: open the quick panel and no main window (`--quick`).
pub fn run(app: Application, panel_only: bool) {
    app.run(move |cx: &mut App| {
        // Must run before any gpui-component widget is used.
        gpui_kit::init(cx);
        // IBM Plex is the QueryMT typeface; embed it so both targets match.
        install_fonts(cx);

        let store = cx.new(store::AgentsStore::new);
        #[cfg(target_family = "wasm")]
        let forced = web_sys::window()
            .and_then(|w| w.location().search().ok())
            .and_then(|s| {
                if s.contains("theme=dark") {
                    Some(true)
                } else if s.contains("theme=light") {
                    Some(false)
                } else {
                    None
                }
            });
        #[cfg(not(target_family = "wasm"))]
        let forced: Option<bool> = std::env::var("QMT_THEME").ok().map(|t| t == "dark");
        let dark = forced.or(store.read(cx).prefs.dark).unwrap_or_else(|| {
            matches!(
                cx.window_appearance(),
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            )
        });
        let appearance = store.read(cx).prefs.appearance(dark);
        theme::apply(appearance, cx);

        cx.on_action(|_: &Quit, cx| cx.quit());
        #[cfg(not(target_family = "wasm"))]
        cx.on_app_quit(|_| async { telemetry::shutdown() }).detach();
        cx.bind_keys([
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        ui::bind_keys(cx);

        #[cfg(target_family = "wasm")]
        {
            let _ = panel_only;
            ui::open_main_window(&store, cx).expect("failed to open window");
            cx.activate(true);
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let main = if panel_only {
                None
            } else {
                let main = ui::open_main_window(&store, cx).expect("failed to open window");
                cx.activate(true);
                Some(main)
            };
            let host = quick::QuickHost::install(store.clone(), main, cx);
            if panel_only {
                host.update(cx, |host, cx| host.show(cx));
            }
        }
    });
}

/// The icons the web build embeds: the stock wasm loader fetches lazily over
/// HTTP and never redraws, so every icon the app or its widgets draw must be
/// listed. On wasm this is the `WebIcons` asset source; in tests, the names,
/// so `web_icons_cover_the_app` catches an icon used but not listed.
macro_rules! web_icons {
    ($($icon:ident),* $(,)?) => {
        #[cfg(target_family = "wasm")]
        gpui_kit::assets::icon_assets!(WebIcons, [$($icon),*]);
        #[cfg(test)]
        const WEB_ICONS: &[&str] = &[$(stringify!($icon)),*];
    };
}

web_icons!(
    // Every icon the app names (`IconName::…`; a test keeps this in step).
    Archive,
    ArchiveRestore,
    ArrowDownLeft,
    ArrowDownToLine,
    ArrowLeft,
    ArrowUpRight,
    ArrowUpToLine,
    AudioLines,
    BookmarkPlus,
    Bot,
    Braces,
    Brain,
    Bug,
    CalendarClock,
    Check,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    ChevronsDown,
    ChevronsDownUp,
    ChevronsUp,
    ChevronsUpDown,
    ChevronUp,
    Circle,
    CircleAlert,
    CircleArrowUp,
    CircleCheck,
    CircleCheckBig,
    CircleDashed,
    CircleDot,
    CircleQuestionMark,
    CircleStop,
    Clock,
    CloudOff,
    Copy,
    Diff,
    Ellipsis,
    EllipsisVertical,
    Eraser,
    ExternalLink,
    Eye,
    FileCode,
    FileCog,
    FilePenLine,
    FileText,
    FingerprintPattern,
    Folder,
    FolderClock,
    FolderOpen,
    FolderPlus,
    FolderSearch,
    FoldVertical,
    Gauge,
    GitBranch,
    GitBranchPlus,
    GitCommitHorizontal,
    GitFork,
    Globe,
    HardDrive,
    Image,
    Inbox,
    Info,
    Keyboard,
    KeyRound,
    LayoutDashboard,
    LayoutGrid,
    Link,
    List,
    ListChecks,
    ListTodo,
    Lock,
    LogIn,
    LogOut,
    MessageCircleQuestionMark,
    MessageSquare,
    MessageSquarePlus,
    MessagesSquare,
    Monitor,
    Moon,
    Navigation,
    Network,
    Palette,
    PanelLeftClose,
    PanelLeftOpen,
    Paperclip,
    Pause,
    Pencil,
    Pin,
    PinOff,
    Play,
    Plug,
    PlugZap,
    Plus,
    Power,
    PowerOff,
    QrCode,
    Redo2,
    RefreshCw,
    RotateCcw,
    Scissors,
    ScrollText,
    Search,
    SearchX,
    SendHorizontal,
    Settings,
    Settings2,
    SlidersHorizontal,
    Sparkles,
    Square,
    SquareSlash,
    SquareTerminal,
    Sun,
    Terminal,
    TextWrap,
    Ticket,
    Trash,
    TriangleAlert,
    Type,
    Undo2,
    Video,
    Wand,
    WandSparkles,
    Waypoints,
    WindowClose,
    WindowMaximize,
    WindowMinimize,
    WindowRestore,
    Wrench,
    X,
    Zap,
    // Used internally by gpui-kit / gpui-component widgets.
    ArrowDown,
    ArrowUp,
    Asterisk,
    Ban,
    BookOpen,
    Calendar,
    CaseSensitive,
    CircleX,
    Close,
    EyeOff,
    Github,
    Inspector,
    Loader,
    LoaderCircle,
    Maximize,
    Mic,
    Minimize,
    Minus,
    PanelBottom,
    PanelBottomOpen,
    PanelLeft,
    PanelRight,
    PanelRightClose,
    PanelRightOpen,
    Replace,
    SortAscending,
    SortDescending,
    Star,
    StarFill,
    User,
);

/// Browser entry point. `wasm-bindgen` calls this when the module loads.
#[cfg(target_family = "wasm")]
mod web {
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen(start)]
    pub fn start() {
        gpui_kit::platform::web_init();
        // Single-threaded: works on any static server, no COOP/COEP headers.
        let app = gpui_kit::platform::single_threaded_web()
            .with_assets(super::AppAssets(super::WebIcons));
        super::run(app, false);
    }
}

#[cfg(test)]
mod web_icon_tests {
    /// Every `IconName::…` the app's source names is embedded in the web
    /// build: an icon missing from `web_icons!` draws nothing there.
    #[test]
    fn web_icons_cover_the_app() {
        fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    sources(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        sources(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut missing = std::collections::BTreeSet::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            for (at, _) in text.match_indices("IconName::") {
                let name: String = text[at + "IconName::".len()..]
                    .chars()
                    .take_while(char::is_ascii_alphanumeric)
                    .collect();
                if name.starts_with(|c: char| c.is_ascii_uppercase())
                    && !super::WEB_ICONS.contains(&name.as_str())
                {
                    missing.insert(name);
                }
            }
        }
        assert!(
            missing.is_empty(),
            "add to web_icons! in src/main.rs: {missing:?}"
        );
    }
}
