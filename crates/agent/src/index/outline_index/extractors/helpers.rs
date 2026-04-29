//! Shared UTF-8-safe slicing helper for outline extraction tests.

#[allow(dead_code)]
/// Safely slice `source` between byte offsets, snapping both boundaries to
/// valid UTF-8 char boundaries. This prevents panics when a byte offset
/// lands inside a multi-byte character.
pub fn safe_slice(source: &str, from: usize, to: usize) -> &str {
    let from = source.floor_char_boundary(from.min(source.len()));
    let to = source.ceil_char_boundary(to.min(source.len()));
    &source[from..to]
}
