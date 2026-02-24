use querymt::chat::{Tool, ToolChoice};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct MistralRSConfig {
    pub model: String,
    pub model_kind: Option<MistralRSModelKind>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
    pub tok_model_id: Option<String>,
    pub hf_revision: Option<String>,
    pub token_source: Option<String>,
    pub chat_template: Option<String>,
    pub tokenizer_json: Option<String>,
    pub jinja_explicit: Option<String>,
    pub hf_cache_path: Option<String>,
    pub loader_type: Option<String>,
    pub dtype: Option<String>,
    pub topology: Option<String>,
    pub isq: Option<String>,
    pub imatrix: Option<String>,
    pub calibration_file: Option<String>,
    pub max_edge: Option<u32>,
    pub force_cpu: Option<bool>,
    pub device_map: Option<MistralRSDeviceMap>,
    pub max_num_seqs: Option<usize>,
    pub no_kv_cache: Option<bool>,
    pub prefix_cache_n: Option<usize>,
    pub throughput_logging: Option<bool>,
    pub paged_attn: Option<bool>,
    pub paged_attn_block_size: Option<usize>,
    pub paged_attn_gpu_mem: Option<usize>,
    pub paged_attn_gpu_mem_usage: Option<f32>,
    pub paged_attn_context_len: Option<usize>,
    pub paged_attn_cache_type: Option<MistralRSPagedCacheType>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum MistralRSModelKind {
    #[default]
    Text,
    Vision,
    Embedding,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MistralRSDeviceMap {
    Auto,
    Single,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
pub enum MistralRSPagedCacheType {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "f8e4m3")]
    F8E4M3,
}
