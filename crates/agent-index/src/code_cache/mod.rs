//! Persistent per-workspace cache of derived per-file results (symbols and function entries),
//! backed by Ajika.
//!
//! One owner thread per workspace holds the database ([`CodeCacheClient`]); results are stored
//! under the platform cache directory and reused across restarts (see
//! [`CodeIndexCacheOptions`]).

#[cfg(test)]
mod ajika_contract_tests;
mod owner;
mod queries;
mod storage;
#[cfg(test)]
mod tests;
mod values;

pub use owner::{CodeCacheClient, CodeCacheError, CodeCacheStats, FileEntries};
pub use storage::{
    CODE_CACHE_SCHEMA_VERSION, CacheStorageKind, CodeIndexCacheOptions, CodeIndexStorage,
};
