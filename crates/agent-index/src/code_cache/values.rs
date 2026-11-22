//! Persisted value types for the code cache (design D4).
//!
//! The public index types are not serialisable (`AstFingerprint` holds a
//! `HashMap<&'static str, u32>`) and do not implement `PartialEq`, which Ajika requires of
//! stored results. These private mirrors derive `PartialEq` and wincode, convert losslessly
//! from and to the public types, and drop the TypeScript `AstFingerprint`, which
//! [`FunctionValue::into_entry`] recomputes from the body text.

use std::path::{Path, PathBuf};

use querymt_agent_core::hash::RapidHash;
use similarity_core::AstFingerprint;

use crate::function_index::IndexedFunctionEntry;
use crate::symbol_index::{SymbolDigest, SymbolEntry, SymbolError, SymbolIndex, SymbolKind};

/// Language category whose functions carry an `AstFingerprint`.
const FINGERPRINT_LANGUAGE: &str = "typescript";

#[derive(Debug, Clone, Copy, PartialEq, Eq, wincode::SchemaRead, wincode::SchemaWrite)]
pub(crate) enum KindValue {
    Import,
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Impl,
    TypeAlias,
    Const,
    Static,
    Field,
    Interface,
    EnumVariant,
    Module,
    Macro,
    Test,
    Unknown,
}

impl From<SymbolKind> for KindValue {
    fn from(kind: SymbolKind) -> Self {
        match kind {
            SymbolKind::Import => Self::Import,
            SymbolKind::Function => Self::Function,
            SymbolKind::Method => Self::Method,
            SymbolKind::Class => Self::Class,
            SymbolKind::Struct => Self::Struct,
            SymbolKind::Enum => Self::Enum,
            SymbolKind::Trait => Self::Trait,
            SymbolKind::Impl => Self::Impl,
            SymbolKind::TypeAlias => Self::TypeAlias,
            SymbolKind::Const => Self::Const,
            SymbolKind::Static => Self::Static,
            SymbolKind::Field => Self::Field,
            SymbolKind::Interface => Self::Interface,
            SymbolKind::EnumVariant => Self::EnumVariant,
            SymbolKind::Module => Self::Module,
            SymbolKind::Macro => Self::Macro,
            SymbolKind::Test => Self::Test,
            SymbolKind::Unknown => Self::Unknown,
        }
    }
}

