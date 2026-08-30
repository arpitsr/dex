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

/// Summary of a session for listing. Constructed via serde from the
/// daemon's `GET /api/sessions` response.
#[allow(dead_code)]
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
    /// Optional per-request overrides; when absent the daemon uses its own
    /// environment/config file. These let a co-located client forward its
    /// CLI flags through to the turn.
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission: Option<String>,
}

/// Response to approve/deny a tool execution. `request_id` must match the
/// `ApprovalRequired` stream event the decision resolves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub request_id: String,
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
        /// A few informational output lines to show under the summary.
        #[serde(default)]
        preview: Vec<String>,
        /// Wall-clock seconds the tool took; 0 when unknown.
        #[serde(default)]
        duration: f64,
    },

    /// The agent needs user approval for a tool.
    #[serde(rename = "approval_required")]
    ApprovalRequired {
        request_id: String,
        name: String,
        input: String,
    },

    /// The turn completed successfully. `usage` is the daemon-reported prompt
    /// token count for the conversation, when known.
    #[serde(rename = "turn_complete")]
    TurnComplete {
        response: String,
        #[serde(default)]
        usage: Option<u64>,
    },

    /// The turn failed.
    #[serde(rename = "turn_failed")]
    TurnFailed { error: String },

    /// Prompt tokens reported by the provider after each LLM call within a
    /// turn, letting the client render live context usage in its status bar.
    #[serde(rename = "usage")]
    Usage { tokens: u64 },

    /// A system message (e.g. compaction notice).
    #[serde(rename = "system")]
    System(String),

    /// An error occurred.
    #[serde(rename = "error")]
    Error(String),
}

/// Runtime info about the daemon, returned by `GET /api/config`. The client
/// TUI uses it for the status footer and slash-command suggestions; the
/// daemon resolves provider/model/permission from its own environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub provider: String,
    pub model: String,
    pub available_models: Vec<String>,
    pub context_window: u64,
    pub permission: String,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub git_dirty: bool,
}
