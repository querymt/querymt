//! Film the real app offscreen, frame by frame, from a scene file (OpenSpec
//! changes `spike-gpui-demo-film` and `add-gpui-film-scenes`). Test-only; the
//! film tests are ignored (they need `ffmpeg`).
//!
//! Run: `QMT_FILM_SCENE=<scene.toml> cargo nextest run -p querymt-gpui --bins
//! --run-ignored only -E 'test(=film::film_scene)' --no-capture`
//!
//! The scene file is read at run time: editing it needs no recompile.

mod agent;
mod scene;
mod timing;
mod track;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Entity, HeadlessAppContext, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, ScrollDelta, ScrollEvent,
    ScrollSource, SharedString, TouchPhase, WindowBounds, WindowOptions, bounds, point, px, size,
};
use serde_json::json;

use self::agent::{FakeAgent, Out};
use self::scene::{Action, Resolved, Scene, Step, Target, Theme, WindowName};
use self::timing::{Canvas, Event, Mark, Timing, TrackLog};
use self::track::{Encoder, Frame, PanelCodec};
use crate::quick::{MAX_HEIGHT, QuickHost, WIDTH};
use crate::rpc::RpcClient;
use crate::session::SessionView;
use crate::store::AgentsStore;
use crate::ui::QmtApp;

const AGENT: &str = "test-agent";
/// How long a seeded take waits for the agent's sessions and models.
const SEED_TIMEOUT: Duration = Duration::from_secs(20);

/// Where a clip goes: `$QMT_FILM_OUT/<name>/`, else `target/film/<name>/`.
fn out_dir(name: &str) -> PathBuf {
    let base = std::env::var_os("QMT_FILM_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/film"));
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).expect("create the film output directory");
    dir
}

fn write_png(path: &Path, frame: &Frame) {
    let file = std::fs::File::create(path).expect("create png");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&frame.rgba).expect("png data");
}

/// The agent a seeded world connects to: `$QMT_FILM_AGENT`.
fn agent_env() -> Option<String> {
    std::env::var("QMT_FILM_AGENT").ok()
}

struct Film {
    cx: HeadlessAppContext,
    store: Entity<AgentsStore>,
    main: AnyWindowHandle,
    host: Option<Entity<QuickHost>>,
    /// The fixture agent's connection; `None` in a seeded world.
    link: Option<Link>,
}

/// The fixture agent's end of the connection.
struct Link {
    rpc: Rc<RpcClient>,
    requests: async_channel::Receiver<String>,
    agent: FakeAgent,
}

