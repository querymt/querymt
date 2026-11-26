//! Scene files: a clip as data (OpenSpec change `add-gpui-film-scenes`,
//! design D2). TOML in, a validated and timed list of steps out.

use serde::Deserialize;
use serde_json::Value;

pub(super) const DEFAULT_FPS: u32 = 30;
pub(super) const DEFAULT_TYPE_CPS: f64 = 18.0;
pub(super) const DEFAULT_STREAM_WPS: f64 = 9.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Theme {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum PanelMode {
    /// The panel is its own alpha track (`panel.webm`).
    Track,
    /// The panel is drawn over the main window into `main.mp4`.
    Composite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum WorldKind {
    #[default]
    Fixture,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum WindowName {
    #[default]
    Main,
    Panel,
}

impl WindowName {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            WindowName::Main => "main",
            WindowName::Panel => "panel",
        }
    }
}

/// Where the panel sits in the main canvas (composite mode, and a hint for
/// the video tool): `x` is the centre as a fraction of the width, `bottom`
/// the gap under the panel as a fraction of the height.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Anchor {
    #[serde(default = "half")]
    pub x: f64,
    #[serde(default = "default_bottom")]
    pub bottom: f64,
}

fn half() -> f64 {
    0.5
}

fn default_bottom() -> f64 {
    0.15
}

