//! Interface and code typefaces: where each field comes from (the user's
//! setting, the theme file, the app default), which families may be used
//! at all, and the list of installed families the settings pickers show.
//!
//! GPUI panics when it lays out text in a family that is not installed, so a
//! family from the preferences or from a theme file is only applied once it
//! is known to exist ([`resolve`]); otherwise the next source wins.

use std::{collections::HashSet, sync::Arc};

use gpui_kit::{App, Global, SharedString, TextSystem, font, px};

/// The bundled proportional family, the app default for the interface.
pub const DEFAULT_UI_FAMILY: &str = "IBM Plex Sans";
/// The bundled monospace family, the app default for code.
pub const DEFAULT_MONO_FAMILY: &str = "IBM Plex Mono";
/// The interface size the design was authored at (the window's rem size).
pub const DEFAULT_UI_SIZE: f32 = 16.;
/// The code size the design was authored at; mono text sites scale by
/// `mono size / DEFAULT_MONO_SIZE`.
pub const DEFAULT_MONO_SIZE: f32 = 13.;
/// Bounds of the interface size, in px.
pub const UI_SIZE_RANGE: (f32, f32) = (10., 32.);
/// Bounds of the code size, in px.
pub const MONO_SIZE_RANGE: (f32, f32) = (8., 32.);

/// Families that ship inside the app (`main::install_fonts`): always valid.
pub const BUNDLED_FAMILIES: [&str; 2] = [DEFAULT_UI_FAMILY, DEFAULT_MONO_FAMILY];

fn bundled(family: &str) -> bool {
    BUNDLED_FAMILIES.contains(&family)
}

/// The user's choices (`Prefs`); `None` leaves the field to the theme.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FontPrefs {
    pub ui_family: Option<String>,
    pub ui_size: Option<f32>,
    pub mono_family: Option<String>,
    pub mono_size: Option<f32>,
}

/// What a theme file says about type (`font.family`, `font.size`,
/// `mono_font.family`, `mono_font.size`); `None` where it is silent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThemeFonts {
    pub ui_family: Option<SharedString>,
    pub ui_size: Option<f32>,
    pub mono_family: Option<SharedString>,
    pub mono_size: Option<f32>,
}

impl ThemeFonts {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The four values in force after applying the precedence.
#[derive(Clone, Debug, PartialEq)]
pub struct Typography {
    pub ui_family: SharedString,
    pub ui_size: f32,
    pub mono_family: SharedString,
    pub mono_size: f32,
}

impl Default for Typography {
    fn default() -> Self {
        Self {
            ui_family: DEFAULT_UI_FAMILY.into(),
            ui_size: DEFAULT_UI_SIZE,
            mono_family: DEFAULT_MONO_FAMILY.into(),
            mono_size: DEFAULT_MONO_SIZE,
        }
    }
}

/// `value` held to `range`; `None` for NaN and infinities.
pub fn clamp_size(value: f32, range: (f32, f32)) -> Option<f32> {
    value.is_finite().then(|| value.clamp(range.0, range.1))
}

fn pick_size(user: Option<f32>, theme: Option<f32>, default: f32, range: (f32, f32)) -> f32 {
    user.and_then(|v| clamp_size(v, range))
        .or_else(|| theme.and_then(|v| clamp_size(v, range)))
        .unwrap_or(default)
}

/// The family to use: the user's if `usable`, else the theme's if `usable`,
/// else `default`. Each rejected family is logged when `warn`.
fn pick_family(
    user: Option<&str>,
    theme: Option<&str>,
    default: &str,
    field: &str,
    usable: &mut dyn FnMut(&str) -> bool,
    warn: bool,
) -> SharedString {
    for (source, name) in [("preference", user), ("theme", theme)] {
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            continue;
        };
        if bundled(name) || usable(name) {
            return name.to_string().into();
        }
        if !warn {
            continue;
        }
        tracing::warn!(
            family = name,
            field,
            source,
            "font family is not installed; using the next choice"
        );
    }
    default.to_string().into()
}

/// The precedence, per field: user setting, then theme file, then app
/// default. `installed` says whether a non-bundled family exists; it is not
/// asked about the bundled ones or when no family is named.
#[cfg(test)]
pub fn resolve(
    prefs: &FontPrefs,
    theme: &ThemeFonts,
    installed: &mut dyn FnMut(&str) -> bool,
) -> Typography {
    resolve_logged(prefs, theme, installed, true)
}

