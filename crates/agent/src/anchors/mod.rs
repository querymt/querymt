//! Session-scoped line anchors for anchored reads and edits.

pub mod edit;
pub mod reconcile;
pub mod render;
pub mod store;
pub mod symbol_cache;

pub use render::{ANCHOR_DELIMITER, render_anchored_range};
pub use store::{AnchorKey, FileAnchorState, LineAnchor, reconcile_file, resolve_anchor};

/// Split a raw anchor string on the § delimiter.
///
/// - `"xK7mQ2§fn main()"` → `("xK7mQ2", Some("fn main()"))`
/// - `"xK7mQ2"`           → `("xK7mQ2", None)`
pub fn split_anchor(raw: &str) -> (&str, Option<&str>) {
    match raw.split_once(ANCHOR_DELIMITER) {
        Some((id, content)) => (id, Some(content)),
        None => (raw, None),
    }
}