/// Connect the store's agent and wait, in wall time, until its session list
/// and model catalog have arrived.
fn seed(cx: &mut HeadlessAppContext, store: &Entity<AgentsStore>, address: &str) {
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
            Err(err) => panic!("the agent at {address} failed: {err}"),
        }
        assert!(
            started.elapsed() < SEED_TIMEOUT,
            "no sessions and models from {address} within {SEED_TIMEOUT:?}"
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

/// The store a scene starts from, and the fixture agent's link.
fn build_store(
    scene: &Scene,
    address: Option<&str>,
    cx: &mut gpui_kit::App,
) -> (Entity<AgentsStore>, Option<Link>) {
    let world = &scene.world;
    let mut link = None;
    let store = cx.new(|cx| {
        let mut store = match address {
            Some(address) => {
                let mut store = crate::store::launch::websocket_fixture(address);
                store.connect(AGENT, cx);
                store
            }
            None => {
                let mut store = crate::store::automations::schedule_ui_fixture();
                let (rpc, requests) = store.rpc_for_test(AGENT);
                link = Some(Link {
                    rpc,
                    requests,
                    agent: FakeAgent::new(world.session.as_ref().map(|s| s.id.as_str())),
                });
                let rt = store.runtime_mut_for_test(AGENT);
                if let Some(title) = &world.agent_title {
                    rt.agent_title = Some(title.clone());
                }
                rt.sessions = world
                    .sessions
                    .iter()
                    .map(|s| crate::store::SessionSummary {
                        agent_id: AGENT.into(),
                        id: s.id.clone(),
                        cwd: s.cwd.clone(),
                        title: s.title.clone(),
                        updated_at: s.updated.clone(),
                        phase: s.phase.clone(),
                        root: true,
                        open: true,
                        ..Default::default()
                    })
                    .collect();
                rt.sessions_loaded = true;
                if !world.models.is_empty() {
                    let models: Vec<_> = world
                        .models
                        .iter()
                        .map(|m| json!({ "id": m.id, "label": m.label, "provider": m.provider }))
                        .collect();
                    crate::store::launch::add_models_for_test(
                        &mut store,
                        serde_json::Value::Array(models),
                    );
                    store.runtime_mut_for_test(AGENT).models_loaded = true;
                }
                store
            }
        };
        if let Some(cwd) = &world.cwd {
            store.prefs.cwd = cwd.clone();
        }
        // In memory only: the window's appearance is the machine's, and the
        // clip must not depend on it.
        store.prefs.dark = Some(scene.theme == Theme::Dark);
        store.prefs.accent = scene.accent.clone();
        store
    });
    (store, link)
}

fn open(resolved: &Resolved) -> Film {
    let scene = &resolved.scene;
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
        crate::theme::apply(scene.theme == Theme::Dark, &scene.accent, cx);
        crate::ui::bind_keys(cx);
    });
    let (store, link) = cx.update(|cx| build_store(scene, resolved.address.as_deref(), cx));
    if let Some(address) = &resolved.address {
        seed(&mut cx, &store, address);
    }
    let (main, app) = cx.update(|cx| {
        if let Some(open) = &scene.world.session {
            store.update(cx, |store, cx| {
                let cwd = scene
                    .world
                    .cwd
                    .clone()
                    .unwrap_or_else(|| "/workspace".to_owned());
                let mut view = SessionView::new(AGENT, &open.id, &cwd, &open.title);
                // The scene's model, else the agent's first, so the pill
                // names one.
                view.model = scene
                    .world
                    .model
                    .clone()
                    .or_else(|| store.models(AGENT).first().map(|m| m.id.clone()));
                store.install_main_for_test(view, cx);
            });
        }
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds(
                point(px(0.), px(0.)),
                size(px(scene.size[0] as f32), px(scene.size[1] as f32)),
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
    let has_session = scene.world.session.is_some();
    cx.update_window(main, |_, window, cx| {
        app.update(cx, |app, cx| {
            app.apply_theme(window, cx);
            if has_session {
                app.show_session_for_test(window, cx);
            }
        });
    })
    .expect("show the session");
    let host = resolved
        .uses_panel()
        .then(|| cx.update(|cx| QuickHost::install(store.clone(), Some((main, app.clone())), cx)));
    Film {
        cx,
        store,
        main,
        host,
        link,
    }
}

impl Film {
    fn deliver(&mut self, out: Out) {
        match out {
            Out::Notify { method, params } => {
                let store = self.store.clone();
                self.cx.update(|cx| {
                    store.update(cx, |store, cx| {
                        store.notify_for_test(AGENT, &method, params, cx);
                    })
                });
            }
            Out::Reply { id, result } => {
                let link = self.link.as_ref().expect("a fixture agent");
                link.rpc
                    .handle(&json!({ "id": id, "result": result }).to_string());
            }
        }
    }

    /// Send the fake agent's output; only a fixture world has one.
    fn agent_out(&mut self, make: impl FnOnce(&mut FakeAgent) -> Vec<Out>) {
        let link = self
            .link
            .as_mut()
            .expect("agent steps need a fixture world");
        for out in make(&mut link.agent) {
            self.deliver(out);
        }
    }

    /// Answer what the app asked the fake agent since the last call.
    fn answer_requests(&mut self) {
        loop {
            self.cx.run_until_parked();
            let Some(link) = self.link.as_mut() else {
                return;
            };
            let requests: Vec<serde_json::Value> =
                std::iter::from_fn(|| link.requests.try_recv().ok())
                    .map(|text| serde_json::from_str(&text).expect("a JSON request"))
                    .collect();
            if requests.is_empty() {
                return;
            }
            for request in requests {
                let out = self
                    .link
                    .as_mut()
                    .map(|l| l.agent.answer(&request))
                    .unwrap_or_default();
                for out in out {
                    self.deliver(out);
                }
            }
        }
    }

    /// Advance the fake clock to the end of frame `n`, settle tasks.
    fn advance(&mut self, n: u64, fps: u32) {
        let fps = u64::from(fps);
        // Cumulative, so 1/fps s does not drift.
        let from = n * 1_000_000_000 / fps;
        let to = (n + 1) * 1_000_000_000 / fps;
        self.cx.advance_clock(Duration::from_nanos(to - from));
        self.answer_requests();
    }

    fn with_window<R>(
        &mut self,
        handle: AnyWindowHandle,
        f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App) -> R,
    ) -> R {
        self.cx
            .update_window(handle, |_, window, cx| f(window, cx))
            .expect("the window is open")
    }

    fn draw(&mut self, handle: AnyWindowHandle) {
        self.with_window(handle, |window, cx| {
            // The headless platform window changes its size on `resize` but
            // never tells GPUI, which keeps laying out at the old viewport
            // (the quick panel grows with its chat). A real platform calls
            // this from its resize callback.
            if window.viewport_size() != window.bounds().size {
                window.bounds_changed(cx);
            }
            window.render_frame(cx)
        });
    }

    fn capture(&mut self, handle: AnyWindowHandle) -> Frame {
        let image = self.cx.capture_screenshot(handle).expect("capture");
        Frame {
            width: image.width(),
            height: image.height(),
            rgba: image.into_raw(),
        }
    }

    /// The quick panel's window, while it is open.
    fn panel(&mut self) -> Option<(AnyWindowHandle, Entity<crate::quick::QuickPanel>)> {
        let host = self.host.clone()?;
        self.cx.update(|cx| host.read(cx).panel_for_test())
    }

    fn handle_of(&mut self, window: WindowName) -> AnyWindowHandle {
        match window {
            WindowName::Main => self.main,
            WindowName::Panel => self.panel().expect("the panel is open").0,
        }
    }
}

/// Where a window's canvas places it: logical offset, and the canvas size.
#[derive(Clone, Copy)]
struct Placement {
    offset: [f32; 2],
    canvas: [u32; 2],
}

/// Everything that depends on the output mode, for one frame.
struct Layout {
    scene_size: [u32; 2],
    composite: bool,
    anchor: scene::Anchor,
    /// Device pixels per logical pixel, from the first capture.
    scale: f32,
}

impl Layout {
    /// The logical size of the panel window, from its captured image.
    fn panel_size(&self, image: &Frame) -> [f32; 2] {
        [
            image.width as f32 / self.scale,
            image.height as f32 / self.scale,
        ]
    }

    fn placement(&self, window: WindowName, panel: Option<[f32; 2]>) -> Placement {
        match (window, panel) {
            (WindowName::Panel, Some([w, h])) if self.composite => {
                let [mw, mh] = self.scene_size.map(|v| v as f32);
                Placement {
                    offset: [
                        self.anchor.x as f32 * mw - w / 2.0,
                        mh - self.anchor.bottom as f32 * mh - h,
                    ],
                    canvas: self.scene_size,
                }
            }
            (WindowName::Panel, Some([w, h])) => Placement {
                offset: [(WIDTH - w) / 2.0, MAX_HEIGHT - h],
                canvas: [WIDTH as u32, MAX_HEIGHT as u32],
            },
            _ => Placement {
                offset: [0.0, 0.0],
                canvas: self.scene_size,
            },
        }
    }
}

fn element_id(id: &str) -> SharedString {
    SharedString::from(id.to_owned())
}

/// A click, hover or scroll at a point, as raw mouse events.
fn dispatch_at(window: &mut gpui_kit::Window, cx: &mut gpui_kit::App, at: [f32; 2], what: &Action) {
    let position = point(px(at[0]), px(at[1]));
    let modifiers = Modifiers::default();
    let mut send = |event: PlatformInput| {
        window.dispatch_event(event, cx);
    };
    send(PlatformInput::MouseMove(MouseMoveEvent {
        position,
        pressed_button: None,
        modifiers,
    }));
    match what {
        Action::Click(_) => {
            send(PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers,
                click_count: 1,
                first_mouse: false,
            }));
            send(PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers,
                click_count: 1,
            }));
        }
        Action::Scroll(spec) => {
            send(PlatformInput::Scroll(ScrollEvent {
                position,
                delta: ScrollDelta::Pixels(point(px(spec.dx), px(spec.dy))),
                modifiers,
                touch_phase: TouchPhase::Moved,
                source: ScrollSource::Pointer,
            }));
        }
        _ => {}
    }
}