impl From<KindValue> for SymbolKind {
    fn from(kind: KindValue) -> Self {
        match kind {
            KindValue::Import => Self::Import,
            KindValue::Function => Self::Function,
            KindValue::Method => Self::Method,
            KindValue::Class => Self::Class,
            KindValue::Struct => Self::Struct,
            KindValue::Enum => Self::Enum,
            KindValue::Trait => Self::Trait,
            KindValue::Impl => Self::Impl,
            KindValue::TypeAlias => Self::TypeAlias,
            KindValue::Const => Self::Const,
            KindValue::Static => Self::Static,
            KindValue::Field => Self::Field,
            KindValue::Interface => Self::Interface,
            KindValue::EnumVariant => Self::EnumVariant,
            KindValue::Module => Self::Module,
            KindValue::Macro => Self::Macro,
            KindValue::Test => Self::Test,
            KindValue::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, wincode::SchemaRead, wincode::SchemaWrite)]
pub(crate) struct DigestValue {
    hash: u64,
    byte_len: usize,
    line_count: usize,
}

impl From<&SymbolDigest> for DigestValue {
    fn from(digest: &SymbolDigest) -> Self {
        Self {
            hash: digest.hash.as_u64(),
            byte_len: digest.byte_len,
            line_count: digest.line_count,
        }
    }
}

impl From<DigestValue> for SymbolDigest {
    fn from(digest: DigestValue) -> Self {
        Self {
            hash: RapidHash::from_u64(digest.hash),
            byte_len: digest.byte_len,
            line_count: digest.line_count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, wincode::SchemaRead, wincode::SchemaWrite)]
pub(crate) struct SymbolValue {
    kind: KindValue,
    name: String,
    qualified_name: String,
    signature: String,
    start_line: usize,
    end_line: usize,
    start_byte: usize,
    end_byte: usize,
    body_start_line: Option<usize>,
    body_end_line: Option<usize>,
    parent: Option<String>,
    children: Vec<SymbolValue>,
    digest: DigestValue,
}

impl From<&SymbolEntry> for SymbolValue {
    fn from(entry: &SymbolEntry) -> Self {
        Self {
            kind: entry.kind.into(),
            name: entry.name.clone(),
            qualified_name: entry.qualified_name.clone(),
            signature: entry.signature.clone(),
            start_line: entry.start_line,
            end_line: entry.end_line,
            start_byte: entry.start_byte,
            end_byte: entry.end_byte,
            body_start_line: entry.body_start_line,
            body_end_line: entry.body_end_line,
            parent: entry.parent.clone(),
            children: entry.children.iter().map(Self::from).collect(),
            digest: (&entry.digest).into(),
        }
    }
}

impl From<SymbolValue> for SymbolEntry {
    fn from(value: SymbolValue) -> Self {
        Self {
            kind: value.kind.into(),
            name: value.name,
            qualified_name: value.qualified_name,
            signature: value.signature,
            start_line: value.start_line,
            end_line: value.end_line,
            start_byte: value.start_byte,
            end_byte: value.end_byte,
            body_start_line: value.body_start_line,
            body_end_line: value.body_end_line,
            parent: value.parent,
            children: value.children.into_iter().map(Self::from).collect(),
            digest: value.digest.into(),
        }
    }
}

/// Outcome of extracting the symbols of one file, stored as data so a broken file never
/// fails an Ajika operation.
#[derive(Debug, Clone, PartialEq, Eq, wincode::SchemaRead, wincode::SchemaWrite)]
pub(crate) enum SymbolsResult {
    Symbols(Vec<SymbolValue>),
    UnsupportedExtension(String),
    UnsupportedLanguage(String),
    ParseFailed(String),
    /// An I/O failure reported by the extractor.
    Io(String),
    /// The file could not be read as UTF-8 text.
    Unreadable,
}

impl SymbolsResult {
    /// Extract symbols for `path` (its extension selects the language) from `source`.
    pub(crate) fn compute(path: &Path, source: &str) -> Self {
        match SymbolIndex::from_source_for_path(path, source) {
            Ok(index) => Self::from_symbols(&index.symbols),
            Err(error) => Self::from_error(error),
        }
    }

    pub(crate) fn from_symbols(symbols: &[SymbolEntry]) -> Self {
        Self::Symbols(symbols.iter().map(SymbolValue::from).collect())
    }

    fn from_error(error: SymbolError) -> Self {
        match error {
            SymbolError::UnsupportedExtension(ext) => Self::UnsupportedExtension(ext),
            SymbolError::UnsupportedLanguage(language) => Self::UnsupportedLanguage(language),
            SymbolError::ParseError(message) => Self::ParseFailed(message),
            SymbolError::Io(message) => Self::Io(message),
        }
    }

