//! Edit tool with fuzzy matching strategies

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool as ChatTool};
use serde_json::{Value, json};

use crate::tools::{CapabilityRequirement, Tool, ToolContext, ToolError};

/// Edit tool for precise string replacement with fuzzy matching
pub struct EditTool;

impl EditTool {
    pub fn new() -> Self {
        Self
    }

    /// Simple exact match replacer
    fn simple_replacer(content: &str, find: &str) -> Vec<String> {
        if content.contains(find) {
            vec![find.to_string()]
        } else {
            vec![]
        }
    }

    /// Line-trimmed replacer: matches lines with trimmed content
    fn line_trimmed_replacer(content: &str, find: &str) -> Vec<String> {
        let mut matches = Vec::new();
        let original_lines: Vec<&str> = content.lines().collect();
        let mut search_lines: Vec<&str> = find.lines().collect();

        // Remove trailing empty line
        if search_lines.last() == Some(&"") {
            search_lines.pop();
        }

        if search_lines.is_empty() {
            return matches;
        }

        // Avoid indexing past the available content lines when the search block is longer.
        if original_lines.len() < search_lines.len() {
            return matches;
        }

        for i in 0..=(original_lines.len().saturating_sub(search_lines.len())) {
            let mut is_match = true;

            for j in 0..search_lines.len() {
                let original_trimmed = original_lines[i + j].trim();
                let search_trimmed = search_lines[j].trim();

                if original_trimmed != search_trimmed {
                    is_match = false;
                    break;
                }
            }

            if is_match {
                // Calculate character indices
                let mut match_start = 0;
                for orig_line in original_lines.iter().take(i) {
                    match_start += orig_line.len() + 1; // +1 for newline
                }

                let mut match_end = match_start;
                for k in 0..search_lines.len() {
                    match_end += original_lines[i + k].len();
                    if k < search_lines.len() - 1 {
                        match_end += 1; // Add newline except for last line
                    }
                }

                matches.push(content[match_start..match_end].to_string());
            }
        }

        matches
    }

    /// Block anchor replacer: fuzzy matching using first and last line anchors
    fn block_anchor_replacer(content: &str, find: &str) -> Vec<String> {
        const SINGLE_CANDIDATE_THRESHOLD: f64 = 0.3;
        const MULTIPLE_CANDIDATES_THRESHOLD: f64 = 0.5;

        let mut matches = Vec::new();
        let original_lines: Vec<&str> = content.lines().collect();
        let mut search_lines: Vec<&str> = find.lines().collect();

        if search_lines.len() < 3 {
            return matches;
        }

        if search_lines.last() == Some(&"") {
            search_lines.pop();
        }

        let first_line_search = search_lines[0].trim();
        let last_line_search = search_lines[search_lines.len() - 1].trim();
        let search_block_size = search_lines.len();

        // Collect candidates
        let mut candidates = Vec::new();
        for i in 0..original_lines.len() {
            if original_lines[i].trim() != first_line_search {
                continue;
            }

            for (j, orig_line) in original_lines.iter().enumerate().skip(i + 2) {
                if orig_line.trim() == last_line_search {
                    candidates.push((i, j));
                    break;
                }
            }
        }

        if candidates.is_empty() {
            return matches;
        }

        // Helper to calculate similarity
        let calc_similarity = |start_line: usize, end_line: usize| -> f64 {
            let actual_block_size = end_line - start_line + 1;

            // Reject if search block is much larger than actual block
            if search_block_size > actual_block_size + 1 {
                return 0.0;
            }

            let lines_to_check = (search_block_size - 2).min(actual_block_size - 2);

            if lines_to_check == 0 {
                return 1.0;
            }

            let mut similarity = 0.0;
            for j in 1..search_block_size - 1 {
                if j >= actual_block_size - 1 {
                    break;
                }
                let original_line = original_lines[start_line + j].trim();
                let search_line = search_lines[j].trim();
                let max_len = original_line.len().max(search_line.len());

                if max_len == 0 {
                    continue;
                }

                let distance = strsim::levenshtein(original_line, search_line);
                similarity += (1.0 - distance as f64 / max_len as f64) / lines_to_check as f64;
            }

            similarity
        };

        let extract_match = |start_line: usize, end_line: usize| -> String {
            let mut match_start = 0;
            for orig_line in original_lines.iter().take(start_line) {
                match_start += orig_line.len() + 1;
            }
            let mut match_end = match_start;
            for (k, orig_line) in original_lines
                .iter()
                .enumerate()
                .take(end_line + 1)
                .skip(start_line)
            {
                match_end += orig_line.len();
                if k < end_line {
                    match_end += 1;
                }
            }
            content[match_start..match_end].to_string()
        };

        // Single candidate
        if candidates.len() == 1 {
            let (start_line, end_line) = candidates[0];
            let similarity = calc_similarity(start_line, end_line);
            if similarity >= SINGLE_CANDIDATE_THRESHOLD {
                matches.push(extract_match(start_line, end_line));
            }
            return matches;
        }

        // Multiple candidates - find best match
        let mut best_match: Option<(usize, usize)> = None;
        let mut max_similarity = -1.0;

        for &(start_line, end_line) in &candidates {
            let similarity = calc_similarity(start_line, end_line);
            if similarity > max_similarity {
                max_similarity = similarity;
                best_match = Some((start_line, end_line));
            }
        }

        if max_similarity >= MULTIPLE_CANDIDATES_THRESHOLD
            && let Some((start_line, end_line)) = best_match
        {
            matches.push(extract_match(start_line, end_line));
        }

        matches
    }

