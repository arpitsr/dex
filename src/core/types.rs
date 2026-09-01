use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::mpsc;

#[derive(Clone, Debug)]
pub(crate) struct Skill {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) path: PathBuf,
}

/// Optional per-task budget, part of the durable task contract (P8+). Each
/// field, when set, caps the corresponding turn dimension; the agent loop
/// enforces the tightest of config and budget.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_iterations: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
}

/// Durable task contract: goal, constraints, acceptance criteria, plan steps
/// with done flags, budget, and a derived completion state. Persisted as JSON in
/// `session_state "plan"`; `#[serde(default)]` on new fields keeps older
/// JSONL loadable (a `1`-era session restores with empty constraints/acceptance).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub goal: Option<String>,
    pub steps: Vec<(String, bool)>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<(String, bool)>,
    #[serde(default)]
    pub budget: Option<Budget>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.goal.is_none()
            && self.steps.is_empty()
            && self.constraints.is_empty()
            && self.acceptance.is_empty()
            && self.budget.is_none()
    }
    /// Every step done and every acceptance criterion checked (vacuous when
    /// a list is empty). Never complete without at least one step.
    pub fn is_complete(&self) -> bool {
        !self.steps.is_empty()
            && self.steps.iter().all(|(_, d)| *d)
            && self.acceptance.iter().all(|(_, d)| *d)
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
    pub fn from_json(s: &str) -> Self {
        serde_json::from_str(s).unwrap_or_default()
    }
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut out = String::new();
        if let Some(g) = &self.goal {
            out.push_str(&format!("Goal: {}\n", g));
        }
        if !self.constraints.is_empty() {
            out.push_str("Constraints:\n");
            for c in &self.constraints {
                out.push_str(&format!("- {c}\n"));
            }
        }
        if !self.steps.is_empty() {
            out.push_str("Plan:\n");
            for (i, (text, done)) in self.steps.iter().enumerate() {
                out.push_str(&format!(
                    "{} {}. {}\n",
                    if *done { "[x]" } else { "[ ]" },
                    i + 1,
                    text
                ));
            }
        }
        if !self.acceptance.is_empty() {
            out.push_str("Acceptance:\n");
            for (i, (text, done)) in self.acceptance.iter().enumerate() {
                out.push_str(&format!(
                    "{} {}. {}\n",
                    if *done { "[x]" } else { "[ ]" },
                    i + 1,
                    text
                ));
            }
        }
        if let Some(budget) = &self.budget {
            let mut parts = Vec::new();
            if let Some(s) = budget.max_seconds {
                parts.push(format!("{s}s"));
            }
            if let Some(i) = budget.max_tool_iterations {
                parts.push(format!("{i} tool iterations"));
            }
            if let Some(c) = budget.max_cost_usd {
                parts.push(format!("${c:.2}"));
            }
            if !parts.is_empty() {
                out.push_str(&format!(
                    "Budget: {} — stay within these limits\n",
                    parts.join(", ")
                ));
            }
        }
        if self.is_complete() {
            out.push_str(
                "Task complete: every plan step and acceptance criterion is done — summarize the outcome and stop.\n",
            );
        }
        Some(out.trim_end().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_contract_summary_includes_completion_state() {
        let plan = Plan {
            goal: Some("g".into()),
            constraints: vec!["c".into()],
            steps: vec![("s".into(), true)],
            acceptance: vec![("a".into(), true)],
            budget: None,
        };
        let s = plan.summary().unwrap();
        assert!(s.contains("Goal: g"));
        assert!(s.contains("Constraints:\n- c"));
        assert!(s.contains("[x] 1. s"));
        assert!(s.contains("[x] 1. a"));
        assert!(s.contains("Task complete"));
        assert!(plan.is_complete());

        // One unchecked acceptance criterion keeps it incomplete and drops the stop line.
        let mut partial = plan.clone();
        partial.acceptance[0].1 = false;
        assert!(!partial.is_complete());
        assert!(!partial.summary().unwrap().contains("Task complete"));
        // No steps → never complete even with everything else done.
        let mut no_steps = plan.clone();
        no_steps.steps.clear();
        assert!(!no_steps.is_complete());
    }

    #[test]
    fn plan_round_trip_keeps_contract_and_loads_legacy_json() {
        let plan = Plan {
            goal: Some("g".into()),
            constraints: vec!["c1".into()],
            steps: vec![("s".into(), false)],
            acceptance: vec![("a1".into(), true)],
            budget: None,
        };
        assert_eq!(Plan::from_json(&plan.to_json()), plan);
        // Pre-level-2 JSONL (no constraints/acceptance keys) still loads.
        let legacy = r#"{"goal":"g","steps":[["s",false]]}"#;
        let parsed = Plan::from_json(legacy);
        assert!(parsed.constraints.is_empty() && parsed.acceptance.is_empty());
        assert_eq!(parsed.steps, vec![("s".to_string(), false)]);
        assert!(parsed.budget.is_none());
    }

    #[test]
    fn budget_round_trips_and_shows_in_summary() {
        let plan = Plan {
            goal: Some("g".into()),
            steps: vec![("s".into(), false)],
            constraints: vec![],
            acceptance: vec![],
            budget: Some(Budget {
                max_seconds: Some(120),
                max_tool_iterations: Some(5),
                max_cost_usd: Some(0.5),
            }),
        };
        assert_eq!(Plan::from_json(&plan.to_json()), plan);
        let s = plan.summary().unwrap();
        assert!(s.contains("Budget: 120s, 5 tool iterations, $0.50"), "{s}");
        // Budget-only plan is not empty.
        let budget_only = Plan {
            budget: Some(Budget {
                max_seconds: Some(10),
                ..Budget::default()
            }),
            ..Plan::default()
        };
        assert!(!budget_only.is_empty());
    }
}

/// A single streamed line destined for the UI transcript. Plain text (no
/// ANSI) so the UI applies its own styling. Ratatui-agnostic.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum SinkLine {
    Assistant(String),
    /// Incremental model reasoning ("thinking") delta. UIs render a collapsed
    /// one-line preview and can expand the full text on demand.
    Thinking(String),
    ToolInput(String),
    ToolOutput {
        name: String,
        summary: String,
        /// Whether the tool call succeeded; rendered as ✓/✗ by the UIs.
        success: bool,
        /// A few informational output lines shown dim under the summary.
        preview: Vec<String>,
        /// Wall-clock seconds the tool took; 0 when unknown.
        duration: f64,
    },
    System(String),
    Error(String),
    /// Prompt tokens reported by the provider after each LLM call, so the
    /// status bar can track context usage live instead of once per turn.
    /// `cached` is the provider-reported cached-token subset, when reported.
    Usage {
        tokens: u64,
        cached: Option<u64>,
    },
    Plan(Plan),
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
    pub(crate) role: String,
    pub(crate) content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_calls: Option<Vec<LlmToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct LlmToolCall {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) call_type: String,
    pub(crate) function: FunctionCall,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct FunctionCall {
    pub(crate) name: String,
    pub(crate) arguments: String,
}

#[derive(Serialize)]
pub(crate) struct ChatRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) tools: Vec<ToolDefinition>,
    pub(crate) stream: bool,
    pub(crate) stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_effort: Option<String>,
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
    pub(crate) include_usage: bool,
}

