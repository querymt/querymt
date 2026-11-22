//! Ajika inputs and queries of the code cache (design D2, D3, D7).
//!
//! Expected per-file failures (parse error, unsupported language, unreadable text, oversize
//! file) are returned as stored data, so only cancellation or storage errors fail an
//! operation.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ajika::InputInit;

use super::values::{FunctionsResult, SymbolsResult};
use crate::function_index::{
    FunctionIndexConfig, IndexedFunctionEntry, index_file_entries, parse_pool,
};

/// Host state read by input initializers: the workspace root that keys are relative to.
pub(crate) struct WorkspaceRoot(pub(crate) PathBuf);

/// Untracked host services (design D7).
///
/// - Counts real query executions for the statistics.
/// - Hands the entries `file_functions` just computed back to the host, with their TypeScript
///   fingerprints, so the host does not recompute those fingerprints from the stored mirror.
///   This is an untracked side output: it never affects an answer, and a reused result simply
///   has no fresh entries, so the host rebuilds them from the stored value.
#[derive(Default)]
pub(crate) struct CacheServices {
    executions: AtomicU64,
    fresh_functions: Mutex<HashMap<String, Vec<IndexedFunctionEntry>>>,
}

impl CacheServices {
    pub(crate) fn executions(&self) -> u64 {
        self.executions.load(Ordering::Relaxed)
    }

    fn record_execution(&self) {
        self.executions.fetch_add(1, Ordering::Relaxed);
    }

    fn record_fresh_functions(&self, key: &str, entries: Vec<IndexedFunctionEntry>) {
        self.fresh_functions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), entries);
    }

    /// Take every entry list computed since the last call, keyed by cache key.
    pub(crate) fn take_fresh_functions(&self) -> HashMap<String, Vec<IndexedFunctionEntry>> {
        std::mem::take(
            &mut *self
                .fresh_functions
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

impl ajika::Services for CacheServices {}

pub(crate) type Db<'a> = ajika::Db<'a, CacheServices>;

/// Text of one workspace file, keyed by its root-relative path with `/` separators.
///
/// `None` when the file could not be read as UTF-8 text. Readability and text come from one
/// read, so they can never disagree.
#[ajika::input(key = String)]
pub(crate) struct SourceFile {
    #[input(arc, initialize = read_source)]
    text: Option<String>,
}

/// Settings that change query answers, so they are inputs rather than services.
#[ajika::input(key = ())]
pub(crate) struct IndexSettings {
    min_function_lines: u32,
    max_file_bytes: u64,
}

fn read_source(key: &String, cx: &dyn InputInit) -> ajika::Result<Option<String>> {
    cx.check_cancelled()?;
    let root = cx.state::<WorkspaceRoot>()?;
    Ok(fs::read_to_string(root.0.join(key)).ok())
}

/// Run a leaf parse on the shared 64 MB-stack pool.
///
/// Ajika's scoped workers have default-sized stacks, and tree-sitter recursion can overflow
/// them. The closure never calls a query or touches `Db`, so blocking the worker until it
/// returns cannot deadlock (design D3).
fn on_parse_pool<R: Send>(work: impl FnOnce() -> R + Send) -> ajika::Result<R> {
    let pool = parse_pool().map_err(ajika::Error::application)?;
    Ok(pool.install(work))
}

#[ajika::query]
fn file_symbols(db: &Db, path: String) -> ajika::Result<SymbolsResult> {
    db.services().record_execution();
    let text = SourceFile::at(path.clone()).text(db)?;
    let Some(text) = text.as_deref() else {
        return Ok(SymbolsResult::Unreadable);
    };
    on_parse_pool(|| SymbolsResult::compute(Path::new(&path), text))
}

#[ajika::query]
fn file_functions(db: &Db, path: String) -> ajika::Result<FunctionsResult> {
    db.services().record_execution();
    #[cfg(test)]
    assert!(
        !path.ends_with(PANIC_FOR_TEST),
        "test hook: simulated parser panic"
    );
    let settings = IndexSettings::at(());
    let config = FunctionIndexConfig {
        min_function_lines: settings.min_function_lines(db)?,
        max_file_bytes: usize::try_from(settings.max_file_bytes(db)?).unwrap_or(usize::MAX),
        ..FunctionIndexConfig::default()
    };
    let text = SourceFile::at(path.clone()).text(db)?;
    let Some(text) = text.as_deref() else {
        return Ok(FunctionsResult::default());
    };
    // `index_file_entries` turns a pool startup failure into an empty list, which must not
    // be stored as this file's answer, so fail the operation before computing anything.
    parse_pool().map_err(ajika::Error::application)?;
    // `index_file_entries` applies the oversize skip and runs on the parse pool.
    let entries = index_file_entries(Path::new(&path), text, &config);
    let result = FunctionsResult::from_entries(&entries);
    db.services().record_fresh_functions(&path, entries);
    Ok(result)
}

/// File name that makes `file_functions` panic in tests, to exercise the owner's recovery.
#[cfg(test)]
pub(crate) const PANIC_FOR_TEST: &str = "qmt-test-panic.rs";

/// Symbols for each key, in order, computed concurrently.
pub(crate) fn symbols_for(db: &Db<'_>, keys: &[String]) -> ajika::Result<Vec<Arc<SymbolsResult>>> {
    db.parallel_map(keys, |db, key| file_symbols(db, key.clone()))
}

/// Function entries for each key, in order, computed concurrently.
///
/// This is a plain function rather than a stored query: persisting one record that repeats
/// every file's functions would duplicate them on disk and rewrite them for any single change.
pub(crate) fn functions_for(
    db: &Db<'_>,
    keys: &[String],
) -> ajika::Result<Vec<Arc<FunctionsResult>>> {
    db.parallel_map(keys, |db, key| file_functions(db, key.clone()))
}