    /// Convert back to what `SymbolIndex::from_source_for_path` returns for the same text.
    ///
    /// # Errors
    ///
    /// Returns the [`SymbolError`] the direct parse reports for the same input.
    pub(crate) fn to_index(&self) -> Result<SymbolIndex, SymbolError> {
        match self {
            Self::Symbols(symbols) => Ok(SymbolIndex {
                symbols: symbols.iter().cloned().map(SymbolEntry::from).collect(),
            }),
            Self::UnsupportedExtension(ext) => Err(SymbolError::UnsupportedExtension(ext.clone())),
            Self::UnsupportedLanguage(language) => {
                Err(SymbolError::UnsupportedLanguage(language.clone()))
            }
            Self::ParseFailed(message) => Err(SymbolError::ParseError(message.clone())),
            Self::Io(message) => Err(SymbolError::Io(message.clone())),
            Self::Unreadable => Err(SymbolError::Io(
                "stream did not contain valid UTF-8".to_owned(),
            )),
        }
    }
}

/// A function entry without its file path (the cache key supplies it) and without the
/// TypeScript `AstFingerprint` (recomputed on load).
#[derive(Debug, Clone, PartialEq, Eq, wincode::SchemaRead, wincode::SchemaWrite)]
pub(crate) struct FunctionValue {
    name: String,
    start_line: u32,
    end_line: u32,
    structural_fingerprint: u64,
    body_text: String,
    language: String,
}

impl From<&IndexedFunctionEntry> for FunctionValue {
    fn from(entry: &IndexedFunctionEntry) -> Self {
        Self {
            name: entry.name.clone(),
            start_line: entry.start_line,
            end_line: entry.end_line,
            structural_fingerprint: entry.structural_fingerprint,
            body_text: entry.body_text.clone(),
            language: entry.language.clone(),
        }
    }
}

impl FunctionValue {
    /// Rebuild the public entry for `file_path`, recomputing the TypeScript fingerprint.
    ///
    /// Entries were only stored after their fingerprint computed successfully, so a failure
    /// here is not expected; it falls back to an empty fingerprint, as non-TypeScript
    /// entries carry.
    pub(crate) fn into_entry(self, file_path: PathBuf) -> IndexedFunctionEntry {
        let fingerprint = if self.language == FINGERPRINT_LANGUAGE {
            AstFingerprint::from_source(&self.body_text).unwrap_or_else(|error| {
                log::debug!(
                    "code cache: fingerprint recompute failed for '{}' in {file_path:?}: {error}",
                    self.name
                );
                AstFingerprint::new()
            })
        } else {
            AstFingerprint::new()
        };
        IndexedFunctionEntry {
            name: self.name,
            file_path,
            start_line: self.start_line,
            end_line: self.end_line,
            fingerprint,
            structural_fingerprint: self.structural_fingerprint,
            body_text: self.body_text,
            language: self.language,
        }
    }
}

/// Function entries of one file; empty for skipped, oversized, unsupported or failed files.
#[derive(Debug, Clone, Default, PartialEq, Eq, wincode::SchemaRead, wincode::SchemaWrite)]
pub(crate) struct FunctionsResult {
    pub(crate) entries: Vec<FunctionValue>,
}

impl FunctionsResult {
    pub(crate) fn from_entries(entries: &[IndexedFunctionEntry]) -> Self {
        Self {
            entries: entries.iter().map(FunctionValue::from).collect(),
        }
    }

