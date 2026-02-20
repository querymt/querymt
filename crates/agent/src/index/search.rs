use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, sinks::Lossy};
use ignore::WalkBuilder;
use std::error::Error;
use std::path::Path;

pub struct CodeSearch;

impl CodeSearch {
    pub fn search(root: &Path, pattern: &str) -> Result<Vec<String>, Box<dyn Error + Send>> {
        let matcher =
            RegexMatcher::new(pattern).map_err(|e| Box::new(e) as Box<dyn Error + Send>)?;
        let mut matches = vec![];

        // TODO: Consider consolidating with file_index.rs's Override pattern for consistency
        // Currently using .standard_filters() which respects .gitignore and common ignore patterns
        for result in WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .standard_filters(true)
            .build()
        {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            let path = entry.path().to_owned();

            Searcher::new()
                .search_path(
                    &matcher,
                    &path,
                    Lossy(|lnum, line| {
                        matches.push(format!("{}:{}: {}", path.display(), lnum, line));
                        Ok(true)
                    }),
                )
                .map_err(|e| Box::new(e) as Box<dyn Error + Send>)?;
        }

        Ok(matches)
    }
}

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("main.rs"),
            "fn main() {\n    println!(\"hello world\");\n}\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("README.md"),
            "# My Project\nThis is a test project.\n",
        )
        .unwrap();
        tmp
    }

    // ── Basic search ────────────────────────────────────────────────────────

    #[test]
    fn test_search_finds_pattern_in_file() {
        let tmp = setup_dir();
        let results = CodeSearch::search(tmp.path(), "fn main").unwrap();
        assert!(!results.is_empty(), "should find 'fn main'");
        assert!(
            results.iter().any(|r| r.contains("main.rs")),
            "match should be in main.rs"
        );
    }

    #[test]
    fn test_search_no_match_returns_empty() {
        let tmp = setup_dir();
        let results = CodeSearch::search(tmp.path(), "ZZZNOMATCH99999").unwrap();
        assert!(
            results.is_empty(),
            "should find no matches for unique pattern"
        );
    }

    #[test]
    fn test_search_pattern_across_multiple_files() {
        let tmp = setup_dir();
        // "i32" appears in lib.rs
        let results = CodeSearch::search(tmp.path(), r"i32").unwrap();
        assert!(!results.is_empty());
        assert!(
            results.iter().any(|r| r.contains("lib.rs")),
            "should find 'i32' in lib.rs"
        );
    }

    #[test]
    fn test_search_includes_line_numbers() {
        let tmp = setup_dir();
        let results = CodeSearch::search(tmp.path(), "hello world").unwrap();
        assert!(!results.is_empty());
        // Format is "path:lnum: content"
        let first = &results[0];
        // Should contain a colon-separated number (the line number)
        assert!(
            first.contains(':'),
            "result should contain line number: {}",
            first
        );
    }

    #[test]
    fn test_search_empty_directory_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let results = CodeSearch::search(tmp.path(), "anything").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_invalid_regex_returns_error() {
        let tmp = setup_dir();
        // An invalid regex pattern
        let result = CodeSearch::search(tmp.path(), "[invalid(regex");
        assert!(result.is_err(), "invalid regex should return an error");
    }

    #[test]
    fn test_search_regex_pattern() {
        let tmp = setup_dir();
        // Regex: match 'pub fn' or 'fn main'
        let results = CodeSearch::search(tmp.path(), r"(pub fn|fn main)").unwrap();
        assert!(
            results.len() >= 2,
            "should find at least 'pub fn add' and 'fn main'"
        );
    }

    #[test]
    fn test_search_result_contains_line_content() {
        let tmp = setup_dir();
        let results = CodeSearch::search(tmp.path(), "println").unwrap();
        assert!(!results.is_empty());
        assert!(
            results[0].contains("println"),
            "result should contain the matched line content"
        );
    }

    #[test]
    fn test_search_nested_directory() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("src").join("utils");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("helper.rs"),
            "pub fn helper() -> &'static str { \"help\" }\n",
        )
        .unwrap();

        let results = CodeSearch::search(tmp.path(), "helper").unwrap();
        assert!(!results.is_empty(), "should find 'helper' in nested file");
        assert!(results.iter().any(|r| r.contains("helper.rs")));
    }
}
