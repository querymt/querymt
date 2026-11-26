//! Spike: film the real app offscreen, frame by frame (OpenSpec change
//! `spike-gpui-demo-film`). Test-only; the film test is ignored.
//!
//! Run: `cargo nextest run -p querymt-gpui --bins --run-ignored only
//! -E 'test(/film_prompt_and_streamed_reply/)' --no-capture`

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, HeadlessAppContext, WindowBounds, WindowOptions, bounds,
    point, px, size,
};
use serde_json::json;

use crate::session::SessionView;
use crate::ui::QmtApp;

const FPS: u64 = 30;
const SECONDS: u64 = 5;
const FRAMES: u64 = FPS * SECONDS;
/// The agent the seeded takes connect to: `$QMT_FILM_AGENT`, else a qmtcode
/// listening on `localhost:3030`.
const DEFAULT_AGENT_ADDRESS: &str = "localhost:3030";
/// How long a seeded take waits for the agent's sessions and models.
const SEED_TIMEOUT: Duration = Duration::from_secs(20);
const AGENT: &str = "test-agent";
const SESSION: &str = "film";
const RUN: &str = "r1";

const PROMPT: &str = "How do I stream a reply into a GPUI view?";
const REPLY: &str = "## Streaming a reply\n\n\
Each chunk the agent sends is applied to the transcript as it arrives:\n\n\
- the store queues the notification\n\
- a short timer flushes the queue into the view\n\
- the view re-renders only the rows that changed\n\n\
```rust\n\
fn on_chunk(view: &mut SessionView, text: &str) {\n    view.append(text);\n}\n```\n\n\
That is all it takes.";

/// Where frames and the clip go: `$QMT_FILM_OUT`, else `target/film/`.
fn out_dir() -> PathBuf {
    let dir = std::env::var_os("QMT_FILM_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/film"));
    std::fs::create_dir_all(&dir).expect("create the film output directory");
    dir
}

/// One captured frame: RGBA8 pixels, row by row.
struct Frame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn write_png(path: &Path, frame: &Frame) {
    let file = std::fs::File::create(path).expect("create png");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&frame.rgba).expect("png data");
}

/// What happens at the start of frame `n`: notifications for the store.
fn steps(n: u64, chunks: &[String]) -> Vec<(&'static str, serde_json::Value)> {
    let update = |update: serde_json::Value| {
        (
            "session/update",
            json!({ "sessionId": SESSION, "update": update,
                    "_meta": { "querymt": { "runId": RUN } } }),
        )
    };
    let state = |state: &str| {
        (
            "_querymt/session/stateUpdate",
            json!({ "sessionId": SESSION, "state": state, "runId": RUN,
                    "stopReason": "end_turn" }),
        )
    };
    match n {
        // 0.3 s: the user prompt.
        9 => vec![update(json!({ "sessionUpdate": "user_message_chunk",
            "messageId": "u1", "content": { "type": "text", "text": PROMPT } }))],
        // 0.8 s: the run starts, so the spinner shows.
        24 => vec![state("running")],
        // 1.0 s .. 4.0 s: the reply streams in.
        30..120 => {
            let chunk = &chunks[(n - 30) as usize];
            if chunk.is_empty() {
                Vec::new()
            } else {
                vec![update(json!({ "sessionUpdate": "agent_message_chunk",
                    "messageId": "a1", "content": { "type": "text", "text": chunk } }))]
            }
        }
        // 4.0 s: the turn ends.
        120 => vec![state("idle")],
        _ => Vec::new(),
    }
}

/// The reply cut at whitespace into one chunk per streaming frame (90).
fn reply_chunks() -> Vec<String> {
    let tokens: Vec<&str> = REPLY.split_inclusive(char::is_whitespace).collect();
    let frames = 90;
    let mut chunks = vec![String::new(); frames];
    for (i, token) in tokens.iter().enumerate() {
        chunks[i * frames / tokens.len()].push_str(token);
    }
    chunks
}

struct Film {
    cx: HeadlessAppContext,
    window: AnyWindowHandle,
    store: gpui_kit::Entity<crate::store::AgentsStore>,
}

/// What the app knows before the scene starts.
enum World {
    /// A hand-made connected agent (`peer_fixture`): no sessions, no models.
    Fixture,
    /// A real agent at `host:port`, over WebSocket. Only its session list,
    /// model catalog and capabilities are used; the scene itself is still
    /// scripted, and nothing is sent to the agent's sessions.
    Agent(String),
}

fn agent_address() -> String {
    std::env::var("QMT_FILM_AGENT").unwrap_or_else(|_| DEFAULT_AGENT_ADDRESS.into())
}

