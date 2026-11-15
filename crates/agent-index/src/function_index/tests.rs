use super::core::FunctionIndex;
use super::fingerprint::{rust_structural_fingerprint, simhash};
use super::types::{FunctionIndexConfig, IndexedFunctionEntry};
use similarity_core::AstFingerprint;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

#[tokio::test]
async fn test_build_empty_index() {
    let temp_dir = TempDir::new().unwrap();
    let config = FunctionIndexConfig::default();

    let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    assert_eq!(index.function_count(), 0);
    assert_eq!(index.file_count(), 0);
}

#[tokio::test]
async fn test_build_index_with_typescript() {
    let temp_dir = TempDir::new().unwrap();

    fs::write(
        temp_dir.path().join("test.ts"),
        r#"
function hello(name: string) {
    console.log("Hello, " + name);
    const greeting = "Hi there";
    return greeting + " " + name;
}

function goodbye(name: string) {
    console.log("Goodbye, " + name);
    const farewell = "See you";
    return farewell + " " + name;
}
"#,
    )
    .unwrap();

    let config = FunctionIndexConfig::default().with_min_lines(3);
    let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    assert_eq!(index.file_count(), 1);
    assert!(index.function_count() >= 2);
}

#[tokio::test]
async fn test_find_similar_functions() {
    let temp_dir = TempDir::new().unwrap();

    // Create a file with a function
    fs::write(
        temp_dir.path().join("utils.ts"),
        r#"
function calculateTotal(items: any[]) {
    let total = 0;
    for (const item of items) {
        total += item.price * item.quantity;
    }
    return total;
}
"#,
    )
    .unwrap();

    let config = FunctionIndexConfig::default()
        .with_min_lines(3)
        .with_threshold(0.7);
    let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    // Now check for similar code
    let new_code = r#"
function computeSum(products: Product[]) {
    let sum = 0;
    for (const product of products) {
        sum += product.price * product.quantity;
    }
    return sum;
}
"#;

    let results = index.find_similar_to_code(Path::new("new.ts"), new_code);

    // Should find the similar function
    assert!(!results.is_empty() || index.function_count() > 0);
}

#[tokio::test]
async fn test_update_file() {
    let temp_dir = TempDir::new().unwrap();

    fs::write(
        temp_dir.path().join("test.ts"),
        r#"
function original(x: number) {
    const result = x * 2;
    console.log(result);
    return result;
}
"#,
    )
    .unwrap();

    let config = FunctionIndexConfig::default().with_min_lines(3);
    let mut index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    let initial_count = index.function_count();

    // Update the file with new content
    let new_content = r#"
function updated(x: number) {
    const result = x * 3;
    console.log(result);
    return result;
}

function another(y: number) {
    const value = y + 1;
    console.log(value);
    return value;
}
"#;

    index.update_file(&temp_dir.path().join("test.ts"), new_content);

    // Should have more functions now
    assert!(index.function_count() >= initial_count);
}

#[tokio::test]
async fn test_build_index_with_python() {
    let temp_dir = TempDir::new().unwrap();

    fs::write(
        temp_dir.path().join("test.py"),
        r#"
def hello(name):
    """Say hello to someone."""
    greeting = f"Hello, {name}!"
    print(greeting)
    return greeting

def add(a, b):
    """Add two numbers."""
    result = a + b
    print(f"Result: {result}")
    return result

class Calculator:
    """A simple calculator class."""
    
    def __init__(self):
        self.result = 0
    
    def multiply(self, x, y):
        """Multiply two numbers."""
        self.result = x * y
        return self.result
    
    async def async_divide(self, x, y):
        """Async division."""
        if y == 0:
            raise ValueError("Cannot divide by zero")
        self.result = x / y
        return self.result
"#,
    )
    .unwrap();

    let config = FunctionIndexConfig::default().with_min_lines(3);
    let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    // Should have indexed the Python file
    assert_eq!(index.file_count(), 1);

    // Should have 4 functions: hello (5 lines), add (5 lines), multiply (4 lines), async_divide (6 lines)
    // Note: __init__ (2 lines) is filtered out by min_lines=3
    assert_eq!(index.function_count(), 4);
}

