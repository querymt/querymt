/// Validates unified diff patches before applying them using patchkit
use super::patch_utils;

pub struct PatchValidator;

impl PatchValidator {
    /// Validate that a patch can be applied to actual file content
    /// Returns Ok(()) if valid, Err with detailed explanation if invalid
    ///
    /// This performs a lightweight validation:
    /// 1. Checks if the patch is parseable
    /// 2. Checks if target files exist
    ///
    /// Note: Content matching is validated during actual application
    pub fn validate(patch_text: &str, workdir: Option<&str>) -> Result<(), String> {
        // Parse the patches (this validates syntax)
        let patches = patch_utils::parse_patches(patch_text)?;

        // Check that all target files exist
        for parsed in &patches {
            // Note: We don't know the strip level here, so we validate with strip=0
            // The actual strip will be applied during apply_patch
            let _ = patch_utils::resolve_file_path(&parsed.file_path, workdir, 0)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_file(dir: &TempDir, filename: &str, content: &str) -> std::path::PathBuf {
        let file_path = dir.path().join(filename);
        let mut file = fs::File::create(&file_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file_path
    }

    #[test]
    fn test_validate_simple_patch_success() {
        let temp_dir = TempDir::new().unwrap();
        let file_content = "line 1\nline 2\nline 3\n";
        create_test_file(&temp_dir, "test.txt", file_content);

        let patch = r#"--- test.txt
+++ test.txt
@@ -1,3 +1,3 @@
 line 1
-line 2
+line 2 modified
 line 3
"#;

        let result = PatchValidator::validate(patch, Some(temp_dir.path().to_str().unwrap()));
        assert!(
            result.is_ok(),
            "Expected validation to succeed, got: {:?}",
            result
        );
    }

    #[test]
    fn test_validate_file_not_found() {
        let temp_dir = TempDir::new().unwrap();

        let patch = r#"--- nonexistent.txt
+++ nonexistent.txt
@@ -1,1 +1,1 @@
-old line
+new line
"#;

        let result = PatchValidator::validate(patch, Some(temp_dir.path().to_str().unwrap()));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn test_validate_multiple_files() {
        let temp_dir = TempDir::new().unwrap();
        create_test_file(&temp_dir, "file1.txt", "content 1\n");
        create_test_file(&temp_dir, "file2.txt", "content 2\n");

        let patch = r#"--- file1.txt
+++ file1.txt
@@ -1,1 +1,1 @@
-content 1
+content 1 modified
--- file2.txt
+++ file2.txt
@@ -1,1 +1,1 @@
-content 2
+content 2 modified
"#;

        let result = PatchValidator::validate(patch, Some(temp_dir.path().to_str().unwrap()));
        assert!(
            result.is_ok(),
            "Expected validation to succeed, got: {:?}",
            result
        );
    }

    #[test]
    fn test_validate_malformed_patch() {
        let temp_dir = TempDir::new().unwrap();
        create_test_file(&temp_dir, "test.txt", "content\n");

        let patch = r#"this is not a valid patch
just random text
"#;

        let result = PatchValidator::validate(patch, Some(temp_dir.path().to_str().unwrap()));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("No valid patches") || err.contains("syntax"));
    }

    #[test]
    fn test_validate_without_workdir() {
        // Create a file in the current directory
        let file_path = std::path::Path::new("test_temp_file_validator.txt");
        fs::write(file_path, "test content\n").unwrap();

        let patch = r#"--- test_temp_file_validator.txt
+++ test_temp_file_validator.txt
@@ -1,1 +1,1 @@
-test content
+modified content
"#;

        let result = PatchValidator::validate(patch, None);

        // Clean up
        fs::remove_file(file_path).ok();

        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_with_a_b_prefix() {
        let temp_dir = TempDir::new().unwrap();
        create_test_file(&temp_dir, "test.txt", "content\n");

        // Note: resolve_file_path now auto-strips prefixes if the stripped path exists
        let patch = r#"--- a/test.txt
+++ b/test.txt
@@ -1,1 +1,1 @@
-content
+modified
"#;

        let result = PatchValidator::validate(patch, Some(temp_dir.path().to_str().unwrap()));
        assert!(
            result.is_ok(),
            "Expected validation to succeed with a/b prefix due to auto-detection, got: {:?}",
            result
        );
    }
}
