//! Ajika inputs and queries of the code cache (design D2, D3, D7).
//!
//! Expected per-file failures (parse error, unsupported language, unreadable text, oversize
//! file) are returned as stored data, so only cancellation or storage errors fail an
//! operation.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ajika::InputInit;

use super::values::{FunctionsResult, SymbolsResult};
use crate::function_index::{FunctionIndexConfig, index_file_entries, parse_pool};

/// Host state read by input initializers: the workspace root that keys are relative to.
pub(crate) struct WorkspaceRoot(pub(crate) PathBuf);

/// Untracked host services. Counts real query executions for the statistics (design D7).
#[derive(Default)]
pub(crate) struct CacheServices {
    executions: AtomicU64,
}

impl CacheServices {
    pub(crate) fn executions(&self) -> u64 {
        self.executions.load(Ordering::Relaxed)
    }

    fn record_execution(&self) {
        self.executions.fetch_add(1, Ordering::Relaxed);
    }
}

impl ajika::Services for CacheServices {}

pub(crate) type Db<'a> = ajika::Db<'a, CacheServices>;

/// Text of one workspace file, keyed by its root-relative path with `/` separators.
///
/// `readable` is false when the file could not be read as UTF-8 text; `text` is then empty.
#[ajika::input(key = String)]
pub(crate) struct SourceFile {
    #[input(arc, initialize = read_source)]
    text: String,
    #[input(initialize = read_readable)]
    readable: bool,
}

/// Settings that change query answers, so they are inputs rather than services.
#[ajika::input(key = ())]
pub(crate) struct IndexSettings {
    min_function_lines: u32,
    max_file_bytes: u64,
}

fn read_source(key: &String, cx: &dyn InputInit) -> ajika::Result<String> {
    cx.check_cancelled()?;
    let root = cx.state::<WorkspaceRoot>()?;
    Ok(fs::read_to_string(root.0.join(key)).unwrap_or_default())
}

fn read_readable(key: &String, cx: &dyn InputInit) -> ajika::Result<bool> {
    cx.check_cancelled()?;
    let root = cx.state::<WorkspaceRoot>()?;
    Ok(fs::read_to_string(root.0.join(key)).is_ok())
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
    let file = SourceFile::at(path.clone());
    if !file.readable(db)? {
        return Ok(SymbolsResult::Unreadable);
    }
    let text = file.text(db)?;
    on_parse_pool(|| SymbolsResult::compute(Path::new(&path), &text))
}

#[ajika::query]
fn file_functions(db: &Db, path: String) -> ajika::Result<FunctionsResult> {
    db.services().record_execution();
    let settings = IndexSettings::at(());
    let config = FunctionIndexConfig {
        min_function_lines: settings.min_function_lines(db)?,
        max_file_bytes: usize::try_from(settings.max_file_bytes(db)?).unwrap_or(usize::MAX),
        ..FunctionIndexConfig::default()
    };
    let file = SourceFile::at(path.clone());
    if !file.readable(db)? {
        return Ok(FunctionsResult::default());
    }
    let text = file.text(db)?;
    // `index_file_entries` applies the oversize skip and runs on the parse pool.
    let entries = index_file_entries(Path::new(&path), &text, &config);
    Ok(FunctionsResult::from_entries(&entries))
}

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