/// [`resolve`], logging rejected families only when `warn` (applying does;
/// the settings page, which asks on every frame, does not).
fn resolve_logged(
    prefs: &FontPrefs,
    theme: &ThemeFonts,
    installed: &mut dyn FnMut(&str) -> bool,
    warn: bool,
) -> Typography {
    Typography {
        ui_family: pick_family(
            prefs.ui_family.as_deref(),
            theme.ui_family.as_deref(),
            DEFAULT_UI_FAMILY,
            "font.family",
            installed,
            warn,
        ),
        ui_size: pick_size(prefs.ui_size, theme.ui_size, DEFAULT_UI_SIZE, UI_SIZE_RANGE),
        mono_family: pick_family(
            prefs.mono_family.as_deref(),
            theme.mono_family.as_deref(),
            DEFAULT_MONO_FAMILY,
            "mono_font.family",
            installed,
            warn,
        ),
        mono_size: pick_size(
            prefs.mono_size,
            theme.mono_size,
            DEFAULT_MONO_SIZE,
            MONO_SIZE_RANGE,
        ),
    }
}

/// [`resolve`] against the families installed on this machine. `warn`: log
/// the families skipped.
pub fn resolve_installed(
    prefs: &FontPrefs,
    theme: &ThemeFonts,
    warn: bool,
    cx: &mut App,
) -> Typography {
    let wanted = |f: &Option<String>| f.as_deref().is_some_and(|f| !bundled(f.trim()));
    let theme_wanted = |f: &Option<SharedString>| f.as_deref().is_some_and(|f| !bundled(f.trim()));
    // Only enumerate the system's families when a non-bundled one is named.
    if !(wanted(&prefs.ui_family)
        || wanted(&prefs.mono_family)
        || theme_wanted(&theme.ui_family)
        || theme_wanted(&theme.mono_family))
    {
        return resolve_logged(prefs, theme, &mut |_| false, warn);
    }
    let installed = FontCatalog::installed(cx);
    resolve_logged(prefs, theme, &mut |name| installed.contains(name), warn)
}

/// The families on this machine.
#[derive(Debug, Default)]
pub struct Installed {
    /// Sorted case-insensitively, the bundled families included.
    families: Vec<SharedString>,
    set: HashSet<SharedString>,
    /// Families whose `i` and `W` have the same advance. `None` until the
    /// background pass has finished.
    mono: Option<HashSet<SharedString>>,
}

impl Installed {
    pub fn new(names: impl IntoIterator<Item = String>) -> Self {
        let mut families: Vec<SharedString> = names
            .into_iter()
            .map(|n| n.trim().to_string())
            // Private families (`.SystemUIFont`, `.AppleSystemUIFont`) are
            // not picks.
            .filter(|n| !n.is_empty() && !n.starts_with('.'))
            .chain(BUNDLED_FAMILIES.iter().map(|b| b.to_string()))
            .map(SharedString::from)
            .collect();
        families.sort_by_key(|f| f.to_lowercase());
        families.dedup();
        let set = families.iter().cloned().collect();
        Self {
            families,
            set,
            mono: None,
        }
    }

    pub fn contains(&self, family: &str) -> bool {
        self.set.contains(family)
    }

    /// Every family, sorted.
    pub fn families(&self) -> &[SharedString] {
        &self.families
    }

    /// Whether the monospace pass has finished (the code picker is only
    /// complete then).
    pub fn mono_ready(&self) -> bool {
        self.mono.is_some()
    }

    pub fn is_mono(&self, family: &str) -> bool {
        bundled(family) && family == DEFAULT_MONO_FAMILY
            || self.mono.as_ref().is_some_and(|m| m.contains(family))
    }

    /// The monospace families, sorted.
    pub fn mono_families(&self) -> Vec<SharedString> {
        self.families
            .iter()
            .filter(|f| self.is_mono(f))
            .cloned()
            .collect()
    }
}

