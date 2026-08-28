use std::env;
use std::path::PathBuf;
use std::sync::mpsc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::llm::config::*;

#[derive(Clone, Debug)]
pub(crate) struct Skill {
    pub(crate)     name: String,
    pub(crate)     description: String,
    pub(crate)     path: PathBuf,
}

/// A single streamed line destined for the UI transcript. Plain text (no
/// ANSI) so the UI applies its own styling. Ratatui-agnostic.
#[derive(Debug, Clone)]
pub enum SinkLine {
    Assistant(String),
    ToolInput(String),
    ToolOutput { name: String, summary: String },
    System(String),
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    Once,
    Session,
    Deny,
}

pub(crate) struct ApprovalRequest {
    pub name: String,
    pub input: String,
    pub response: mpsc::Sender<ApprovalDecision>,
}

/// Width of a string as displayed, ignoring ANSI escape sequences.
/// dividers, truncated to fit the terminal width.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct ChatMessage {
    pub(crate)     role: String,
    pub(crate)     content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate)     tool_calls: Option<Vec<LlmToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate)     tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate)     name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct LlmToolCall {
    pub(crate)     id: String,
    #[serde(rename = "type")]
    pub(crate)     call_type: String,
    pub(crate)     function: FunctionCall,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct FunctionCall {
    pub(crate)     name: String,
    pub(crate)     arguments: String,
}

#[derive(Serialize)]
pub(crate) struct ChatRequest {
    pub(crate)     model: String,
    pub(crate)     messages: Vec<ChatMessage>,
    pub(crate)     tools: Vec<ToolDefinition>,
    pub(crate)     stream: bool,
    pub(crate)     stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate)     reasoning_effort: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiProtocol {
    ChatCompletions,
    Responses,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    OpenCode,
    OpenAiCodex,
}

impl Provider {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "opencode" => Ok(Self::OpenCode),
            "openai-codex" | "codex" => Ok(Self::OpenAiCodex),
            other => Err(format!(
                "unsupported provider '{}'; use opencode or openai-codex",
                other
            )),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::OpenCode => "opencode",
            Self::OpenAiCodex => "openai-codex",
        }
    }
}

#[derive(Serialize)]
pub(crate) struct StreamOptions {
    pub(crate)     include_usage: bool,
}

#[derive(Deserialize, Default)]
pub(crate) struct Usage {
    pub(crate)     prompt_tokens: u64,
}

#[derive(Serialize)]
pub(crate) struct ToolDefinition {
    #[serde(rename = "type")]
    pub(crate)     tool_type: String,
    pub(crate)     function: FunctionDef,
}

#[derive(Serialize)]
pub(crate) struct FunctionDef {
    pub(crate)     name: String,
    pub(crate)     description: String,
    pub(crate)     parameters: Value,
}

#[derive(Deserialize)]
pub(crate) struct StreamChunk {
    pub(crate)     choices: Vec<StreamChoice>,
    #[serde(default)]
    pub(crate)     usage: Option<Usage>,
}

#[derive(Deserialize)]
pub(crate) struct StreamChoice {
    pub(crate)     delta: StreamDelta,
}

#[derive(Deserialize)]
pub(crate) struct StreamDelta {
    pub(crate)     content: Option<String>,
    pub(crate)     tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Deserialize)]
pub(crate) struct StreamToolCall {
    pub(crate)     index: usize,
    pub(crate)     id: Option<String>,
    pub(crate)     function: Option<StreamFunctionCall>,
}

#[derive(Deserialize)]
pub(crate) struct StreamFunctionCall {
    pub(crate)     name: Option<String>,
    pub(crate)     arguments: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PermissionMode {
    /// Permit reads, but reject all mutations and shell commands.
    ReadOnly,
    /// Prompt before writes and edits; reads are always permitted.
    AskWrites,
    /// Prompt before shell commands; reads and file mutations are permitted.
    AskShell,
    /// Permit every tool without prompting (useful for automation).
    Trusted,
}

impl PermissionMode {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().replace('_', "-").as_str() {
            "read-only" | "readonly" => Ok(Self::ReadOnly),
            "ask-writes" | "ask-write" => Ok(Self::AskWrites),
            "ask-shell" | "ask-commands" => Ok(Self::AskShell),
            "trusted" | "non-interactive" => Ok(Self::Trusted),
            other => Err(format!(
                "invalid permission mode '{}'; use read-only, ask-writes, ask-shell, or trusted",
                other
            )),
        }
    }

    pub(crate) fn from_env_or_file(file: &FileConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let value = env::var("AK_PERMISSION")
            .ok()
            .or_else(|| file.permission.clone())
            .unwrap_or_else(|| "ask-writes".to_string());
        Self::parse(&value).map_err(Into::into)
    }
}
