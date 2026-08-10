use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub reasoning: bool,
    pub context_window: u32,
    pub max_tokens: u32,
    #[serde(default)]
    pub thinking_levels: Vec<String>,
    #[serde(default)]
    pub vision: bool,
    /// Modalities accepted as model input, e.g. ["text", "image", "pdf"].
    #[serde(default)]
    pub input: Vec<String>,
    /// Modalities emitted by the model, normally ["text"].
    #[serde(default)]
    pub output: Vec<String>,
    /// Whether the model advertises tool/function calling support.
    #[serde(default)]
    pub tool_call: bool,
    /// Whether the model advertises structured output support.
    #[serde(default)]
    pub structured_output: bool,
    #[serde(default)]
    pub provider: String,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct ClientInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}