    /// Whitespace-normalized replacer
    fn whitespace_normalized_replacer(content: &str, find: &str) -> Vec<String> {
        let mut matches = Vec::new();
        let normalize = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let normalized_find = normalize(find);

        let find_lines: Vec<&str> = find.lines().collect();

        // If find has multiple lines, only do multi-line matching
        if find_lines.len() > 1 {
            let lines: Vec<&str> = content.lines().collect();
            // Only process if content has enough lines
            if lines.len() >= find_lines.len() {
                for i in 0..=(lines.len() - find_lines.len()) {
                    let block = lines[i..i + find_lines.len()].join("\n");
                    if normalize(&block) == normalized_find {
                        matches.push(block);
                    }
                }
            }
        } else {
            // Single line matches only when find is single line
            for line in content.lines() {
                if normalize(line) == normalized_find {
                    matches.push(line.to_string());
                }
            }
        }

        matches
    }

    /// Indentation-flexible replacer
    fn indentation_flexible_replacer(content: &str, find: &str) -> Vec<String> {
        let mut matches = Vec::new();

        let remove_indentation = |text: &str| -> String {
            let lines: Vec<&str> = text.lines().collect();
            let non_empty_lines: Vec<&str> = lines
                .iter()
                .filter(|l| !l.trim().is_empty())
                .copied()
                .collect();

            if non_empty_lines.is_empty() {
                return text.to_string();
            }

            let min_indent = non_empty_lines
                .iter()
                .map(|line| line.len() - line.trim_start().len())
                .min()
                .unwrap_or(0);

            lines
                .iter()
                .map(|line| {
                    if line.trim().is_empty() {
                        *line
                    } else {
                        &line[min_indent.min(line.len())..]
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let normalized_find = remove_indentation(find);
        let content_lines: Vec<&str> = content.lines().collect();
        let find_lines: Vec<&str> = find.lines().collect();

        if find_lines.is_empty() || content_lines.len() < find_lines.len() {
            return matches;
        }

        for i in 0..=(content_lines.len().saturating_sub(find_lines.len())) {
            let block = content_lines[i..i + find_lines.len()].join("\n");
            if remove_indentation(&block) == normalized_find {
                matches.push(block);
            }
        }

        matches
    }

    /// Escape-normalized replacer
    fn escape_normalized_replacer(content: &str, find: &str) -> Vec<String> {
        let mut matches = Vec::new();

        let unescape = |s: &str| -> String {
            s.replace("\\n", "\n")
                .replace("\\t", "\t")
                .replace("\\r", "\r")
                .replace("\\'", "'")
                .replace("\\\"", "\"")
                .replace("\\`", "`")
                .replace("\\\\", "\\")
                .replace("\\$", "$")
        };

        let unescaped_find = unescape(find);

        // Try finding escaped versions by searching through blocks
        let lines: Vec<&str> = content.lines().collect();
        let find_lines: Vec<&str> = unescaped_find.lines().collect();

        if find_lines.is_empty() || lines.len() < find_lines.len() {
            return matches;
        }

        for i in 0..=(lines.len().saturating_sub(find_lines.len())) {
            let block = lines[i..i + find_lines.len()].join("\n");
            if unescape(&block) == unescaped_find {
                // Only add if not already present (deduplicate)
                if !matches.contains(&block) {
                    matches.push(block);
                }
            }
        }

        // If no matches found via block search, try direct substring match
        if matches.is_empty() && content.contains(&unescaped_find) {
            matches.push(unescaped_find);
        }

        matches
    }

    /// Trimmed boundary replacer
    fn trimmed_boundary_replacer(content: &str, find: &str) -> Vec<String> {
        let mut matches = Vec::new();
        let trimmed_find = find.trim();

        if trimmed_find == find {
            return matches; // Already trimmed, no point trying
        }

        // Try finding blocks where trimmed content matches
        let lines: Vec<&str> = content.lines().collect();
        let find_lines: Vec<&str> = find.lines().collect();

        if find_lines.is_empty() || lines.len() < find_lines.len() {
            return matches;
        }

        for i in 0..=(lines.len().saturating_sub(find_lines.len())) {
            let block = lines[i..i + find_lines.len()].join("\n");
            if block.trim() == trimmed_find {
                // Return the original block with whitespace, not the trimmed version
                matches.push(block);
            }
        }

        matches
    }

    /// Context-aware replacer
    fn context_aware_replacer(content: &str, find: &str) -> Vec<String> {
        let mut matches = Vec::new();
        let mut find_lines: Vec<&str> = find.lines().collect();

        if find_lines.len() < 3 {
            return matches;
        }

        if find_lines.last() == Some(&"") {
            find_lines.pop();
        }

        let content_lines: Vec<&str> = content.lines().collect();
        let first_line = find_lines[0].trim();
        let last_line = find_lines[find_lines.len() - 1].trim();

        for i in 0..content_lines.len() {
            if content_lines[i].trim() != first_line {
                continue;
            }

            for j in (i + 2)..content_lines.len() {
                if content_lines[j].trim() == last_line {
                    let block_lines = &content_lines[i..=j];

                    if block_lines.len() == find_lines.len() {
                        let mut matching_lines = 0;
                        let mut total_non_empty = 0;

                        for k in 1..block_lines.len() - 1 {
                            let block_line = block_lines[k].trim();
                            let find_line = find_lines[k].trim();

                            if !block_line.is_empty() || !find_line.is_empty() {
                                total_non_empty += 1;
                                if block_line == find_line {
                                    matching_lines += 1;
                                }
                            }
                        }

                        if total_non_empty == 0
                            || matching_lines as f64 / total_non_empty as f64 >= 0.5
                        {
                            matches.push(block_lines.join("\n"));
                            break;
                        }
                    }
                    break;
                }
            }
        }

        matches
    }

    /// Multi-occurrence replacer (for replaceAll)
    fn multi_occurrence_replacer(content: &str, find: &str) -> Vec<String> {
        let mut matches = Vec::new();
        let mut start = 0;

        while let Some(idx) = content[start..].find(find) {
            matches.push(find.to_string());
            start += idx + find.len();
        }

        matches
    }

    /// Main replace function that tries all strategies
    pub fn replace(
        content: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<String, String> {
        if old_string.is_empty() {
            return Err("oldString cannot be empty".to_string());
        }
        if old_string == new_string {
            return Err("oldString and newString must be different".to_string());
        }

        let replacers: Vec<fn(&str, &str) -> Vec<String>> = vec![
            Self::simple_replacer,
            Self::line_trimmed_replacer,
            Self::block_anchor_replacer,
            Self::whitespace_normalized_replacer,
            Self::indentation_flexible_replacer,
            Self::escape_normalized_replacer,
            Self::trimmed_boundary_replacer,
            Self::context_aware_replacer,
            Self::multi_occurrence_replacer,
        ];

        let mut found_any = false;

        for replacer in replacers {
            let matches = replacer(content, old_string);

            for search in matches {
                if let Some(idx) = content.find(&search) {
                    found_any = true;

                    if replace_all {
                        return Ok(content.replace(&search, new_string));
                    }

                    // Check if there's only one occurrence
                    let last_idx = content.rfind(&search).unwrap();
                    if idx == last_idx {
                        // Single occurrence - safe to replace
                        let mut result = String::with_capacity(content.len());
                        result.push_str(&content[..idx]);
                        result.push_str(new_string);
                        result.push_str(&content[idx + search.len()..]);
                        return Ok(result);
                    }
                    // Multiple occurrences found, continue to next replacer
                }
            }
        }

        if !found_any {
            Err("oldString not found in content".to_string())
        } else {
            Err("oldString found multiple times and requires more code context to uniquely identify the intended match. Either provide a larger string with more surrounding context to make it unique or use `replaceAll` to change every instance of `oldString`.".to_string())
        }
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Performs exact string replacements in files. \n\n\
                    Usage:\n\
                    - You must use your `read_tool` tool at least once in the conversation before editing. This tool will error if you attempt an edit without reading the file. \n\
                    - When editing text from read_tool tool output, ensure you preserve the exact indentation (tabs/spaces) as it appears AFTER the line number prefix. The line number prefix format is '00001| ' (5 digits + pipe + space). Everything after '| ' is the actual file content to match. Never include any part of the line number prefix in the oldString or newString.\n\
                    - ALWAYS prefer editing existing files in the codebase. NEVER write new files unless explicitly required.\n\
                    - Only use emojis if the user explicitly requests it. Avoid adding emojis to files unless asked.\n\
                    - The edit will FAIL if `oldString` is not found in the file with an error \"oldString not found in content\".\n\
                    - The edit will FAIL if `oldString` is found multiple times in the file with an error \"oldString found multiple times and requires more code context to uniquely identify the intended match\". Either provide a larger string with more surrounding context to make it unique or use `replaceAll` to change every instance of `oldString`. \n\
                    - Use `replaceAll` for replacing and renaming strings across the file. This parameter is useful if you want to rename a variable for instance."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "filePath": {
                            "type": "string",
                            "description": "The absolute path to the file to modify"
                        },
                        "oldString": {
                            "type": "string",
                            "description": "The text to replace"
                        },
                        "newString": {
                            "type": "string",
                            "description": "The text to replace it with (must be different from oldString)"
                        },
                        "replaceAll": {
                            "type": "boolean",
                            "description": "Replace all occurrences of oldString (default false)",
                            "default": false
                        }
                    },
                    "required": ["filePath", "oldString", "newString"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        if context.is_read_only() {
            return Err(ToolError::PermissionDenied(
                "Session is in read-only mode — file edits are not allowed".to_string(),
            ));
        }

        let file_path_str = args
            .get("filePath")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("filePath is required".to_string()))?;

        let old_string = args
            .get("oldString")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("oldString is required".to_string()))?;

        let new_string = args
            .get("newString")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("newString is required".to_string()))?;

        let replace_all = args
            .get("replaceAll")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if old_string == new_string {
            return Err(ToolError::InvalidRequest(
                "oldString and newString must be different".to_string(),
            ));
        }

        let file_path = context.resolve_path(file_path_str)?;

        // Read current content
        let content = tokio::fs::read_to_string(&file_path)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Failed to read file: {}", e)))?;

        // Perform replacement
        let new_content = Self::replace(&content, old_string, new_string, replace_all)
            .map_err(ToolError::ProviderError)?;

        // Write new content
        tokio::fs::write(&file_path, &new_content)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Failed to write file: {}", e)))?;

        // Generate diff for output
        let result = json!({
            "success": true,
            "file": file_path.display().to_string(),
            "replaced": if replace_all { "all occurrences" } else { "single occurrence" },
        });

        serde_json::to_string_pretty(&result)
            .map_err(|e| ToolError::ProviderError(format!("Failed to serialize result: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_edit_simple() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "hello world\nrust is great").unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = EditTool::new();

        let args = json!({
            "filePath": file_path.display().to_string(),
            "oldString": "rust is great",
            "newString": "rust is awesome"
        });

        let result = tool.call(args, &context).await.unwrap();
        assert!(result.contains("success"));

        let new_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(new_content, "hello world\nrust is awesome");
    }

    #[tokio::test]
    async fn test_edit_replace_all() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "foo bar foo baz foo").unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = EditTool::new();

        let args = json!({
            "filePath": file_path.display().to_string(),
            "oldString": "foo",
            "newString": "qux",
            "replaceAll": true
        });

        let result = tool.call(args, &context).await.unwrap();
        assert!(result.contains("success"));

        let new_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(new_content, "qux bar qux baz qux");
    }

    #[test]
    fn test_line_trimmed_replacer() {
        let content = "  hello world  \n  rust is great  \n  goodbye  ";
        let find = "hello world\nrust is great";
        let matches = EditTool::line_trimmed_replacer(content, find);
        assert_eq!(matches.len(), 1);
        assert!(matches[0].contains("hello world"));
    }

    #[test]
    fn test_levenshtein() {
        assert_eq!(strsim::levenshtein("", ""), 0);
        assert_eq!(strsim::levenshtein("hello", "hello"), 0);
        assert_eq!(strsim::levenshtein("hello", "hallo"), 1);
        assert_eq!(strsim::levenshtein("kitten", "sitting"), 3);
    }

    // BUG #2: escape_normalized_replacer returns duplicate matches
    #[test]
    fn test_escape_normalized_duplicates() {
        let content = "hello\nworld";
        let find = "hello\\nworld";
        let matches = EditTool::escape_normalized_replacer(content, find);
        // Should return unique matches, not duplicates
        assert_eq!(
            matches.len(),
            1,
            "Should only have one match, not duplicates"
        );
    }

    // BUG #3: block_anchor_replacer potential index out of bounds
    #[test]
    fn test_block_anchor_size_mismatch() {
        let content = "fn foo() {\n    bar();\n}";
        // Search block is longer than actual block
        let find = "fn foo() {\n    bar();\n    baz();\n    qux();\n    extra();\n}";
        // Should not panic
        let matches = EditTool::block_anchor_replacer(content, find);
        assert_eq!(matches.len(), 0);
    }

    // BUG #4: Newline handling in line_trimmed_replacer (file without trailing newline)
    #[test]
    fn test_line_trimmed_no_trailing_newline() {
        let content = "hello world\nrust is great"; // No trailing newline
        let find = "rust is great";
        let matches = EditTool::line_trimmed_replacer(content, find);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], "rust is great");
    }

    // BUG #5: trimmed_boundary_replacer returns trimmed version instead of original
    #[test]
    fn test_trimmed_boundary_wrong_match() {
        let content = "  hello world  \nrust is great";
        let find = "  hello world  ";
        let matches = EditTool::trimmed_boundary_replacer(content, find);
        if !matches.is_empty() {
            // Should return the original with spaces, not trimmed
            assert_eq!(
                matches[0], "  hello world  ",
                "Should preserve original whitespace"
            );
        }
    }

    // BUG #6: whitespace_normalized_replacer matches single line when expecting multi-line
    #[test]
    fn test_whitespace_normalized_wrong_line_count() {
        let content = "hello world rust is great\nfoo bar";
        let find = "hello\nworld\nrust\nis\ngreat";
        let matches = EditTool::whitespace_normalized_replacer(content, find);
        // Should only match if the line count is correct
        for m in &matches {
            let match_lines = m.lines().count();
            let find_lines = find.lines().count();
            assert_eq!(
                match_lines, find_lines,
                "Match should have same number of lines as find string"
            );
        }
    }

    // BUG #7: SINGLE_CANDIDATE_THRESHOLD of 0.0 is too permissive
    #[test]
    fn test_block_anchor_too_permissive() {
        let content = "fn foo() {\n    completely_different();\n    totally_wrong();\n}";
        let find = "fn foo() {\n    bar();\n    baz();\n}";
        let matches = EditTool::block_anchor_replacer(content, find);
        // With threshold 0.0, this might incorrectly match even though content is different
        // This test documents the current behavior (which is arguably a bug)
        if !matches.is_empty() {
            println!("WARNING: block_anchor matched despite very different content");
        }
    }

    // BUG #9: Empty oldString should be rejected
    #[test]
    fn test_empty_old_string() {
        let content = "hello world";
        let result = EditTool::replace(content, "", "replacement", false);
        // Should return an error, not try to replace
        assert!(result.is_err(), "Empty oldString should be rejected");
        assert!(result.unwrap_err().contains("cannot be empty"));
    }

    // BUG #1: replace_all with fuzzy matchers may replace wrong occurrences
    #[test]
    fn test_replace_all_with_fuzzy_match() {
        let content = "  foo  \nbar\n  foo  \nbaz";
        let find = "foo"; // Will match via whitespace_normalized or trimmed
        let result = EditTool::replace(content, find, "qux", true);
        // This might replace ALL instances of "foo" in content, even trimmed ones
        // which may not be the intended behavior
        if let Ok(new_content) = result {
            println!("Replaced content: {}", new_content);
            // The fuzzy match might have found "  foo  " but then replaced just "foo"
            // or it might replace all variations
        }
    }

    // BUG #1 continued: Multiple occurrences should fail when not using replace_all
    #[test]
    fn test_multiple_occurrences_without_replace_all() {
        let content = "hello world\nhello rust\nhello test";
        let find = "hello";
        let result = EditTool::replace(content, find, "hi", false);
        // Should fail with "multiple occurrences" error
        assert!(
            result.is_err(),
            "Should fail when multiple occurrences found without replaceAll"
        );
        if let Err(e) = result {
            assert!(
                e.contains("multiple times"),
                "Error should mention multiple occurrences"
            );
        }
    }

    #[test]
    fn test_line_trimmed_empty_content_no_panic() {
        let content = "";
        let find = "hello";
        let matches = EditTool::line_trimmed_replacer(content, find);
        assert!(matches.is_empty(), "Expected no matches for empty content");
    }

    #[test]
    fn test_replace_empty_content_returns_not_found() {
        let result = EditTool::replace("", "hello", "world", false);
        assert!(
            result.is_err(),
            "Expected not found error for empty content"
        );
        assert!(result.unwrap_err().contains("not found"));
    }

    // Test edge case: oldString same as newString
    #[test]
    fn test_same_old_and_new_string() {
        let content = "hello world";
        let result = EditTool::replace(content, "hello", "hello", false);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("oldString and newString must be different")
        );
    }