impl Default for Anchor {
    fn default() -> Self {
        Anchor {
            x: half(),
            bottom: default_bottom(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelCfg {
    pub id: String,
    pub label: String,
    pub provider: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionCfg {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default = "idle")]
    pub phase: String,
}

fn idle() -> String {
    "idle".into()
}

/// The session open in the main pane.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OpenSessionCfg {
    pub id: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorldCfg {
    #[serde(default)]
    pub kind: WorldKind,
    /// `kind = "agent"` only; `$QMT_FILM_AGENT` overrides.
    pub address: Option<String>,
    pub agent_title: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelCfg>,
    #[serde(default)]
    pub sessions: Vec<SessionCfg>,
    /// Omitted: the Start page.
    pub session: Option<OpenSessionCfg>,
}

/// Something to point at: an element id, or a point in logical window pixels.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub(super) enum Target {
    Id(String),
    Point {
        #[serde(rename = "at")]
        point: [f32; 2],
    },
}

impl Target {
    /// How the timing file names the target.
    pub(super) fn label(&self) -> String {
        match self {
            Target::Id(id) => id.clone(),
            Target::Point { point } => format!("at[{},{}]", point[0], point[1]),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ScrollSpec {
    pub target: Target,
    #[serde(default)]
    pub dx: f32,
    #[serde(default)]
    pub dy: f32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ToolCall {
    pub id: String,
    pub title: String,
    #[serde(default = "other_kind")]
    pub kind: String,
    #[serde(default = "in_progress")]
    pub status: String,
    pub content: Option<String>,
}

fn other_kind() -> String {
    "other".into()
}

fn in_progress() -> String {
    "in_progress".into()
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NotifySpec {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MarkSpec {
    pub name: String,
    pub target: Target,
}

/// One `[[step]]` as written. Exactly one action key is allowed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawStep {
    pub at: Option<f64>,
    pub after: Option<f64>,
    pub caption: Option<String>,
    pub window: Option<WindowName>,

    pub wait: Option<f64>,
    #[serde(rename = "type")]
    pub type_text: Option<String>,
    pub cps: Option<f64>,
    pub press: Option<String>,
    pub click: Option<Target>,
    pub hover: Option<Target>,
    pub scroll: Option<ScrollSpec>,
    pub quick_open: Option<bool>,
    pub quick_close: Option<bool>,
    pub stream: Option<String>,
    pub wps: Option<f64>,
    pub tool_call: Option<ToolCall>,
    pub state: Option<String>,
    pub end_turn: Option<String>,
    pub notify: Option<NotifySpec>,
    pub mark: Option<MarkSpec>,
    pub end: Option<bool>,
}

/// The file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scene {
    pub version: u32,
    pub name: String,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_size")]
    pub size: [u32; 2],
    #[serde(default = "default_theme")]
    pub theme: Theme,
    #[serde(default = "default_accent")]
    pub accent: String,
    #[serde(default = "default_panel")]
    pub panel: PanelMode,
    #[serde(default)]
    pub anchor: Anchor,
    #[serde(default)]
    pub stills: Vec<u64>,
    #[serde(default)]
    pub world: WorldCfg,
    #[serde(default)]
    pub step: Vec<RawStep>,
}

fn default_fps() -> u32 {
    DEFAULT_FPS
}

fn default_size() -> [u32; 2] {
    [1280, 800]
}

fn default_theme() -> Theme {
    Theme::Dark
}

fn default_accent() -> String {
    "orange".into()
}

fn default_panel() -> PanelMode {
    PanelMode::Track
}

/// What a step does.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Action {
    Wait,
    Type { text: String },
    Press(String),
    Click(Target),
    Hover(Target),
    Scroll(ScrollSpec),
    QuickOpen,
    QuickClose,
    Stream { text: String },
    ToolCall(ToolCall),
    State(String),
    EndTurn(String),
    Notify(NotifySpec),
    Mark(MarkSpec),
    End,
}

impl Action {
    /// Steps that talk to the (fake) agent.
    fn is_agent(&self) -> bool {
        matches!(
            self,
            Action::Stream { .. }
                | Action::ToolCall(_)
                | Action::State(_)
                | Action::EndTurn(_)
                | Action::Notify(_)
        )
    }
}

/// A step with its times resolved.
#[derive(Debug, Clone)]
pub(super) struct Step {
    pub action: Action,
    pub window: WindowName,
    pub caption: Option<String>,
    /// Seconds from the start.
    pub start: f64,
    /// Seconds from the start, when a `wait`, `type` or `stream` ends.
    pub end: f64,
    /// The frame the step starts in.
    pub frame: u64,
    /// How many frames a `type` or `stream` is spread over (at least 1);
    /// 0 for instantaneous steps.
    pub frames: u64,
}

/// A validated scene.
#[derive(Debug, Clone)]
pub(super) struct Resolved {
    pub scene: Scene,
    pub steps: Vec<Step>,
    /// The agent address of a seeded world.
    pub address: Option<String>,
    /// Frames in the clip: the `end` step's frame.
    pub frames: u64,
}

impl Resolved {
    pub(super) fn uses_panel(&self) -> bool {
        self.steps.iter().any(|s| {
            s.window == WindowName::Panel
                || matches!(s.action, Action::QuickOpen | Action::QuickClose)
        })
    }
}

/// Seconds to frames, rounded.
pub(super) fn to_frame(seconds: f64, fps: u32) -> u64 {
    (seconds * f64::from(fps)).round().max(0.0) as u64
}

/// `tokens` spread over `frames` frames, one chunk per frame (empty chunks
/// are frames without input). Every token is in some chunk, in order.
pub(super) fn distribute<S: AsRef<str>>(tokens: &[S], frames: u64) -> Vec<String> {
    let frames = frames.max(1) as usize;
    let mut chunks = vec![String::new(); frames];
    for (i, token) in tokens.iter().enumerate() {
        chunks[i * frames / tokens.len()].push_str(token.as_ref());
    }
    chunks
}

/// Words of `text`, as the speed `wps` counts them.
fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

/// The chunks of a `type` step, per frame.
pub(super) fn type_chunks(text: &str, frames: u64) -> Vec<String> {
    let chars: Vec<String> = text.chars().map(String::from).collect();
    distribute(&chars, frames)
}

/// The chunks of a `stream` step, per frame: whitespace-terminated tokens,
/// so a reply is never cut mid-word.
pub(super) fn stream_chunks(text: &str, frames: u64) -> Vec<String> {
    let tokens: Vec<&str> = text.split_inclusive(char::is_whitespace).collect();
    distribute(&tokens, frames)
}

/// Parse and validate a scene. `agent_env` is `$QMT_FILM_AGENT`. Errors name
/// the step index and the key.
pub(super) fn parse(text: &str, agent_env: Option<&str>) -> Result<Resolved, String> {
    let scene: Scene = toml::from_str(text).map_err(|e| format!("scene file: {e}"))?;
    resolve(scene, agent_env)
}

fn step_err(index: usize, key: &str, message: impl std::fmt::Display) -> String {
    format!("step {index}: `{key}`: {message}")
}

fn action_of(index: usize, raw: &RawStep) -> Result<(&'static str, Action, f64), String> {
    let mut found: Vec<(&'static str, Action, f64)> = Vec::new();
    let positive = |key: &str, v: f64| {
        if v.is_finite() && v > 0.0 {
            Ok(v)
        } else {
            Err(step_err(index, key, "must be greater than 0"))
        }
    };
    let truthy = |key: &str, v: bool| {
        if v {
            Ok(())
        } else {
            Err(step_err(index, key, "must be true"))
        }
    };
    if let Some(v) = raw.wait {
        if !v.is_finite() || v < 0.0 {
            return Err(step_err(index, "wait", "must be 0 or more seconds"));
        }
        found.push(("wait", Action::Wait, v));
    }
    if let Some(text) = &raw.type_text {
        let cps = positive("cps", raw.cps.unwrap_or(DEFAULT_TYPE_CPS))?;
        let secs = text.chars().count() as f64 / cps;
        found.push(("type", Action::Type { text: text.clone() }, secs));
    } else if raw.cps.is_some() {
        return Err(step_err(index, "cps", "only goes with `type`"));
    }
    if let Some(key) = &raw.press {
        found.push(("press", Action::Press(key.clone()), 0.0));
    }
    if let Some(t) = &raw.click {
        found.push(("click", Action::Click(t.clone()), 0.0));
    }
    if let Some(t) = &raw.hover {
        found.push(("hover", Action::Hover(t.clone()), 0.0));
    }
    if let Some(s) = &raw.scroll {
        found.push(("scroll", Action::Scroll(s.clone()), 0.0));
    }
    if let Some(v) = raw.quick_open {
        truthy("quick_open", v)?;
        found.push(("quick_open", Action::QuickOpen, 0.0));
    }
    if let Some(v) = raw.quick_close {
        truthy("quick_close", v)?;
        found.push(("quick_close", Action::QuickClose, 0.0));
    }
    if let Some(text) = &raw.stream {
        let wps = positive("wps", raw.wps.unwrap_or(DEFAULT_STREAM_WPS))?;
        let secs = word_count(text) as f64 / wps;
        found.push(("stream", Action::Stream { text: text.clone() }, secs));
    } else if raw.wps.is_some() {
        return Err(step_err(index, "wps", "only goes with `stream`"));
    }
    if let Some(t) = &raw.tool_call {
        found.push(("tool_call", Action::ToolCall(t.clone()), 0.0));
    }
    if let Some(s) = &raw.state {
        if s != "running" && s != "idle" {
            return Err(step_err(index, "state", "must be \"running\" or \"idle\""));
        }
        found.push(("state", Action::State(s.clone()), 0.0));
    }
    if let Some(s) = &raw.end_turn {
        found.push(("end_turn", Action::EndTurn(s.clone()), 0.0));
    }
    if let Some(n) = &raw.notify {
        found.push(("notify", Action::Notify(n.clone()), 0.0));
    }
    if let Some(m) = &raw.mark {
        found.push(("mark", Action::Mark(m.clone()), 0.0));
    }
    if let Some(v) = raw.end {
        truthy("end", v)?;
        found.push(("end", Action::End, 0.0));
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(step_err(index, "action", "the step has no action")),
        _ => {
            let keys: Vec<&str> = found.iter().map(|(k, _, _)| *k).collect();
            Err(step_err(
                index,
                keys[1],
                format!("only one action per step, but also has `{}`", keys[0]),
            ))
        }
    }
}

fn resolve(scene: Scene, agent_env: Option<&str>) -> Result<Resolved, String> {
    if scene.version != 1 {
        return Err(format!(
            "scene file: `version` is {}, only 1 is supported",
            scene.version
        ));
    }
    if scene.name.is_empty()
        || !scene
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("scene file: `name` must be a file stem (letters, digits, - and _)".into());
    }
    if scene.fps == 0 {
        return Err("scene file: `fps` must be greater than 0".into());
    }
    if scene.size[0] == 0 || scene.size[1] == 0 {
        return Err("scene file: `size` must be greater than 0".into());
    }
    let address = match scene.world.kind {
        WorldKind::Agent => Some(
            agent_env
                .map(str::to_owned)
                .or_else(|| scene.world.address.clone())
                .ok_or("world: `kind = \"agent\"` needs `address` or $QMT_FILM_AGENT")?,
        ),
        WorldKind::Fixture => None,
    };
    if scene.step.is_empty() {
        return Err("scene file: there are no `[[step]]`; an `end` step is required".into());
    }

    let mut steps: Vec<Step> = Vec::with_capacity(scene.step.len());
    let mut prev_start = 0.0_f64;
    let mut prev_end = 0.0_f64;
    let mut panel_open = false;
    let last = scene.step.len() - 1;
    for (index, raw) in scene.step.iter().enumerate() {
        let (key, action, duration) = action_of(index, raw)?;
        let start = match (raw.at, raw.after) {
            (Some(_), Some(_)) => {
                return Err(step_err(index, "after", "use `at` or `after`, not both"));
            }
            (Some(at), None) => {
                if !at.is_finite() || at < 0.0 {
                    return Err(step_err(index, "at", "must be 0 or more seconds"));
                }
                at
            }
            (None, Some(after)) => {
                if !after.is_finite() || after < 0.0 {
                    return Err(step_err(index, "after", "must be 0 or more seconds"));
                }
                prev_end + after
            }
            (None, None) => return Err(step_err(index, "at", "needs `at` or `after`")),
        };
        if start + 1e-9 < prev_start {
            return Err(step_err(
                index,
                if raw.at.is_some() { "at" } else { "after" },
                format!("time {start:.3} s is before the previous step ({prev_start:.3} s)"),
            ));
        }
        if matches!(action, Action::End) != (index == last) {
            return Err(if index == last {
                step_err(index, "end", "the last step must be `end = true`")
            } else {
                step_err(index, "end", "`end` must be the last step")
            });
        }
        let window = raw.window.unwrap_or_default();
        match action {
            Action::QuickOpen => panel_open = true,
            Action::QuickClose => {
                if !panel_open {
                    return Err(step_err(index, key, "the panel is not open"));
                }
                panel_open = false;
            }
            _ if window == WindowName::Panel && !panel_open => {
                return Err(step_err(
                    index,
                    "window",
                    "`panel` needs an earlier `quick_open` and no `quick_close` since",
                ));
            }
            _ => {}
        }
        if scene.world.kind == WorldKind::Agent && action.is_agent() {
            return Err(step_err(
                index,
                key,
                "agent steps are refused in a seeded world (`kind = \"agent\"`)",
            ));
        }
        let end = start + duration;
        let frames = match action {
            Action::Type { .. } | Action::Stream { .. } => {
                (to_frame(end, scene.fps) - to_frame(start, scene.fps)).max(1)
            }
            _ => 0,
        };
        prev_start = start;
        prev_end = end;
        steps.push(Step {
            action,
            window,
            caption: raw.caption.clone(),
            start,
            end,
            frame: to_frame(start, scene.fps),
            frames,
        });
    }
    let frames = steps.last().map(|s| s.frame).unwrap_or(0);
    if frames == 0 {
        return Err(step_err(
            last,
            "end",
            "the clip must last at least one frame",
        ));
    }
    if let Some(still) = scene.stills.iter().find(|f| **f >= frames) {
        return Err(format!(
            "scene file: `stills` has frame {still}, but the clip has {frames} frames"
        ));
    }
    Ok(Resolved {
        scene,
        steps,
        address,
        frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
version = 1
name = "demo"
fps = 30
stills = [0, 60]

[world]
kind = "fixture"
cwd = "/w"
models = [{ id = "a:b", label = "AB", provider = "a" }]
sessions = [{ id = "s-1", title = "One" }]
session = { id = "film", title = "Film" }

[[step]]
at = 0.3
type = "hello world"
cps = 10
caption = "Ask"

[[step]]
after = 0.2
press = "enter"

[[step]]
after = 0.5
stream = "one two three four"
wps = 4

[[step]]
after = 0.1
click = "composer-input"

[[step]]
after = 0.1
click = { at = [10, 20.5] }

[[step]]
at = 4.0
end = true
"#;

    fn err(text: &str) -> String {
        parse(text, None).expect_err("the scene must be refused")
    }

    /// `body` as the steps of an otherwise valid scene.
    fn with_steps(body: &str) -> String {
        format!("version = 1\nname = \"t\"\n{body}")
    }

    #[test]
    fn a_valid_scene_resolves_times() {
        let r = parse(VALID, None).expect("valid");
        assert_eq!(r.frames, 120);
        assert_eq!(r.steps.len(), 6);
        // `type`: 11 chars at 10 cps = 1.1 s, from 0.3 s.
        let t = &r.steps[0];
        assert_eq!((t.frame, t.frames), (9, 33));
        assert!((t.end - 1.4).abs() < 1e-9);
        assert_eq!(t.caption.as_deref(), Some("Ask"));
        // `after` counts from the end of the previous step: 1.4 + 0.2.
        assert!((r.steps[1].start - 1.6).abs() < 1e-9);
        assert_eq!(r.steps[1].frame, 48);
        // `stream`: 4 words at 4 wps = 1 s, from 2.1 s.
        let s = &r.steps[2];
        assert!((s.start - 2.1).abs() < 1e-9);
        assert_eq!((s.frame, s.frames), (63, 30));
        assert_eq!(
            r.steps[3].action,
            Action::Click(Target::Id("composer-input".into()))
        );
        assert_eq!(
            r.steps[4].action,
            Action::Click(Target::Point {
                point: [10.0, 20.5]
            })
        );
        assert_eq!(r.steps[5].action, Action::End);
        assert!(!r.uses_panel());
    }

    #[test]
    fn durations_round_to_whole_frames() {
        // 7 chars at 10 cps = 0.7 s, from 0.3 s (frame 9) to 1.0 s (frame 30).
        let r = parse(
            &with_steps(
                "[[step]]\nat = 0.3\ntype = \"abcdefg\"\ncps = 10\n[[step]]\nat = 2.0\nend = true",
            ),
            None,
        )
        .unwrap();
        let t = &r.steps[0];
        assert_eq!(t.frame, 9);
        assert_eq!(t.frames, 21);
        // Never less than one frame.
        let r = parse(
            &with_steps(
                "[[step]]\nat = 0.0\ntype = \"a\"\ncps = 1000\n[[step]]\nat = 1.0\nend = true",
            ),
            None,
        )
        .unwrap();
        assert_eq!(r.steps[0].frames, 1);
    }

    #[test]
    fn chunks_cover_the_text_in_order() {
        let chunks = type_chunks("hello", 3);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), "hello");
        let reply = "## Title\n\nOne two\nthree";
        let chunks = stream_chunks(reply, 4);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks.concat(), reply);
        // Cut at whitespace only.
        assert!(chunks.iter().all(|c| c.is_empty()
            || c.ends_with(char::is_whitespace)
            || reply.ends_with(c.as_str())));
        // More frames than tokens: some frames carry nothing.
        let chunks = type_chunks("ab", 10);
        assert_eq!(chunks.iter().filter(|c| !c.is_empty()).count(), 2);
        assert_eq!(chunks.concat(), "ab");
    }

    #[test]
    fn times_must_not_go_back() {
        let e = err(&with_steps(
            "[[step]]\nat = 1.0\nwait = 0.1\n[[step]]\nat = 0.5\npress = \"a\"\n[[step]]\nat = 2.0\nend = true",
        ));
        assert!(e.starts_with("step 1: `at`"), "{e}");
    }

    #[test]
    fn a_step_has_exactly_one_action() {
        let e = err(&with_steps(
            "[[step]]\nat = 0.1\npress = \"a\"\nclick = \"x\"\n[[step]]\nat = 2.0\nend = true",
        ));
        assert!(e.starts_with("step 0: `click`"), "{e}");
        let e = err(&with_steps(
            "[[step]]\nat = 0.1\ncaption = \"x\"\n[[step]]\nat = 2.0\nend = true",
        ));
        assert!(e.starts_with("step 0: `action`"), "{e}");
    }

    #[test]
    fn steps_need_a_time() {
        let e = err(&with_steps(
            "[[step]]\npress = \"a\"\n[[step]]\nat = 2.0\nend = true",
        ));
        assert!(e.starts_with("step 0: `at`"), "{e}");
        let e = err(&with_steps(
            "[[step]]\nat = 1.0\nafter = 1.0\npress = \"a\"\n[[step]]\nat = 2.0\nend = true",
        ));
        assert!(e.starts_with("step 0: `after`"), "{e}");
    }

    #[test]
    fn end_is_required_and_last() {
        let e = err(&with_steps("[[step]]\nat = 0.1\npress = \"a\""));
        assert!(e.starts_with("step 0: `end`"), "{e}");
        let e = err(&with_steps(
            "[[step]]\nat = 1.0\nend = true\n[[step]]\nat = 2.0\npress = \"a\"",
        ));
        assert!(e.starts_with("step 0: `end`"), "{e}");
        let e = err(&with_steps(""));
        assert!(e.contains("`end`"), "{e}");
    }

    #[test]
    fn a_seeded_world_needs_an_address_and_refuses_agent_steps() {
        let seeded = "[world]\nkind = \"agent\"\n";
        let end = "[[step]]\nat = 2.0\nend = true";
        let e = err(&with_steps(&format!("{seeded}\n{end}")));
        assert!(e.contains("address"), "{e}");
        // `$QMT_FILM_AGENT` is enough.
        let r = parse(&with_steps(&format!("{seeded}\n{end}")), Some("h:1")).unwrap();
        assert_eq!(r.address.as_deref(), Some("h:1"));
        // The env var wins over the file.
        let r = parse(
            &with_steps(&format!(
                "[world]\nkind = \"agent\"\naddress = \"f:2\"\n{end}"
            )),
            Some("h:1"),
        )
        .unwrap();
        assert_eq!(r.address.as_deref(), Some("h:1"));
        let e = parse(
            &with_steps(&format!(
                "[world]\nkind = \"agent\"\naddress = \"f:2\"\n[[step]]\nat = 0.1\nstream = \"hi\"\n{end}"
            )),
            None,
        )
        .expect_err("refused");
        assert!(e.starts_with("step 0: `stream`"), "{e}");
    }

    #[test]
    fn panel_steps_need_an_open_panel() {
        let end = "[[step]]\nat = 3.0\nend = true";
        let e = err(&with_steps(&format!(
            "[[step]]\nat = 0.1\nwindow = \"panel\"\ntype = \"x\"\n{end}"
        )));
        assert!(e.starts_with("step 0: `window`"), "{e}");
        let r = parse(
            &with_steps(&format!(
                "[[step]]\nat = 0.1\nquick_open = true\n[[step]]\nat = 0.2\nwindow = \"panel\"\ntype = \"x\"\n[[step]]\nat = 1.0\nquick_close = true\n{end}"
            )),
            None,
        )
        .unwrap();
        assert!(r.uses_panel());
        // After `quick_close` the panel is gone again.
        let e = err(&with_steps(&format!(
            "[[step]]\nat = 0.1\nquick_open = true\n[[step]]\nat = 0.5\nquick_close = true\n[[step]]\nat = 1.0\nwindow = \"panel\"\npress = \"a\"\n{end}"
        )));
        assert!(e.starts_with("step 2: `window`"), "{e}");
        let e = err(&with_steps(&format!(
            "[[step]]\nat = 0.5\nquick_close = true\n{end}"
        )));
        assert!(e.starts_with("step 0: `quick_close`"), "{e}");
    }

    #[test]
    fn speeds_and_unknown_keys_are_checked() {
        let end = "[[step]]\nat = 3.0\nend = true";
        let e = err(&with_steps(&format!(
            "[[step]]\nat = 0.1\ntype = \"x\"\ncps = 0\n{end}"
        )));
        assert!(e.starts_with("step 0: `cps`"), "{e}");
        let e = err(&with_steps(&format!(
            "[[step]]\nat = 0.1\npress = \"a\"\nwps = 3\n{end}"
        )));
        assert!(e.starts_with("step 0: `wps`"), "{e}");
        let e = err(&with_steps(&format!(
            "[[step]]\nat = 0.1\nprees = \"a\"\n{end}"
        )));
        assert!(e.contains("prees"), "{e}");
        let e = err(&with_steps(&format!(
            "[[step]]\nat = 0.1\nstate = \"busy\"\n{end}"
        )));
        assert!(e.starts_with("step 0: `state`"), "{e}");
    }

    #[test]
    fn settings_are_checked() {
        let end = "[[step]]\nat = 1.0\nend = true";
        let e = err(&format!("version = 2\nname = \"t\"\n{end}"));
        assert!(e.contains("version"), "{e}");
        let e = err(&format!("version = 1\nname = \"a/b\"\n{end}"));
        assert!(e.contains("name"), "{e}");
        let e = err(&format!("version = 1\nname = \"t\"\nstills = [99]\n{end}"));
        assert!(e.contains("stills"), "{e}");
        let e = err(&format!("version = 1\nname = \"t\"\nfps = 0\n{end}"));
        assert!(e.contains("fps"), "{e}");
    }

    #[test]
    fn notify_and_tool_call_parse() {
        let r = parse(
            &with_steps(
                r#"
[[step]]
at = 0.1
tool_call = { id = "t1", title = "Read src/lib.rs", kind = "read", status = "completed", content = "ok" }

[[step]]
at = 0.2
notify = { method = "session/update", params = { sessionId = "s", n = 3 } }

[[step]]
at = 0.3
mark = { name = "reply", target = "composer-input" }

[[step]]
at = 0.4
scroll = { target = "transcript", dy = -200 }

[[step]]
at = 1.0
end = true
"#,
            ),
            None,
        )
        .unwrap();
        let Action::ToolCall(t) = &r.steps[0].action else {
            panic!("tool_call");
        };
        assert_eq!((t.kind.as_str(), t.status.as_str()), ("read", "completed"));
        let Action::Notify(n) = &r.steps[1].action else {
            panic!("notify");
        };
        assert_eq!(n.params["n"], 3);
        assert!(matches!(&r.steps[2].action, Action::Mark(m) if m.name == "reply"));
        assert!(matches!(&r.steps[3].action, Action::Scroll(s) if s.dy == -200.0));
    }
}