/// Connect the store's agent and wait, in wall time, until its session list
/// and model catalog have arrived.
fn seed(cx: &mut HeadlessAppContext, store: &gpui_kit::Entity<crate::store::AgentsStore>) {
    use crate::store::ConnState;
    // The transport's thread wakes app tasks; the test scheduler only allows
    // that once parking is allowed.
    cx.allow_parking();
    let started = Instant::now();
    loop {
        cx.run_until_parked();
        let ready = cx.update(|cx| {
            let store = store.read(cx);
            match store.runtime(AGENT) {
                Some(rt) => match &rt.state {
                    ConnState::Failed(err) => Err(err.clone()),
                    ConnState::Connected => Ok(rt.models_loaded && rt.sessions_loaded),
                    _ => Ok(false),
                },
                None => Ok(false),
            }
        });
        match ready {
            Ok(true) => break,
            Ok(false) => {}
            Err(err) => panic!("the agent at {} failed: {err}", agent_address()),
        }
        assert!(
            started.elapsed() < SEED_TIMEOUT,
            "no sessions and models from {} within {SEED_TIMEOUT:?}",
            agent_address()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // The model info calls that follow the catalog; give them a moment.
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    cx.update(|cx| {
        let store = store.read(cx);
        let rt = store.runtime(AGENT).expect("connected");
        println!(
            "seeded in {:.2?}: agent {} {}, {} sessions, {} models",
            started.elapsed(),
            rt.agent_title.as_deref().unwrap_or("?"),
            rt.agent_version.as_deref().unwrap_or(""),
            rt.sessions.len(),
            rt.models.len()
        );
    });
}

fn open(world: World) -> Film {
    // `current_platform` builds a `MacPlatform`, which panics off the process
    // main thread (a test thread), so macOS takes CoreText directly.
    #[cfg(target_os = "macos")]
    let text: Arc<dyn gpui_kit::PlatformTextSystem> = Arc::new(gpui_apple::AppleTextSystem::new());
    #[cfg(not(target_os = "macos"))]
    let text = gpui_kit::platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text,
        Arc::new(crate::AppAssets(gpui_kit::assets::AllAssets)),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::install_fonts(cx);
        crate::theme::apply(true, "orange", cx);
        crate::ui::bind_keys(cx);
    });
    let store = cx.update(|cx| {
        cx.new(|cx| {
            let mut store = match &world {
                World::Fixture => crate::store::launch::peer_fixture(),
                World::Agent(address) => {
                    let mut store = crate::store::launch::websocket_fixture(address);
                    store.connect(AGENT, cx);
                    store
                }
            };
            // In memory only: the window's appearance is light, and the
            // clip must not depend on the machine.
            store.prefs.dark = Some(true);
            store
        })
    });
    if matches!(world, World::Agent(_)) {
        seed(&mut cx, &store);
    }
    let (window, app) = cx.update(|cx| {
        store.update(cx, |store, cx| {
            let mut view = SessionView::new(AGENT, SESSION, "/workspace", "Film");
            // `$QMT_FILM_MODEL` (a model id), else the agent's first model,
            // so the pill names a real one.
            view.model = std::env::var("QMT_FILM_MODEL")
                .ok()
                .or_else(|| store.models(AGENT).first().map(|m| m.id.clone()));
            store.install_main_for_test(view, cx);
        });
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds(
                point(px(0.), px(0.)),
                size(px(1280.), px(800.)),
            ))),
            show: false,
            focus: false,
            ..Default::default()
        };
        let shared = store.clone();
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| QmtApp::new(shared, window, cx))
        })
        .expect("open the window")
    });
    cx.update_window(window, |_, window, cx| {
        app.update(cx, |app, cx| {
            app.apply_theme(window, cx);
            app.show_session_for_test(window, cx);
        });
    })
    .expect("show the session");
    Film { cx, window, store }
}

impl Film {
    fn notify(&mut self, method: &str, params: serde_json::Value) {
        let store = self.store.clone();
        self.cx.update(|cx| {
            store.update(cx, |store, cx| {
                store.notify_for_test(AGENT, method, params, cx);
            })
        });
    }

    /// Advance the fake clock by one frame, settle tasks and draw.
    fn step(&mut self, n: u64) {
        // Cumulative, so 1/30 s does not drift.
        let from = n * 1_000_000_000 / FPS;
        let to = (n + 1) * 1_000_000_000 / FPS;
        self.cx.advance_clock(Duration::from_nanos(to - from));
        self.cx.run_until_parked();
        self.cx
            .update_window(self.window, |_, window, cx| window.render_frame(cx))
            .expect("draw");
    }

    fn capture(&mut self) -> Frame {
        let image = self.cx.capture_screenshot(self.window).expect("capture");
        Frame {
            width: image.width(),
            height: image.height(),
            rgba: image.into_raw(),
        }
    }
}