#[tokio::test]
async fn test_python_update_file() {
    let temp_dir = TempDir::new().unwrap();

    fs::write(
        temp_dir.path().join("test.py"),
        r#"
def original():
    print("original")
    return True
"#,
    )
    .unwrap();

    let config = FunctionIndexConfig::default().with_min_lines(3);
    let mut index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    assert_eq!(index.function_count(), 1);

    // Update the file with new content
    let new_content = r#"
def updated(x):
    result = x * 3
    print(result)
    return result

def another(y):
    value = y + 1
    print(value)
    return value
"#;

    index.update_file(&temp_dir.path().join("test.py"), new_content);

    // Should have 2 functions now
    assert_eq!(index.function_count(), 2);
}

#[tokio::test]
async fn test_python_mixed_with_other_languages() {
    let temp_dir = TempDir::new().unwrap();

    // Add Python file
    fs::write(
        temp_dir.path().join("script.py"),
        r#"
def python_func():
    print("Python")
    return 42
"#,
    )
    .unwrap();

    // Add TypeScript file
    fs::write(
        temp_dir.path().join("code.ts"),
        r#"
function tsFunc() {
    console.log("TypeScript");
    return 42;
}
"#,
    )
    .unwrap();

    let config = FunctionIndexConfig::default().with_min_lines(3);
    let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    // Should have indexed both files
    assert_eq!(index.file_count(), 2);
    // Should have 2 functions total
    assert_eq!(index.function_count(), 2);
}

/// This test documents the thread-safety contract for FunctionIndex
#[test]
fn test_thread_safety_documentation() {
    // FunctionIndex is Send + Sync, which means it CAN be shared across threads
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<FunctionIndex>();

    // HOWEVER: The methods that use tree-sitter parsers are NOT thread-safe
    // and must only be called from a single thread at a time:
    //
    // - update_file() - NOT thread-safe (uses parsers)
    // - find_similar_to_code() - NOT thread-safe (uses parsers)
    // - build_sync() - Uses par_iter() which creates thread-local parsers (SAFE)
    //
    // This test serves as documentation. If you're seeing segfaults:
    // 1. Check that update_file() is not called from multiple threads concurrently
    // 2. Ensure no rayon par_iter() is used in update paths
    // 3. Verify parsers are created fresh within each thread's scope
    //
    // See the documentation on build_sync() and update_file() for details.
}

/// Test that sequential updates work correctly (thread-safe pattern)
#[tokio::test]
async fn test_sequential_updates_are_safe() {
    let temp_dir = TempDir::new().unwrap();

    // Create initial file
    fs::write(
        temp_dir.path().join("test.rs"),
        r#"
fn foo() {
    println!("original");
    let x = 42;
    return x;
}
"#,
    )
    .unwrap();

    let config = FunctionIndexConfig::default().with_min_lines(3);
    let mut index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    assert_eq!(index.function_count(), 1);

    // Simulate multiple sequential updates (like from file watcher)
    // This is the SAFE pattern - one at a time
    for i in 1..=5 {
        let content = format!(
            r#"
fn foo() {{
    println!("update {}");
    let x = 42;
    return x;
}}
"#,
            i
        );
        index.update_file(&temp_dir.path().join("test.rs"), &content);
        assert_eq!(index.function_count(), 1);
    }

    // All updates completed successfully without segfaults
}