/// A `type` or `stream` step that is still delivering.
struct Ongoing {
    step: usize,
    chunks: Vec<String>,
}

/// What a run produced.
struct Report {
    dir: PathBuf,
    timing: Timing,
    requests: Vec<(String, serde_json::Value)>,
}

/// Film a scene into `<out>/<name>/`.
fn film(resolved: &Resolved) -> Report {
    let started = Instant::now();
    let scene = &resolved.scene;
    let dir = out_dir(&scene.name);
    let mut film = open(resolved);
    let fps = scene.fps;
    let composite = scene.panel == scene::PanelMode::Composite;
    let codec = PanelCodec::from_env(std::env::var("QMT_FILM_PANEL_CODEC").ok().as_deref());

    // Frame 0 tells the device size, hence the scale.
    film.draw(film.main);
    let first = film.capture(film.main);
    let scale = first.width as f32 / scene.size[0] as f32;
    println!(
        "captured size: {}x{} (scale {scale})",
        first.width, first.height
    );
    let layout = Layout {
        scene_size: scene.size,
        composite,
        anchor: scene.anchor,
        scale,
    };
    let device = |v: f32| (v * scale).round() as u32;
    let (panel_w, panel_h) = (device(WIDTH), device(MAX_HEIGHT));

    let mut main_encoder = Encoder::main(&dir.join("main.mp4"), first.width, first.height, fps);
    let panel_track = resolved.uses_panel() && !composite;
    let mut panel_encoder = panel_track
        .then(|| Encoder::panel(&dir.join(codec.file_name()), panel_w, panel_h, fps, codec));
    let mut main_log = TrackLog::default();
    let mut panel_log = TrackLog::default();
    let mut events: Vec<Event> = Vec::new();
    let mut marks: Vec<Mark> = Vec::new();
    let mut captions: Vec<(String, f64)> = Vec::new();
    let mut ongoing: Vec<Ongoing> = Vec::new();
    let mut next = 0;
    let mut capture_time = Duration::ZERO;

    for n in 0..resolved.frames {
        // 1. The steps due at this frame.
        while next < resolved.steps.len() && resolved.steps[next].frame <= n {
            let step = &resolved.steps[next];
            if let Some(text) = &step.caption {
                captions.push((text.clone(), step.start));
            }
            apply_step(
                &mut film,
                step,
                next,
                &layout,
                &mut ongoing,
                &mut events,
                &mut marks,
                n,
            );
            next += 1;
        }
        // 2. Typing and streaming go one chunk per frame.
        for o in &mut ongoing {
            let step = &resolved.steps[o.step];
            let chunk = o.chunks[(n - step.frame) as usize].clone();
            if chunk.is_empty() {
                continue;
            }
            match &step.action {
                Action::Type { .. } => {
                    let handle = film.handle_of(step.window);
                    film.with_window(handle, |window, cx| window.input(&chunk, cx));
                }
                Action::Stream { .. } => film.agent_out(|agent| vec![agent.stream(&chunk)]),
                _ => {}
            }
        }
        ongoing.retain(|o| {
            let step = &resolved.steps[o.step];
            n + 1 < step.frame + o.chunks.len() as u64
        });
        // 3. One frame of time passes; the agent's requests are answered.
        film.advance(n, fps);

        // 4. Every open window is drawn and captured.
        film.draw(film.main);
        let timer = Instant::now();
        let mut main_frame = film.capture(film.main);
        let panel = film.panel();
        let mut panel_frame = None;
        if let Some((handle, _)) = &panel {
            film.draw(*handle);
            panel_frame = Some(film.capture(*handle));
        }
        capture_time += timer.elapsed();
        main_log.frame(
            n,
            Some([0.0, 0.0, scene.size[0] as f32, scene.size[1] as f32]),
        );
        match panel_frame {
            Some(mut image) => {
                track::apply_mask(&mut image, crate::quick::chrome::RADIUS * scale);
                let size = layout.panel_size(&image);
                let place = layout.placement(WindowName::Panel, Some(size));
                panel_log.frame(
                    n,
                    Some([place.offset[0], place.offset[1], size[0], size[1]]),
                );
                if composite {
                    let x = (place.offset[0] * scale).round() as i64;
                    let y = (place.offset[1] * scale).round() as i64;
                    track::composite(
                        &mut main_frame,
                        &image,
                        x,
                        y,
                        crate::quick::chrome::RADIUS * scale,
                        scale,
                    );
                } else if let Some(encoder) = &mut panel_encoder {
                    let (canvas, _) = track::place_bottom_center(&image, panel_w, panel_h);
                    encoder.write(&canvas);
                }
            }
            None => {
                panel_log.frame(n, None);
                if let Some(encoder) = &mut panel_encoder {
                    encoder.write(&Frame::transparent(panel_w, panel_h));
                }
            }
        }
        if scene.stills.contains(&n) {
            write_png(&dir.join(format!("still-{n:03}.png")), &main_frame);
        }
        main_encoder.write(&main_frame);
    }
    let render_done = started.elapsed();
    let main_wait = main_encoder.finish();
    let panel_wait = panel_encoder.map(Encoder::finish);

    let mut tracks = vec![main_log.into_info("main", Some("main.mp4".into()), scene.size, None)];
    if resolved.uses_panel() {
        tracks.push(panel_log.into_info(
            "panel",
            (!composite).then(|| codec.file_name().to_owned()),
            [WIDTH as u32, MAX_HEIGHT as u32],
            Some(scene.anchor),
        ));
    }
    let scene_end = resolved.frames as f64 / f64::from(fps);
    let timing = Timing {
        version: 1,
        name: scene.name.clone(),
        fps,
        frames: resolved.frames,
        scale: scale.round() as u32,
        canvas: Canvas {
            width: scene.size[0],
            height: scene.size[1],
        },
        tracks,
        events,
        marks,
        captions: timing::caption_spans(&captions, scene_end),
    };
    std::fs::write(dir.join(format!("{}.film.json", scene.name)), timing.json())
        .expect("write the timing file");
    std::fs::write(dir.join("film.js"), timing.js()).expect("write film.js");

    println!("frames: {}", resolved.frames);
    println!("total wall time: {:.2?}", started.elapsed());
    println!("render + capture + pipe: {render_done:.2?}");
    println!(
        "mean capture per frame: {:.1} ms",
        capture_time.as_secs_f64() * 1000.0 / resolved.frames as f64
    );
    println!("ffmpeg wait after last frame (main): {main_wait:.2?}");
    if let Some(wait) = panel_wait {
        println!("ffmpeg wait after last frame (panel): {wait:.2?}");
    }
    let requests = film
        .link
        .as_ref()
        .map(|l| l.agent.log.clone())
        .unwrap_or_default();
    for (method, _) in &requests {
        println!("request: {method}");
    }
    println!("clip: {}", dir.display());
    Report {
        dir,
        timing,
        requests,
    }
}

