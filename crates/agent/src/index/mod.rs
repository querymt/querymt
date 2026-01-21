pub mod function_index;
pub mod merkle;
pub mod search;

// Re-export commonly used types
pub use function_index::{
    FunctionIndex, FunctionIndexConfig, IndexedFunctionEntry, SimilarFunctionMatch,
};
pub use merkle::DiffPaths;
