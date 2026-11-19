//! Regenerate `tests/traces/*.json` (inputs plus expected output).
//!
//! `cargo run -p querymt-agent-session-core --example regen-traces --features test-support`

use querymt_agent_session_core::testing::{evaluate, golden_cases};
use serde_json::json;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/traces");
    std::fs::create_dir_all(&dir)?;
    for case in golden_cases() {
        let file = json!({
            "name": case.name,
            "batches": case.batches,
            "expected": evaluate(&case),
        });
        let path = dir.join(format!("{}.json", case.name));
        std::fs::write(&path, serde_json::to_string_pretty(&file)? + "\n")?;
        println!("wrote {}", path.display());
    }
    Ok(())
}