/// Run one step at frame `n`.
#[allow(clippy::too_many_arguments)]
fn apply_step(
    film: &mut Film,
    step: &Step,
    index: usize,
    layout: &Layout,
    ongoing: &mut Vec<Ongoing>,
    events: &mut Vec<Event>,
    marks: &mut Vec<Mark>,
    n: u64,
) {
    let window = step.window.as_str();
    let mut event = Event::new(n, step.start, "", window);
    // The target's bounds, where the step has one, in canvas pixels.
    let resolve = |film: &mut Film, target: &Target| -> ([f32; 4], Placement) {
        let handle = film.handle_of(step.window);
        let rect = film.with_window(handle, |window, _| match target {
            Target::Id(id) => {
                let b = window.find(element_id(id)).bounds();
                [
                    f32::from(b.origin.x),
                    f32::from(b.origin.y),
                    f32::from(b.size.width),
                    f32::from(b.size.height),
                ]
            }
            Target::Point { point } => [point[0], point[1], 0.0, 0.0],
        });
        let panel_size = (step.window == WindowName::Panel)
            .then(|| film.capture(handle))
            .map(|image| layout.panel_size(&image));
        (rect, layout.placement(step.window, panel_size))
    };
    let place_event = |mut event: Event, target: &Target, rect: [f32; 4], p: Placement| {
        let canvas_rect = [
            rect[0] + p.offset[0],
            rect[1] + p.offset[1],
            rect[2],
            rect[3],
        ];
        event = event.at(&target.label(), canvas_rect, p.canvas);
        event
    };
    match &step.action {
        Action::Wait | Action::End => return,
        Action::Type { text } => {
            event.kind = "type".into();
            event.text = Some(text.clone());
            event.until = Some(step.end);
            // The composer, when it can be found, is where the text goes.
            let handle = film.handle_of(step.window);
            let found = film.with_window(handle, |window, _| {
                window
                    .try_find(element_id("composer-input"))
                    .map(|s| s.bounds())
            });
            if let Some(b) = found {
                let target = Target::Id("composer-input".into());
                let panel_size = (step.window == WindowName::Panel)
                    .then(|| film.capture(handle))
                    .map(|image| layout.panel_size(&image));
                let rect = [
                    f32::from(b.origin.x),
                    f32::from(b.origin.y),
                    f32::from(b.size.width),
                    f32::from(b.size.height),
                ];
                event = place_event(
                    event,
                    &target,
                    rect,
                    layout.placement(step.window, panel_size),
                );
            }
            ongoing.push(Ongoing {
                step: index,
                chunks: scene::type_chunks(text, step.frames),
            });
        }
        Action::Stream { text } => {
            event.kind = "stream".into();
            event.text = Some(text.clone());
            event.until = Some(step.end);
            ongoing.push(Ongoing {
                step: index,
                chunks: scene::stream_chunks(text, step.frames),
            });
        }
        Action::Press(key) => {
            event.kind = "press".into();
            event.key = Some(key.clone());
            let handle = film.handle_of(step.window);
            film.with_window(handle, |window, cx| window.press(key, cx));
        }
        Action::Click(target) | Action::Hover(target) => {
            let click = matches!(step.action, Action::Click(_));
            event.kind = if click { "click" } else { "hover" }.into();
            let (rect, placement) = resolve(film, target);
            event = place_event(event, target, rect, placement);
            let handle = film.handle_of(step.window);
            film.with_window(handle, |window, cx| match target {
                Target::Id(id) if click => window.click(element_id(id), cx),
                Target::Id(id) => window.hover(element_id(id), cx),
                Target::Point { point } => dispatch_at(window, cx, *point, &step.action),
            });
        }
        Action::Scroll(spec) => {
            event.kind = "scroll".into();
            let (rect, placement) = resolve(film, &spec.target);
            event = place_event(event, &spec.target, rect, placement);
            let handle = film.handle_of(step.window);
            film.with_window(handle, |window, cx| match &spec.target {
                Target::Id(id) => window.scroll(
                    element_id(id),
                    ScrollDelta::Pixels(point(px(spec.dx), px(spec.dy))),
                    cx,
                ),
                Target::Point { point } => dispatch_at(window, cx, *point, &step.action),
            });
        }
        Action::QuickOpen => {
            event.kind = "quick_open".into();
            let host = film.host.clone().expect("the world has a quick host");
            film.cx
                .update(|cx| host.update(cx, |host, cx| host.show(cx)));
            film.cx.run_until_parked();
            let (handle, panel) = film.panel().expect("the panel opened");
            // The panel's composer takes the keyboard focus.
            film.with_window(handle, |window, cx| {
                let pane = panel.read(cx).pane_for_test().clone();
                pane.update(cx, |pane, cx| pane.focus_composer(window, cx));
            });
        }
        Action::QuickClose => {
            event.kind = "quick_close".into();
            let host = film.host.clone().expect("the world has a quick host");
            film.cx
                .update(|cx| host.update(cx, |host, cx| host.hide(cx)));
            film.cx.run_until_parked();
        }
        Action::ToolCall(call) => {
            event.kind = "tool_call".into();
            event.text = Some(call.title.clone());
            film.agent_out(|agent| vec![agent.tool_call(call)]);
        }
        Action::State(state) => {
            event.kind = "state".into();
            event.text = Some(state.clone());
            film.agent_out(|agent| vec![agent.state(state)]);
        }
        Action::EndTurn(reason) => {
            event.kind = "end_turn".into();
            event.text = Some(reason.clone());
            film.agent_out(|agent| agent.end_turn(reason));
        }
        Action::Notify(spec) => {
            event.kind = "notify".into();
            event.text = Some(spec.method.clone());
            film.agent_out(|agent| vec![agent.notify(spec)]);
        }
        Action::Mark(spec) => {
            let (rect, placement) = resolve(film, &spec.target);
            let marked = place_event(event, &spec.target, rect, placement);
            marks.push(Mark {
                name: spec.name.clone(),
                frame: n,
                window: window.to_owned(),
                rect: marked.rect.expect("a mark has bounds"),
                pct: marked.pct.expect("a mark has bounds"),
            });
            return;
        }
    }
    events.push(event);
}