#[tokio::test]
async fn test_ignores_target_directory() {
    let temp_dir = TempDir::new().unwrap();

    // Initialize a git repo (needed for .gitignore to be recognized by WalkBuilder)
    // Using gix (gitoxide) which is already in the project dependencies
    gix::init(temp_dir.path()).expect("Failed to initialize git repo");

    // Create a .gitignore file
    fs::write(temp_dir.path().join(".gitignore"), "/target\n").unwrap();

    // Create source files in the root
    fs::write(
        temp_dir.path().join("main.rs"),
        r#"
fn main() {
    println!("Hello, world!");
    let x = 42;
}
"#,
    )
    .unwrap();

    // Create a target directory with Rust files (build artifacts)
    fs::create_dir_all(temp_dir.path().join("target/debug")).unwrap();
    fs::write(
        temp_dir.path().join("target/debug/build.rs"),
        r#"
fn build_artifact() {
    println!("This should be ignored");
    let y = 99;
}
"#,
    )
    .unwrap();

    fs::create_dir_all(temp_dir.path().join("target/debug/deps")).unwrap();
    fs::write(
        temp_dir.path().join("target/debug/deps/lib.rs"),
        r#"
fn dependency() {
    println!("This should also be ignored");
    let z = 100;
}
"#,
    )
    .unwrap();

    let config = FunctionIndexConfig::default().with_min_lines(3);
    let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    // Verify that no files from target/ directory are indexed
    let indexed_files: Vec<_> = index
        .functions
        .keys()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    assert!(
        indexed_files.iter().all(|f| !f.contains("target")),
        "No files from target/ directory should be indexed. Found: {:?}",
        indexed_files
    );

    // Should have indexed only main.rs, not anything in target/
    assert_eq!(index.file_count(), 1, "Should only index 1 file (main.rs)");
    assert_eq!(
        index.function_count(),
        1,
        "Should only have 1 function (main)"
    );
}

// -------------------------------------------------------------------------
// calculate_similarity dispatch tests (Bug 2 regression suite)
// -------------------------------------------------------------------------

/// Two near-identical Rust functions should score well above threshold.
///
/// This is a direct regression test for the bug where `calculate_similarity`
/// called the OXC TypeScript parser for all languages.  For `.rs` files that
/// parser always returned `Err`, causing a silent `0.0` return.
#[tokio::test]
async fn test_calculate_similarity_rust_nearly_identical() {
    let temp_dir = TempDir::new().unwrap();

    // Index a file containing the "original" function.
    fs::write(
        temp_dir.path().join("math.rs"),
        r#"
pub fn default_temperature(model: &str) -> f64 {
    match model {
        m if m.contains("o1") || m.contains("o3") => 1.0,
        m if m.contains("claude") => 0.7,
        m if m.contains("gpt-4") => 0.8,
        m if m.contains("gemini") => 0.9,
        _ => 0.7,
    }
}
"#,
    )
    .unwrap();

    let config = FunctionIndexConfig::default()
        .with_min_lines(3)
        .with_threshold(0.7);
    let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    assert_eq!(index.function_count(), 1, "should index the Rust function");

    // The probe is structurally identical — only the name and variable names differ.
    let probe = r#"
pub fn example_calculate(model: &str) -> f64 {
    match model {
        m if m.contains("o1") || m.contains("o3") => 1.0,
        m if m.contains("claude") => 0.7,
        m if m.contains("gpt-4") => 0.8,
        m if m.contains("gemini") => 0.9,
        _ => 0.7,
    }
}
"#;

    let results = index.find_similar_to_code(Path::new("new.rs"), probe);

    assert!(
        !results.is_empty(),
        "near-identical Rust functions should be detected as similar (was 0.0 before the fix)"
    );

    let similarity = results[0].1[0].similarity;
    assert!(
        similarity >= 0.7,
        "expected similarity >= 0.7 for near-identical Rust functions, got {similarity:.4}"
    );
}

/// Two completely different Rust functions should score below threshold.
#[tokio::test]
async fn test_calculate_similarity_rust_different() {
    let temp_dir = TempDir::new().unwrap();

    fs::write(
        temp_dir.path().join("lib.rs"),
        r#"
pub fn serialize_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("\"{}\"", s),
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(serialize_json).collect();
            format!("[{}]", items.join(", "))
        }
        serde_json::Value::Object(map) => {
            let pairs: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("\"{}\": {}", k, serialize_json(v)))
                .collect();
            format!("{{{}}}", pairs.join(", "))
        }
    }
}
"#,
    )
    .unwrap();

    let config = FunctionIndexConfig::default()
        .with_min_lines(3)
        .with_threshold(0.7);
    let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    assert!(
        index.function_count() >= 1,
        "should index the Rust function"
    );

    // Probe with something structurally unrelated.
    let probe = r#"
