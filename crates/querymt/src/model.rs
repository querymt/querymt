use std::{fs::File, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::error::LLMError;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelYaml {
    pub model: String,
    pub base: Vec<Base>,
    #[serde(rename = "metadataOverrides")]
    pub metadata_overrides: MetadataOverrides,
    pub config: Config,
    #[serde(rename = "customFields")]
    pub custom_fields: Vec<CustomField>,
    pub suggestions: Vec<Suggestion>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Base {
    pub key: String,
    pub sources: Vec<Source>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Source {
    #[serde(rename = "type")]
    pub source_type: String,
    pub user: Option<String>,
    pub repo: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MetadataOverrides {
    pub domain: String,
    pub architectures: Vec<String>,
    #[serde(rename = "compatibilityTypes")]
    pub compatibility_types: Vec<String>,
    #[serde(rename = "paramsStrings")]
    pub params_strings: Vec<String>,
    #[serde(rename = "minMemoryUsageBytes")]
    pub min_memory_usage_bytes: u64,
    #[serde(rename = "contextLengths")]
    pub context_lengths: Vec<u64>,
    #[serde(rename = "trainedForToolUse")]
    pub trained_for_tool_use: String,
    pub vision: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub operation: Operation,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Operation {
    pub fields: Vec<Field>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Field {
    pub key: String,
    #[serde(flatten)]
    pub value: FieldValue,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldValue {
    Simple(JsonValue),
    Checked { checked: bool, value: JsonValue },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomField {
    pub key: String,
    pub display_name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub field_type: String,
    pub default_value: JsonValue,
    pub effects: Vec<Effect>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Effect {
    #[serde(rename = "type")]
    pub effect_type: String,
    pub variable: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    pub message: String,
    pub conditions: Vec<Condition>,
    pub fields: Vec<Field>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Condition {
    #[serde(rename = "type")]
    pub condition_type: String,
    pub key: String,
    pub value: JsonValue,
}

impl ModelYaml {
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<ModelYaml, LLMError> {
        let file = File::open(path)?;

        let model: ModelYaml = serde_yml::from_reader(file)?;
        Ok(model)
    }

    pub fn from_str(yaml: &str) -> Result<ModelYaml, LLMError> {
        let model: ModelYaml = serde_yml::from_str(yaml)?;
        Ok(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_YAML: &str = r#"
model: qwen/qwen3-8b

base:
  - key: lmstudio-community/qwen3-8b-gguf
    sources:
      - type: huggingface
        user: lmstudio-community
        repo: Qwen-3-8B-GGUF

metadataOverrides:
  domain: llm
  architectures:
    - llama
  compatibilityTypes:
    - gguf
    - safetensors
  paramsStrings:
    - 1B
  minMemoryUsageBytes: 1000000000
  contextLengths:
    - 131072
  trainedForToolUse: mixed
  vision: false

config:
  operation:
    fields:
      - key: llm.prediction.topKSampling
        value: 20
      - key: llm.prediction.minPSampling
        value:
          checked: true
          value: 0

customFields:
  - key: enableThinking
    displayName: Enable Thinking
    description: Enable the model to think before answering.
    type: boolean
    defaultValue: true
    effects:
      - type: setJinjaVariable
        variable: enable_thinking

suggestions:
  - message: The following parameters are recommended for thinking mode
    conditions:
      - type: equals
        key: $.enableThinking
        value: true
    fields:
      - key: llm.prediction.temperature
        value: 0.6
"#;

    #[test]
    fn test_parse_example() {
        let spec = ModelYaml::from_str(EXAMPLE_YAML).expect("Failed to parse YAML");
        println!("Parsed spec: {:#?}", spec);
    }
}