/// Open question of the spike: do clicks and keystrokes reach a headless
/// window? Writes `probe-input.png`; the composer should show "h".
#[test]
#[ignore = "probes headless input; writes a PNG"]
fn probe_headless_input() {
    let out = out_dir();
    let mut film = open(World::Fixture);
    film.step(0);
    // A keystroke goes to the focused composer.
    film.cx
        .update_window(film.window, |_, window, cx| window.press("h", cx))
        .expect("press");
    film.step(1);
    let frame = film.capture();
    write_png(&out.join("probe-input.png"), &frame);
    // The composer is a `debug_selector` (bounds only), not an element id, so
    // `click` cannot name it. Open the find bar with a keystroke, then click
    // its close button by id.
    // Find needs a transcript: deliver the prompt first.
    for (method, params) in steps(9, &[]) {
        film.notify(method, params);
    }
    film.step(1);
    film.step(1);
    let mac = film.cx.update(|cx| cx.platform_info().is_mac());
    println!("platform_info().is_mac() = {mac}");
    film.cx
        .update_window(film.window, |_, window, cx| {
            window.press(if mac { "cmd-f" } else { "ctrl-f" }, cx)
        })
        .expect("press");
    film.step(2);
    write_png(&out.join("probe-find-open.png"), &film.capture());
    let mut found = (false, false);
    film.cx
        .update_window(film.window, |_, window, cx| {
            found = (
                window.try_find("composer-input").is_some(),
                window.try_find("find-close").is_some(),
            );
            if found.1 {
                window.click("find-close", cx);
            }
        })
        .expect("click");
    println!(
        "element ids found: composer-input={}, find-close={}",
        found.0, found.1
    );
    film.step(3);
    write_png(&out.join("probe-find-closed.png"), &film.capture());
}

/// Does a real agent seed the sidebar and the model pill? Connects to
/// `$QMT_FILM_AGENT` (default `localhost:3030`) and writes `probe-seeded.png`.
#[test]
#[ignore = "needs a running qmtcode; writes a PNG"]
fn probe_seeded_from_agent() {
    let out = out_dir();
    let mut film = open(World::Agent(agent_address()));
    film.step(0);
    film.step(1);
    write_png(&out.join("probe-seeded.png"), &film.capture());
}

#[test]
#[ignore = "films a demo clip; needs ffmpeg"]
fn film_prompt_and_streamed_reply() {
    film_scene(World::Fixture, "prompt-and-reply");
}

/// The same scene, with the sidebar and the model catalog from a real agent.
#[test]
#[ignore = "films a demo clip; needs ffmpeg and a running qmtcode"]
fn film_prompt_and_streamed_reply_seeded() {
    film_scene(World::Agent(agent_address()), "prompt-and-reply-seeded");
}

/// Film the scripted prompt and reply into `<name>.mp4`, with stills
/// `<name>-000.png` and `<name>-090.png`.
fn film_scene(world: World, name: &str) {
    let started = Instant::now();
    let out = out_dir();
    let mut film = open(world);

    // Frame 0: an empty session.
    film.step(0);
    let first = film.capture();
    let (width, height) = (first.width, first.height);
    println!("captured size: {width}x{height}");
    write_png(&out.join(format!("{name}-000.png")), &first);

    let clip = out.join(format!("{name}.mp4"));
    let mut ffmpeg = Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
        ])
        .args(["-s", &format!("{width}x{height}"), "-r", &FPS.to_string()])
        .args([
            "-i", "-", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "18",
        ])
        .arg(&clip)
        .stdin(Stdio::piped())
        .spawn()
        .expect("ffmpeg must be on PATH to film demo clips");
    let mut stdin = ffmpeg.stdin.take().expect("ffmpeg stdin");

    let chunks = reply_chunks();
    let mut capture_time = Duration::ZERO;
    let mut frames = 0;
    for n in 0..FRAMES {
        for (method, params) in steps(n, &chunks) {
            film.notify(method, params);
        }
        film.step(n);
        let t = Instant::now();
        let image = film.capture();
        capture_time += t.elapsed();
        assert_eq!((image.width, image.height), (width, height));
        stdin.write_all(&image.rgba).expect("pipe frame");
        if n == 90 {
            write_png(&out.join(format!("{name}-090.png")), &image);
        }
        frames += 1;
    }
    let render_done = started.elapsed();
    drop(stdin);
    let encode = Instant::now();
    let status = ffmpeg.wait().expect("wait for ffmpeg");
    assert!(status.success(), "ffmpeg failed: {status}");

    println!("frames: {frames}");
    println!("total wall time: {:.2?}", started.elapsed());
    println!("render + capture + pipe: {render_done:.2?}");
    println!(
        "mean capture per frame: {:.1} ms",
        capture_time.as_secs_f64() * 1000.0 / frames as f64
    );
    println!("ffmpeg wait after last frame: {:.2?}", encode.elapsed());
    println!("clip: {}", clip.display());
    assert_eq!(frames, FRAMES);
}