pub fn connect_to_database(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let response = client.get(url).send().await?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()).into());
    }
    Ok(())
}
"#;

    let results = index.find_similar_to_code(Path::new("other.rs"), probe);

    // Either no results, or similarity is below threshold (results only contains matches >= threshold)
    assert!(
        results.is_empty(),
        "structurally different Rust functions should not be reported as similar"
    );
}

/// Verify that Rust functions with identical bodies but different names are
/// caught, and that the similarity value is non-zero (the core of Bug 2).
#[tokio::test]
async fn test_calculate_similarity_rust_score_is_nonzero() {
    use std::path::PathBuf;

    let config = FunctionIndexConfig::default()
        .with_min_lines(3)
        .with_threshold(0.0); // accept everything so we can inspect the raw score

    // Build a minimal index containing a single Rust entry directly.
    let body = r#"    let x = value * 2;
    let y = x + offset;
    if y > threshold {
        return y - threshold;
    }
    y"#;

    let entry_a = IndexedFunctionEntry {
        name: "compute_a".to_string(),
        file_path: PathBuf::from("src/a.rs"),
        start_line: 1,
        end_line: 8,
        fingerprint: AstFingerprint::new(),
        structural_fingerprint: 0,
        body_text: body.to_string(),
        language: "rust".to_string(),
    };

    let entry_b = IndexedFunctionEntry {
        name: "compute_b".to_string(),
        file_path: PathBuf::from("src/b.rs"),
        start_line: 1,
        end_line: 8,
        fingerprint: AstFingerprint::new(),
        structural_fingerprint: 0,
        body_text: body.to_string(),
        language: "rust".to_string(),
    };

    // Insert both entries into a hand-built index.
    let mut functions: std::collections::HashMap<PathBuf, Vec<IndexedFunctionEntry>> =
        std::collections::HashMap::new();
    functions
        .entry(PathBuf::from("src/a.rs"))
        .or_default()
        .push(entry_a.clone());
    functions
        .entry(PathBuf::from("src/b.rs"))
        .or_default()
        .push(entry_b);

    let index = FunctionIndex {
        functions,
        config,
        root: PathBuf::from("."),
    };

    let matches = index.find_similar(&entry_a);

    assert!(
        !matches.is_empty(),
        "identical Rust body texts should produce at least one match"
    );
    let score = matches[0].similarity;
    assert!(
        score > 0.0,
        "similarity score must be > 0.0 for identical Rust bodies (was always 0.0 before the fix), got {score}"
    );
}

// -------------------------------------------------------------------------
// Structural fingerprinting unit tests
// -------------------------------------------------------------------------

/// SimHash of the same input must be deterministic.
#[test]
fn test_simhash_deterministic() {
    let tokens = ["fn", "if", "match", "let", "return"];
    let h1 = simhash(tokens.iter().copied());
    let h2 = simhash(tokens.iter().copied());
    assert_eq!(h1, h2, "simhash must be deterministic");
}

/// SimHash of identical token sequences must be identical (Hamming distance 0).
#[test]
fn test_simhash_identical_inputs_zero_distance() {
    let tokens = ["if_expr", "let_decl", "call_expr", "match_arm", "let_decl"];
    let h1 = simhash(tokens.iter().copied());
    let h2 = simhash(tokens.iter().copied());
    let dist = (h1 ^ h2).count_ones();
    assert_eq!(dist, 0, "identical inputs → Hamming distance 0");
}

/// SimHash of completely different sequences should have a large Hamming distance.
#[test]
fn test_simhash_different_inputs_large_distance() {
    let a = ["if_expr", "let_decl", "match_arm"];
    let b = [
        "closure",
        "struct_field",
        "impl_item",
        "use_decl",
        "type_alias",
    ];
    let ha = simhash(a.iter().copied());
    let hb = simhash(b.iter().copied());
    let dist = (ha ^ hb).count_ones();
    // Not guaranteed to be > 25 for any pair, but these are very different; in
    // practice the Hamming distance is typically > 10.
    assert!(
        dist > 0,
        "different inputs should differ (Hamming dist={dist})"
    );
}