    // Test edge case: oldString not found at all
    #[test]
    fn test_string_not_found() {
        let content = "hello world";
        let result = EditTool::replace(content, "nonexistent", "replacement", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    // Test line_trimmed with complex indentation
    #[test]
    fn test_line_trimmed_preserves_original_spacing() {
        let content = "    hello world    \n    rust is great    ";
        let find = "hello world\nrust is great";
        let matches = EditTool::line_trimmed_replacer(content, find);
        assert_eq!(matches.len(), 1);
        // The match should preserve the original spacing from content
        assert!(matches[0].starts_with("    "));
        assert!(matches[0].contains("    rust is great"));
    }

    // Test context_aware with partial matches
    #[test]
    fn test_context_aware_partial_match() {
        let content = "fn foo() {\n    line1();\n    line2();\n    line3();\n}";
        let find = "fn foo() {\n    line1();\n    different();\n    line3();\n}";
        let matches = EditTool::context_aware_replacer(content, find);
        // Should match because 2 out of 3 middle lines match (>= 50%)
        assert_eq!(
            matches.len(),
            1,
            "Should find one match via context awareness"
        );
    }

    // Test indentation_flexible
    #[test]
    fn test_indentation_flexible() {
        let content = "    fn foo() {\n        bar();\n    }";
        let find = "fn foo() {\n    bar();\n}";
        let matches = EditTool::indentation_flexible_replacer(content, find);
        assert_eq!(
            matches.len(),
            1,
            "Should match despite different indentation"
        );
    }

    // Integration test: replace with block_anchor should work
    #[tokio::test]
    async fn test_replace_with_block_anchor() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        let content = "fn foo() {\n    println!(\"old\");\n    bar();\n}";
        fs::write(&file_path, content).unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = EditTool::new();

        let args = json!({
            "filePath": file_path.display().to_string(),
            "oldString": "fn foo() {\n    println!(\"old\");\n    bar();\n}",
            "newString": "fn foo() {\n    println!(\"new\");\n    bar();\n}"
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_ok(), "Replace should succeed");

        let new_content = fs::read_to_string(&file_path).unwrap();
        assert!(new_content.contains("println!(\"new\")"));
    }
}
