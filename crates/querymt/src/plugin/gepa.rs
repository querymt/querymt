use std::process::{Command, Stdio};
use std::io::Write;
use tempfile::NamedTempFile;
use serde_json::Value;

pub struct GepaPlugin;

impl GepaPlugin {
    pub fn new() -> Self {
        GepaPlugin
    }

    pub fn optimize_prompts(&self, prompts: &[String]) -> Result<Vec<String>, std::io::Error> {
        // Create a temporary file for the prompts.
        let mut prompts_file = NamedTempFile::new()?;
        for prompt in prompts {
            writeln!(prompts_file, "{}", prompt)?;
        }

        // Create a temporary file for the configuration.
        let mut config_file = NamedTempFile::new()?;
        let config = format!(
            "prompts_file: {}\noutput_file: Dpareto.json\nfeedback_file: Dfeedback.json\npopulation_size: 10\ngenerations: 10\nmutation_rate: 0.1",
            prompts_file.path().to_str().unwrap()
        );
        write!(config_file, "{}", config)?;

        // Run the GEPA.py script.
        let mut child = Command::new("python")
            .arg("GEPA.py")
            .arg("--config")
            .arg(config_file.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let output = child.wait_with_output()?;

        if !output.status.success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "GEPA.py failed with status: {}\\nstdout: {}\\nstderr: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
            ));
        }

        // Parse the output file.
        let pareto_file = std::fs::read_to_string("Dpareto.json")?;
        let pareto_json: Value = serde_json::from_str(&pareto_file)?;
        let mut optimized_prompts = Vec::new();

        if let Some(prompts_array) = pareto_json.as_array() {
            for prompt_obj in prompts_array {
                if let Some(prompt) = prompt_obj.get("prompt").and_then(|p| p.as_str()) {
                    optimized_prompts.push(prompt.to_string());
                }
            }
        }

        Ok(optimized_prompts)
    }
}