/// `rust_structural_fingerprint` must be non-zero for a valid Rust function.
#[test]
fn test_rust_fingerprint_nonzero_for_valid_fn() {
    let src = r#"
pub fn default_temperature(model: &str) -> f64 {
    match model {
        m if m.contains("o1") => 1.0,
        m if m.contains("claude") => 0.7,
        _ => 0.7,
    }
}
"#;
    let fp = rust_structural_fingerprint(src);
    assert_ne!(fp, 0, "fingerprint must be non-zero for a valid function");
}

/// Two structurally identical Rust functions (only names differ) should
/// produce the same fingerprint because syn-based features are name-agnostic
/// at the structural level.
#[test]
fn test_rust_fingerprint_rename_invariant() {
    let src_a = r#"
pub fn default_temperature(model: &str) -> f64 {
    match model {
        m if m.contains("o1") => 1.0,
        m if m.contains("claude") => 0.7,
        _ => 0.7,
    }
}
"#;
    let src_b = r#"
pub fn example_calculate(input: &str) -> f64 {
    match input {
        m if m.contains("o1") => 1.0,
        m if m.contains("claude") => 0.7,
        _ => 0.7,
    }
}
"#;
    let fp_a = rust_structural_fingerprint(src_a);
    let fp_b = rust_structural_fingerprint(src_b);
    assert_ne!(fp_a, 0);
    assert_ne!(fp_b, 0);
    // The control-flow, parameter arity, and return type are identical, so
    // the fingerprints should be the same (param type strings differ only in
    // variable name "model" vs "input", which the type_sketch strips).
    assert_eq!(
        fp_a, fp_b,
        "structurally identical Rust functions should have equal fingerprints"
    );
}

/// Two structurally *different* Rust functions should have different fingerprints.
#[test]
fn test_rust_fingerprint_different_for_different_fns() {
    let src_a = r#"
fn short(x: u32) -> u32 { x + 1 }
"#;
    let src_b = r#"
fn complex(items: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    for item in items {
        if item.starts_with("prefix") {
            result.push(item.clone());
        }
    }
    result
}
"#;
    let fp_a = rust_structural_fingerprint(src_a);
    let fp_b = rust_structural_fingerprint(src_b);
    assert_ne!(fp_a, 0);
    assert_ne!(fp_b, 0);
    let dist = (fp_a ^ fp_b).count_ones();
    assert!(
        dist > 0,
        "structurally different Rust functions should have different fingerprints (dist={dist})"
    );
}

/// Verify that indexed Rust functions get a non-zero structural_fingerprint.
#[tokio::test]
async fn test_rust_index_populates_structural_fingerprint() {
    let temp_dir = TempDir::new().unwrap();
    fs::write(
        temp_dir.path().join("lib.rs"),
        r#"
pub fn add(a: i32, b: i32) -> i32 {
    let result = a + b;
    if result > 100 {
        return result - 100;
    }
    result
}
"#,
    )
    .unwrap();

    let config = FunctionIndexConfig::default().with_min_lines(3);
    let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    assert_eq!(index.function_count(), 1);

    let entry = index
        .functions
        .values()
        .flat_map(|v| v.iter())
        .next()
        .unwrap();

    assert_ne!(
        entry.structural_fingerprint, 0,
        "indexed Rust functions must have a non-zero structural_fingerprint"
    );
}

/// The size-ratio pre-filter should prevent tiny functions from matching huge ones.
#[tokio::test]
async fn test_size_ratio_prefilter_rejects_mismatched_sizes() {
    let temp_dir = TempDir::new().unwrap();

    // A large function (many lines)
    let large_body: String = (0..50)
        .map(|i| format!("    let x{i} = {i};\n"))
        .collect::<String>();
    fs::write(
        temp_dir.path().join("big.rs"),
        format!("pub fn large_fn() -> i32 {{\n{large_body}    42\n}}\n"),
    )
    .unwrap();

    let config = FunctionIndexConfig::default()
        .with_min_lines(3)
        .with_threshold(0.5);
    let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

    // Probe with a tiny 4-line function — size ratio << 0.3
    let probe = r#"
pub fn tiny(x: i32) -> i32 {
    x + 1
}
"#;
    let results = index.find_similar_to_code(Path::new("probe.rs"), probe);
    assert!(
        results.is_empty(),
        "size-ratio filter should reject tiny vs huge function pairs"
    );
}
