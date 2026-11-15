//! The token estimation trait, kept in its own file so that edits to
//! `pruning.rs` do not move its definition: a shifted span of this
//! `dyn`-used trait invalidates the runtime and facade builds (about 55 s).

/// Trait for estimating token counts from text.
pub trait TokenEstimator: Send + Sync {
    fn estimate(&self, text: &str) -> usize;
}