/// Whether `family` is monospace: `i` and `W` advance alike. A family GPUI
/// cannot load is not.
pub fn is_monospace(system: &TextSystem, family: &str) -> bool {
    // `resolve_font` panics when neither the family nor any fallback loads;
    // one broken family must not take the pass down.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let id = system.resolve_font(&font(family.to_string()));
        let size = px(16.);
        match (system.advance(id, size, 'i'), system.advance(id, size, 'W')) {
            (Ok(i), Ok(w)) => (f32::from(i.width) - f32::from(w.width)).abs() < 0.01,
            _ => false,
        }
    }))
    .unwrap_or(false)
}

/// The installed families, found once per process and kept as a global.
/// Enumerating them takes about 100 ms on macOS, so the settings page starts
/// [`FontCatalog::warm`] and shows a loading row until it is done; only a
/// non-bundled family named by the preferences or a theme makes
/// [`FontCatalog::installed`] do it on the spot.
#[derive(Default)]
pub struct FontCatalog {
    installed: Option<Arc<Installed>>,
    warming: bool,
}

impl Global for FontCatalog {}

impl FontCatalog {
    fn state(cx: &mut App) -> &mut FontCatalog {
        if !cx.has_global::<FontCatalog>() {
            cx.set_global(FontCatalog::default());
        }
        cx.global_mut::<FontCatalog>()
    }

    /// The families, enumerated now when that has not happened yet. On wasm
    /// the system's fonts cannot be listed: only the bundled ones.
    pub fn installed(cx: &mut App) -> Arc<Installed> {
        if let Some(found) = Self::state(cx).installed.clone() {
            return found;
        }
        let found = Arc::new(Installed::new(list_system_families(cx.text_system())));
        Self::state(cx).installed = Some(found.clone());
        found
    }

    /// What is known so far without any work (the picker's render path).
    pub fn peek(cx: &mut App) -> Option<Arc<Installed>> {
        Self::state(cx).installed.clone()
    }

    /// Enumerate the families and detect the monospace ones off the main
    /// thread, once. Windows are refreshed after each step.
    pub fn warm(cx: &mut App) {
        let state = Self::state(cx);
        if state.warming || state.mono_done() {
            return;
        }
        state.warming = true;
        if cfg!(target_family = "wasm") {
            // The system's fonts cannot be listed: the bundled families only.
            let mut installed = Installed::new([]);
            installed.mono = Some(HashSet::from([SharedString::from(DEFAULT_MONO_FAMILY)]));
            Self::state(cx).installed = Some(Arc::new(installed));
            return;
        }
        let system = cx.text_system().clone();
        // Listed already when a saved or theme family needed checking.
        let known = Self::state(cx).installed.clone();
        cx.spawn(async move |cx| {
            let found = match known {
                Some(found) => found,
                None => {
                    let names = {
                        let system = system.clone();
                        cx.background_executor()
                            .spawn(async move { list_system_families(&system) })
                            .await
                    };
                    cx.update(|cx| {
                        let state = Self::state(cx);
                        let found = state
                            .installed
                            .clone()
                            .unwrap_or_else(|| Arc::new(Installed::new(names)));
                        state.installed = Some(found.clone());
                        cx.refresh_windows();
                        found
                    })
                }
            };
            let families: Vec<SharedString> = found.families().to_vec();
            let mono = cx
                .background_executor()
                .spawn(async move {
                    families
                        .into_iter()
                        .filter(|f| is_monospace(&system, f))
                        .collect::<HashSet<_>>()
                })
                .await;
            cx.update(|cx| {
                Self::state(cx).installed = Some(Arc::new(Installed {
                    families: found.families.clone(),
                    set: found.set.clone(),
                    mono: Some(mono),
                }));
                cx.refresh_windows();
            });
        })
        .detach();
    }

    /// Test seam: the families the machine "has".
    #[cfg(test)]
    pub fn seed(cx: &mut App, names: &[&str], mono: &[&str]) {
        let mut installed = Installed::new(names.iter().map(|n| n.to_string()));
        installed.mono = Some(
            mono.iter()
                .map(|m| SharedString::from(m.to_string()))
                .collect(),
        );
        let state = Self::state(cx);
        state.installed = Some(Arc::new(installed));
        state.warming = true;
    }

    fn mono_done(&self) -> bool {
        self.installed.as_ref().is_some_and(|i| i.mono_ready())
    }
}

fn list_system_families(system: &TextSystem) -> Vec<String> {
    if cfg!(target_family = "wasm") {
        return Vec::new();
    }
    system.all_font_names()
}
