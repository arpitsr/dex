use serde::{Deserialize, Serialize};

/// Request to create a new session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub cwd: String,
    pub name: Option<String>,
}

/// Response after creating a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
    pub path: String,
}

/// Summary of a session for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub path: String,
    pub name: Option<String>,
    pub cwd: String,
    pub created_at: String,
    pub message_count: usize,
}

/// Request to submit a chat prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub prompt: String,
    #[serde(default)]
    pub skill_dirs: Vec<String>,
}

/// Response to approve/deny a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

/// SSE event types streamed during a chat turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum StreamEvent {
    /// Incremental assistant text.
    #[serde(rename = "assistant_text")]
    AssistantText(String),

    /// A tool call was initiated.
    #[serde(rename = "tool_call")]
    ToolCall {
        name: String,
        args: serde_json::Value,
    },

    /// A tool call completed.
    #[serde(rename = "tool_result")]
    ToolResult {
        name: String,
        summary: String,
        success: bool,
    },

    /// The agent needs user approval for a tool.
    #[serde(rename = "approval_required")]
    ApprovalRequired {
        request_id: String,
        name: String,
        input: String,
    },

    /// The turn completed successfully.
    #[serde(rename = "turn_complete")]
    TurnComplete { response: String },

    /// The turn failed.
    #[serde(rename = "turn_failed")]
    TurnFailed { error: String },

    /// A system message (e.g. compaction notice).
    #[serde(rename = "system")]
    System(String),

    /// An error occurred.
    #[serde(rename = "error")]
    Error(String),
}

impl StreamEvent {
    /// Serialize to SSE `data:` format.
    pub fn to_sse(&self) -> String {
        let json = serde_json::to_string(self).expect("StreamEvent should always serialize");
        format!("data: {json}\n\n")
    }
}

/// Daemon configuration sent by the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct DaemonConfig {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub permission: Option<String>,
}
