//! The timing file `<name>.film.json` (design D6) and its `film.js` twin:
//! where and when things happened, for a video tool to place a cursor,
//! zooms, captions and the panel.

use serde::Serialize;

use super::scene::Anchor;

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct Canvas {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct TrackInfo {
    pub window: String,
    pub file: Option<String>,
    /// Logical size of the track's canvas.
    pub size: [u32; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<AnchorInfo>,
    /// Inclusive frame ranges in which the window is open.
    pub visible: Vec<[u64; 2]>,
    /// `[frame, x, y, w, h]` for the frames where the window's size changed.
    pub bounds: Vec<[f32; 5]>,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct AnchorInfo {
    pub x: f64,
    pub bottom: f64,
}

impl From<Anchor> for AnchorInfo {
    fn from(a: Anchor) -> Self {
        AnchorInfo {
            x: a.x,
            bottom: a.bottom,
        }
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct Event {
    pub frame: u64,
    pub t: f64,
    pub kind: String,
    pub window: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// `[x, y, w, h]` in logical pixels of the window's canvas.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rect: Option<[f32; 4]>,
    /// `rect` in % of the canvas.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pct: Option<[f64; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// When a `type`, `stream` or `wait` ends, in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<f64>,
}

impl Event {
    pub(super) fn new(frame: u64, t: f64, kind: &str, window: &str) -> Self {
        Event {
            frame,
            t,
            kind: kind.into(),
            window: window.into(),
            target: None,
            rect: None,
            pct: None,
            text: None,
            key: None,
            until: None,
        }
    }

    /// Set the target's name and its bounds `rect` on a canvas of `canvas`.
    pub(super) fn at(mut self, target: &str, rect: [f32; 4], canvas: [u32; 2]) -> Self {
        self.target = Some(target.into());
        self.rect = Some(rect);
        self.pct = Some(pct(rect, canvas));
        self
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct Mark {
    pub name: String,
    pub frame: u64,
    pub window: String,
    pub rect: [f32; 4],
    pub pct: [f64; 4],
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct Caption {
    pub text: String,
    pub from: f64,
    pub to: f64,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct Timing {
    pub version: u32,
    pub name: String,
    pub fps: u32,
    pub frames: u64,
    pub scale: u32,
    pub canvas: Canvas,
    pub tracks: Vec<TrackInfo>,
    pub events: Vec<Event>,
    pub marks: Vec<Mark>,
    pub captions: Vec<Caption>,
}

/// `rect` (`[x, y, w, h]`) in % of `canvas`, to one decimal.
pub(super) fn pct(rect: [f32; 4], canvas: [u32; 2]) -> [f64; 4] {
    let round = |v: f64| (v * 10.0).round() / 10.0;
    let (w, h) = (f64::from(canvas[0]), f64::from(canvas[1]));
    [
        round(f64::from(rect[0]) / w * 100.0),
        round(f64::from(rect[1]) / h * 100.0),
        round(f64::from(rect[2]) / w * 100.0),
        round(f64::from(rect[3]) / h * 100.0),
    ]
}

/// Captions as spans: each lasts until the next caption, the last until
/// the end of the scene.
pub(super) fn caption_spans(starts: &[(String, f64)], scene_end: f64) -> Vec<Caption> {
    starts
        .iter()
        .enumerate()
        .map(|(i, (text, from))| Caption {
            text: text.clone(),
            from: *from,
            to: starts.get(i + 1).map_or(scene_end, |next| next.1),
        })
        .collect()
}

/// Collects the open frames and the size changes of one window while the
/// clip is filmed.
#[derive(Debug, Default)]
pub(super) struct TrackLog {
    visible: Vec<[u64; 2]>,
    bounds: Vec<[f32; 5]>,
    open: bool,
}

impl TrackLog {
    /// Record frame `frame`: whether the window is open and, when it is,
    /// its bounds `[x, y, w, h]`.
    pub(super) fn frame(&mut self, frame: u64, bounds: Option<[f32; 4]>) {
        match (bounds, self.open) {
            (Some(_), true) => {
                if let Some(last) = self.visible.last_mut() {
                    last[1] = frame;
                }
            }
            (Some(_), false) => {
                self.visible.push([frame, frame]);
                self.open = true;
            }
            (None, _) => self.open = false,
        }
        if let Some(b) = bounds {
            let changed = self
                .bounds
                .last()
                .is_none_or(|l| l[3] != b[2] || l[4] != b[3]);
            if changed {
                self.bounds.push([frame as f32, b[0], b[1], b[2], b[3]]);
            }
        }
    }

    pub(super) fn into_info(
        self,
        window: &str,
        file: Option<String>,
        size: [u32; 2],
        anchor: Option<Anchor>,
    ) -> TrackInfo {
        TrackInfo {
            window: window.into(),
            file,
            size,
            anchor: anchor.map(Into::into),
            visible: self.visible,
            bounds: self.bounds,
        }
    }
}

impl Timing {
    pub(super) fn json(&self) -> String {
        serde_json::to_string_pretty(self).expect("the timing file serializes")
    }

    /// The same object for an HTML composition's `<script src>`.
    pub(super) fn js(&self) -> String {
        format!("window.QMT_FILM = {};\n", self.json())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_is_a_share_of_the_canvas_to_one_decimal() {
        assert_eq!(
            pct([312.0, 690.0, 658.0, 64.0], [1280, 800]),
            [24.4, 86.3, 51.4, 8.0]
        );
        assert_eq!(
            pct([0.0, 0.0, 1280.0, 800.0], [1280, 800]),
            [0.0, 0.0, 100.0, 100.0]
        );
    }

    #[test]
    fn captions_last_until_the_next_one_or_the_end() {
        let spans = caption_spans(
            &[
                ("Ask".into(), 0.3),
                ("Answer".into(), 1.5),
                ("Done".into(), 3.0),
            ],
            6.0,
        );
        assert_eq!(
            spans,
            vec![
                Caption {
                    text: "Ask".into(),
                    from: 0.3,
                    to: 1.5
                },
                Caption {
                    text: "Answer".into(),
                    from: 1.5,
                    to: 3.0
                },
                Caption {
                    text: "Done".into(),
                    from: 3.0,
                    to: 6.0
                },
            ]
        );
        assert!(caption_spans(&[], 6.0).is_empty());
    }

    #[test]
    fn a_track_logs_visible_ranges_and_size_changes() {
        let mut log = TrackLog::default();
        log.frame(0, None);
        log.frame(1, None);
        log.frame(2, Some([10.0, 0.0, 680.0, 132.0]));
        log.frame(3, Some([10.0, 0.0, 680.0, 132.0]));
        // Same size, moved: not a size change.
        log.frame(4, Some([12.0, 0.0, 680.0, 132.0]));
        log.frame(5, Some([12.0, 0.0, 680.0, 220.0]));
        log.frame(6, None);
        log.frame(7, Some([12.0, 0.0, 680.0, 220.0]));
        let info = log.into_info("panel", Some("panel.webm".into()), [680, 560], None);
        assert_eq!(info.visible, vec![[2, 5], [7, 7]]);
        assert_eq!(
            info.bounds,
            vec![
                [2.0, 10.0, 0.0, 680.0, 132.0],
                [5.0, 12.0, 0.0, 680.0, 220.0]
            ]
        );
    }

    #[test]
    fn the_json_and_the_js_wrapper_carry_the_same_object() {
        let timing = Timing {
            version: 1,
            name: "demo".into(),
            fps: 30,
            frames: 60,
            scale: 2,
            canvas: Canvas {
                width: 1280,
                height: 800,
            },
            tracks: vec![TrackInfo {
                window: "main".into(),
                file: Some("main.mp4".into()),
                size: [1280, 800],
                anchor: None,
                visible: vec![[0, 59]],
                bounds: vec![],
            }],
            events: vec![Event::new(9, 0.3, "click", "main").at(
                "composer-input",
                [312.0, 690.0, 656.0, 64.0],
                [1280, 800],
            )],
            marks: vec![],
            captions: vec![],
        };
        let json = timing.json();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["tracks"][0]["file"], "main.mp4");
        assert_eq!(value["events"][0]["pct"][0], 24.4);
        assert_eq!(value["events"][0]["target"], "composer-input");
        // Optional fields are left out, not null.
        assert!(value["events"][0].get("text").is_none());
        assert!(value["tracks"][0].get("anchor").is_none());
        let js = timing.js();
        assert!(js.starts_with("window.QMT_FILM = {"));
        assert!(js.ends_with("};\n"));
        let body = js
            .trim_start_matches("window.QMT_FILM = ")
            .trim_end()
            .trim_end_matches(';');
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            value
        );
    }
}