/// Provider-reported usage for one LLM call, threaded from the stream readers
/// through the agent loop to the status bar. `cached_tokens` is the
/// provider-reported cache-hit subset (billed at a fraction of full input
/// price); None when the provider does not report cache detail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub(crate) prompt_tokens: u64,
    pub(crate) cached_tokens: Option<u64>,
}

/// Chat-completions wire shape for usage. Cache detail nests under
/// `prompt_tokens_details`, so it needs its own deserialization target.
#[derive(Deserialize, Default)]
pub(crate) struct StreamUsage {
    pub(crate) prompt_tokens: u64,
    #[serde(rename = "prompt_tokens_details")]
    pub(crate) prompt_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize, Default)]
pub(crate) struct PromptTokensDetails {
    #[serde(default)]
    pub(crate) cached_tokens: u64,
}

#[derive(Serialize)]
pub(crate) struct ToolDefinition {
    #[serde(rename = "type")]
    pub(crate) tool_type: String,
    pub(crate) function: FunctionDef,
}

#[derive(Serialize)]
pub(crate) struct FunctionDef {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: Value,
}

#[derive(Deserialize)]
pub(crate) struct StreamChunk {
    #[serde(default)]
    pub(crate) choices: Vec<StreamChoice>,
    #[serde(default)]
    pub(crate) usage: Option<StreamUsage>,
}

#[derive(Deserialize)]
pub(crate) struct StreamChoice {
    #[serde(default)]
    pub(crate) delta: StreamDelta,
}

#[derive(Default, Deserialize)]
pub(crate) struct StreamDelta {
    pub(crate) content: Option<String>,
    pub(crate) tool_calls: Option<Vec<StreamToolCall>>,
    /// Reasoning deltas arrive under provider-specific keys (OpenRouter
    /// `reasoning`, DeepSeek-style `reasoning_content`) and some providers
    /// send non-string shapes; `Value` keeps a stray shape from failing the
    /// whole chunk parse.
    #[serde(default)]
    pub(crate) reasoning: Option<Value>,
    #[serde(default)]
    pub(crate) reasoning_content: Option<Value>,
}

#[derive(Deserialize)]
pub(crate) struct StreamToolCall {
    pub(crate) index: usize,
    pub(crate) id: Option<String>,
    pub(crate) function: Option<StreamFunctionCall>,
}

#[derive(Deserialize)]
pub(crate) struct StreamFunctionCall {
    pub(crate) name: Option<String>,
    pub(crate) arguments: Option<String>,
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
    pub(crate) fn permissiveness(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::AskWrites => 1,
            Self::AskShell => 2,
            Self::Trusted => 3,
        }
    }
}