/// A scene path: relative to the repository root, or absolute.
fn scene_path(path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }
}

fn film_file(path: &Path) -> Report {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read the scene {}: {e}", path.display()));
    let resolved = scene::parse(&text, agent_env().as_deref())
        .unwrap_or_else(|e| panic!("scene {}: {e}", path.display()));
    film(&resolved)
}

/// Film the scene at `$QMT_FILM_SCENE` (a path relative to the repository
/// root, or absolute) into `$QMT_FILM_OUT/<name>/`.
#[test]
#[ignore = "films a demo clip; needs ffmpeg and QMT_FILM_SCENE"]
fn film_scene() {
    let path = std::env::var("QMT_FILM_SCENE").expect(
        "set QMT_FILM_SCENE to a scene file, e.g. crates/querymt-gpui/film/scenes/smoke.toml",
    );
    film_file(&scene_path(&path));
}

/// The change's own end-to-end check: films `film/scenes/smoke.toml`.
#[test]
#[ignore = "films a demo clip; needs ffmpeg"]
fn smoke_scene() {
    let report = film_file(&Path::new(env!("CARGO_MANIFEST_DIR")).join("film/scenes/smoke.toml"));
    let timing = &report.timing;
    assert_eq!(timing.name, "smoke");
    assert!(report.dir.join("main.mp4").is_file());
    assert!(report.dir.join("smoke.film.json").is_file());
    assert!(report.dir.join("film.js").is_file());
    assert!(
        timing
            .events
            .iter()
            .any(|e| e.kind == "click" && e.rect.is_some())
    );
    assert!(
        timing
            .events
            .iter()
            .any(|e| e.kind == "type" && e.until.is_some())
    );
    // The prompt went out through the composer's real send path.
    assert!(report.requests.iter().any(|(m, _)| m == "session/prompt"));
}

/// The composer can be targeted by id.
#[test]
fn the_composer_has_an_element_id() {
    let text = r#"
version = 1
name = "composer-id"
[world]
cwd = "/w"
session = { id = "film", title = "Film" }
[[step]]
at = 1.0
end = true
"#;
    let resolved = scene::parse(text, None).expect("valid");
    let mut film = open(&resolved);
    film.draw(film.main);
    let found = film.with_window(film.main, |window, _| {
        window.try_find(element_id("composer-input")).is_some()
    });
    assert!(found, "try_find(\"composer-input\") after one frame");
}