    /// Rebuild public entries for `file_path`.
    pub(crate) fn to_entries(&self, file_path: &Path) -> Vec<IndexedFunctionEntry> {
        self.entries
            .iter()
            .cloned()
            .map(|entry| entry.into_entry(file_path.to_path_buf()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::function_index::{FunctionIndexConfig, index_file_entries};
    use crate::symbol_index::fixtures::multi_language_fixtures;

    fn wincode_round_trip<T>(value: &T) -> T
    where
        T: wincode::SchemaWrite<wincode::config::DefaultConfig, Src = T>
            + wincode::SchemaReadOwned<wincode::config::DefaultConfig, Dst = T>,
    {
        let bytes = wincode::serialize(value).expect("serialize");
        wincode::deserialize_exact(&bytes).expect("deserialize")
    }

    #[test]
    fn symbols_round_trip_equals_direct_parse_for_every_fixture() {
        for (language, source) in multi_language_fixtures() {
            let direct = SymbolIndex::from_source(source, language).expect(language);
            assert!(!direct.symbols.is_empty(), "{language} fixture has symbols");

            let result = SymbolsResult::from_symbols(&direct.symbols);
            let restored = wincode_round_trip(&result);
            assert_eq!(restored, result, "{language}: wincode round trip");

            let index = restored.to_index().expect(language);
            assert_eq!(
                index.symbols, direct.symbols,
                "{language}: mirror -> public"
            );
        }
    }

    fn nesting_depth(symbols: &[SymbolEntry]) -> usize {
        symbols
            .iter()
            .map(|symbol| 1 + nesting_depth(&symbol.children))
            .max()
            .unwrap_or(0)
    }

    /// The extractors bound how deep nested configuration becomes symbol children, so the
    /// recursive conversions and drops on the owner thread cannot exhaust its stack.
    #[test]
    fn deeply_nested_config_files_yield_bounded_symbol_depth() {
        let depth = 2000;
        let json = format!("{}1{}", "{\"k\":".repeat(depth), "}".repeat(depth),);
        // Deeper than the cap but within the YAML grammar's own indentation-stack limit.
        let yaml: String = (0..200).map(|n| format!("{}k:\n", " ".repeat(n))).collect();
        for (path, source) in [("deep.json", json), ("deep.yaml", yaml)] {
            let direct = SymbolIndex::from_source_for_path(Path::new(path), &source)
                .unwrap_or_else(|error| panic!("{path}: {error:?}"));
            let found = nesting_depth(&direct.symbols);
            assert!(found > 1, "{path}: nesting is still reported");
            assert!(found <= 130, "{path}: depth {found} is bounded");

            let restored = SymbolsResult::from_symbols(&direct.symbols)
                .to_index()
                .expect(path);
            assert_eq!(restored.symbols, direct.symbols, "{path}: round trip");
        }
    }

    #[test]
    fn symbols_result_preserves_errors_of_the_direct_parse() {
        let unsupported = Path::new("notes.unknownext");
        let direct =
            SymbolIndex::from_source_for_path(unsupported, "x").expect_err("unsupported extension");
        let cached = SymbolsResult::compute(unsupported, "x")
            .to_index()
            .expect_err("unsupported extension");
        assert_eq!(cached.to_string(), direct.to_string());
    }

    #[test]
    fn compute_matches_direct_parse_by_extension() {
        let (_, source) = multi_language_fixtures()
            .into_iter()
            .find(|(language, _)| *language == "rust")
            .expect("rust fixture");
        let path = Path::new("src/lib.rs");
        let direct = SymbolIndex::from_source_for_path(path, source).expect("direct");
        let cached = SymbolsResult::compute(path, source)
            .to_index()
            .expect("cached");
        assert_eq!(cached.symbols, direct.symbols);
    }

    #[test]
    fn typescript_function_fingerprint_is_restored_after_round_trip() {
        let source = r#"export function total(items: number[]): number {
    let sum = 0;
    for (const item of items) {
        sum += item * 2;
    }
    return sum;
}
"#;
        let path = Path::new("src/total.ts");
        let config = FunctionIndexConfig::default();
        let direct = index_file_entries(path, source, &config);
        assert_eq!(direct.len(), 1, "one TypeScript function indexed");

        let stored = wincode_round_trip(&FunctionsResult::from_entries(&direct));
        let restored = stored.to_entries(path);

        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].name, direct[0].name);
        assert_eq!(restored[0].file_path, direct[0].file_path);
        assert_eq!(restored[0].body_text, direct[0].body_text);
        assert_eq!(
            restored[0].fingerprint.similarity(&restored[0].fingerprint),
            1.0
        );
        assert_eq!(
            restored[0].fingerprint.similarity(&direct[0].fingerprint),
            1.0,
            "recomputed fingerprint equals the original"
        );
    }

    #[test]
    fn rust_function_entries_round_trip_without_loss() {
        let source = r#"pub fn compute(values: &[i32]) -> i32 {
    let mut total = 0;
    for value in values {
        total += value;
    }
    total
}
"#;
        let path = Path::new("src/compute.rs");
        let direct = index_file_entries(path, source, &FunctionIndexConfig::default());
        assert_eq!(direct.len(), 1);
        assert_ne!(direct[0].structural_fingerprint, 0);

        let restored = wincode_round_trip(&FunctionsResult::from_entries(&direct)).to_entries(path);
        assert_eq!(restored.len(), 1);
        assert_eq!(
            restored[0].structural_fingerprint,
            direct[0].structural_fingerprint
        );
        assert_eq!(restored[0].start_line, direct[0].start_line);
        assert_eq!(restored[0].end_line, direct[0].end_line);
        assert_eq!(restored[0].language, direct[0].language);
    }
}
