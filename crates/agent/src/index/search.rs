use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, sinks::Lossy};
use ignore::WalkBuilder;
use std::error::Error;
use std::path::Path;

pub struct CodeSearch;

impl CodeSearch {
    pub fn search(root: &Path, pattern: &str) -> Result<Vec<String>, Box<dyn Error>> {
        let matcher = RegexMatcher::new(pattern)?;
        let mut matches = vec![];

        for result in WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .build()
        {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                continue;
            }

            let path = entry.path().to_owned();

            Searcher::new().search_path(
                &matcher,
                &path,
                Lossy(|lnum, line| {
                    matches.push(format!("{}:{}: {}", path.display(), lnum, line));
                    Ok(true)
                }),
            )?;
        }

        Ok(matches)
    }
}
