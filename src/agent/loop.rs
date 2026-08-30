use serde_json::Value;
use std::env;
use std::io::{self, IsTerminal, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::agent::compaction::*;
use crate::agent::state::*;
use crate::core::console::*;
use crate::core::format::*;
use crate::core::types::*;
use crate::llm::client::*;
use crate::llm::config::*;
use crate::session::*;
use crate::tools::*;

pub(crate) fn deadline(limits: TurnLimits) -> Instant {
    Instant::now() + Duration::from_secs(limits.elapsed_seconds)
}

pub(crate) fn within_budget(deadline: Instant) -> bool {
    Instant::now() < deadline
}

/// Maximum number of model round-trips within a single turn.
/// When this many iterations remain, nudge the model to wrap up.
pub(crate) const WRAP_UP_THRESHOLD: usize = 5;

pub(crate) fn permission_denied(mode: PermissionMode, name: &str) -> Option<String> {
    let denied = match mode {
        PermissionMode::ReadOnly => !matches!(name, "read" | "grep" | "find" | "git"),
        PermissionMode::AskWrites => matches!(name, "write" | "edit" | "bash"),
        PermissionMode::AskShell => name == "bash",
        PermissionMode::Trusted => false,
    };
    denied.then(|| format!("Error: tool '{}' requires approval; use --permission trusted or configure OYE_PERMISSION", name))
}

pub(crate) fn approve_tool(
    mode: PermissionMode,
    name: &str,
    input: &str,
    console: &Console,
) -> bool {
    if mode == PermissionMode::ReadOnly
        && crate::tools::metadata(name).is_some_and(|metadata| !metadata.read_only)
    {
        if let Some(sink) = console.sink() {
            let _ = sink.send(SinkLine::System(format!(
                "Denied {}: read-only permission mode",
                name
            )));
        } else {
            with_console(console.sink().is_some(), || {
                eprintln!("Denied {}: read-only permission mode", name)
            });
        }
        return false;
    }
    if permission_denied(mode, name).is_none() {
        return true;
    }
    if console.session_approved(name) {
        return true;
    }
    // Remote approval: daemon sends the request via SSE and blocks for the
    // client's POST response. We skip the stdin terminal check entirely.
    if console.remote_approval {
        if let Some(approval_sink) = console.approval() {
            let (response_tx, response_rx) = mpsc::channel();
            if approval_sink
                .send(ApprovalRequest {
                    name: name.to_string(),
                    input: input.to_string(),
                    response: response_tx,
                })
                .is_ok()
            {
                return match response_rx.recv().unwrap_or(ApprovalDecision::Deny) {
                    ApprovalDecision::Once => true,
                    ApprovalDecision::Session => {
                        console.record_session_approval(name);
                        true
                    }
                    ApprovalDecision::Deny => false,
                };
            }
        }
        return false;
    }
    if !io::stdin().is_terminal() {
        return false;
    }
    if console.sink().is_some() {
        if let Some(approval_sink) = console.approval() {
            let (response_tx, response_rx) = mpsc::channel();
            if approval_sink
                .send(ApprovalRequest {
                    name: name.to_string(),
                    input: input.to_string(),
                    response: response_tx,
                })
                .is_ok()
            {
                return match response_rx.recv().unwrap_or(ApprovalDecision::Deny) {
                    ApprovalDecision::Once => true,
                    ApprovalDecision::Session => {
                        console.record_session_approval(name);
                        true
                    }
                    ApprovalDecision::Deny => false,
                };
            }
        }
    } else {
        with_console(console.sink().is_some(), || {
            eprint!("Approve {} {}? [y/N] ", name, terminal_preview(input))
        });
    }
    let _ = io::stdout().flush();
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).is_ok()
        && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub(crate) fn execute_tool_call(
    call: &LlmToolCall,
    permission: PermissionMode,
    cancel: &dyn CancellationSource,
    console: &Console,
) -> (String, String, String) {
    let name = call.function.name.clone();
    let raw_args = call.function.arguments.clone();
    let value: Value = match serde_json::from_str(&raw_args) {
        Ok(value) => value,
        Err(error) => {
            return (
                name,
                raw_args,
                format!("Error: invalid tool arguments: {}", error),
            )
        }
    };
    let Some(args) = value.as_object().cloned() else {
        return (
            name,
            raw_args,
            "Error: tool arguments must be a JSON object".into(),
        );
    };
    let input = serde_json::to_string(&args).unwrap_or_default();
    if !approve_tool(permission, &name, &input, console) {
        return (
            name.clone(),
            input,
            format!("Error: permission denied for tool '{}'", name),
        );
    }
    (name.clone(), input, execute_to_string(&name, &args, cancel))
}

pub(crate) fn tool_calls_conflict(calls: &[LlmToolCall]) -> bool {
    let mut paths = std::collections::HashSet::new();
    calls.iter().any(|call| {
        let Ok(value) = serde_json::from_str::<Value>(&call.function.arguments) else {
            return false;
        };
        let Some(path) = value.get("path").and_then(Value::as_str) else {
            return false;
        };
        !paths.insert(path.to_string())
    })
}

pub(crate) fn persist_pending(
    session: &mut Option<&mut Session>,
    messages: &[ChatMessage],
    cursor: &mut usize,
) {
    if let Some(session) = session.as_deref_mut() {
        for message in messages.get(*cursor..).unwrap_or_default() {
            let _ = session.append_message(message.clone());
        }
        *cursor = messages.len();
    }
}

/// Run a blocking model call behind a small polling boundary so the turn can
/// return promptly when cancellation is requested. Mirrors the prior
/// `call_llm_cancellable` behavior but takes the client and cancellation source
/// as parameters so the loop is unit-testable without network access or crate
/// globals.
fn call_client_cancellable(
    client: &(impl ModelClient + Sync + Send + Clone + 'static),
    cancel: &(impl CancellationSource + Clone + Send + Sync + 'static),
    messages: &[ChatMessage],
    with_tools: bool,
    console: &Console,
) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    let client = (*client).clone();
    let messages = messages.to_vec();
    let sink = console.sink().cloned();
    // The worker outlives this call when cancelled early; it polls a private
    // clone of the source so the stream read unwinds too.
    let handle = cancel.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = client
            .complete(&messages, with_tools, sink, &handle)
            .map_err(|error| error.to_string());
        let _ = tx.send(result);
    });
    loop {
        if cancel.is_cancelled() {
            return Err("cancelled by user".into());
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(result)) => return Ok(result),
            Ok(Err(error)) => return Err(error.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("provider worker disconnected".into())
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn process_turn(
    config: &LlmConfig,
    messages: &mut Vec<ChatMessage>,
    state: &mut ToolState,
    steering_rx: Option<&mpsc::Receiver<String>>,
    steering_accepted_tx: Option<&mpsc::Sender<String>>,
    mut session: Option<&mut Session>,
    client: &(impl ModelClient + Sync + Send + Clone + 'static),
    cancel: &(impl CancellationSource + Clone + Send + Sync + 'static),
    console: &Console,
) -> Result<String, Box<dyn std::error::Error>> {
    // Spins while the agent works; erased automatically on return.
    let _working = SpinnerGuard::start(console, "Working");
    let mut last_tools: Vec<String> = Vec::new();
    let mut last_usage: Option<u64> = state.last_usage;
    let cancellation = cancel;
    let mut persisted_cursor = messages.len();

    let limits = crate::agent::state::TurnLimits {
        elapsed_seconds: config.max_turn_seconds,
    };
    let turn_deadline = crate::agent::r#loop::deadline(limits);
    for iteration in 0..config.max_tool_iterations {
        if !within_budget(turn_deadline) {
            return Err("turn exceeded configured time limit".into());
        }
        persist_pending(&mut session, messages, &mut persisted_cursor);
        if cancellation.is_cancelled() {
            let _ = cancellation.take_cancelled();
            return Err("cancelled by user".into());
        }
        if estimate_tokens(messages) > config.max_prompt_tokens {
            return Err("prompt exceeded configured token limit".into());
        }
        // Steering is consumed between turns/tool batches, while the worker
        // still owns the conversation state. This avoids concurrent mutation
        // of `messages` while allowing the UI to accept input immediately.
        if let Some(rx) = steering_rx {
            while let Ok(steering) = rx.try_recv() {
                if let Some(accepted) = &steering_accepted_tx {
                    let _ = accepted.send(steering.clone());
                }
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: Some(steering),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some("steering".to_string()),
                });
            }
        }
        // Nudge the model to finish as we approach the iteration budget.
        let remaining = config.max_tool_iterations.saturating_sub(iteration);
        if remaining == WRAP_UP_THRESHOLD {
            messages.push(ChatMessage {
                role: "user".to_string(),
                content: Some(
                    "[System] You are approaching the tool-call limit for this turn. \
                     Before continuing, briefly re-evaluate: (1) why so many tool \
                     calls were needed — e.g. repeated reads, failed edits, or \
                     exploring the wrong paths; (2) what the user's actual task goal \
                     is and the shortest path remaining to reach it. Then recover \
                     toward that goal: avoid repeating failed approaches, prefer \
                     batched/broader tool calls over many small ones, and if the goal \
                     is already (partially) met, state what was accomplished, what \
                     remains, and produce your final answer now."
                        .to_string(),
                ),
                tool_calls: None,
                tool_call_id: None,
                name: Some("system-nudge".to_string()),
            });
            persist_pending(&mut session, messages, &mut persisted_cursor);
        }
        let (message, usage) = match call_client_cancellable(client, cancel, messages, true, console)
        {
            Ok(result) => result,
            Err(e) if e.to_string() == "interrupted" || e.to_string() == "cancelled" => {
                return Err("cancelled by user".into());
            }
            Err(e) => return Err(e),
        };
        if usage.is_some() {
            last_usage = usage;
        }
        // Compact when either the message count or an estimated token
        // budget is exceeded (API-reported usage takes precedence).
        let est = last_usage.unwrap_or_else(|| estimate_tokens(messages));
        if messages.len() > 1 + KEEP_RECENT_MESSAGES || est > config.context_window / 2 {
            compact_history(config, messages, cancel)?;
        }
        if let Some(calls) = message.tool_calls.clone() {
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: message.content,
                tool_calls: Some(calls.clone()),
                tool_call_id: None,
                name: None,
            });
            // Execute all tool calls in this assistant message in parallel.
            let batch_has_mutation = calls
                .iter()
                .any(|call| crate::tools::is_mutating(&call.function.name));
            let serialize_batch = batch_has_mutation || tool_calls_conflict(&calls);
            let results: Vec<_> = if serialize_batch {
                let _guard = TOOL_MUTATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                calls
                    .iter()
                    .map(|call| execute_tool_call(call, config.permission, cancel, console))
                    .collect()
            } else {
                calls
                    .iter()
                    .map(|call| {
                        let call = call.clone();
                        let permission = config.permission;
                        let console = console.clone();
                        let cancel = cancel.clone();
                        thread::spawn(move || {
                            execute_tool_call(&call, permission, &cancel, &console)
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|handle| {
                        handle.join().unwrap_or_else(|_| {
                            (
                                String::new(),
                                String::new(),
                                "Error: tool worker panicked".into(),
                            )
                        })
                    })
                    .collect()
            };

            for (call, (name, input, result)) in calls.iter().zip(results) {
                let cache_key = format!(
                    "{}:{}:{}{}",
                    env::current_dir()
                        .ok()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    name,
                    input,
                    cache_fingerprint(&name, &input)
                );
                // Only successful calls count toward the repeated-identical
                // limit; a failed call is a legitimate retry and must stay
                // allowed so the model can recover instead of being blocked.
                let succeeded = !result.starts_with("Error: ");
                if succeeded {
                    if last_tools.len() >= 6 {
                        last_tools.remove(0);
                    }
                    last_tools.push(cache_key.clone());
                }
                let repeated_count = last_tools.iter().filter(|k| **k == cache_key).count();
                if let Some(sink) = console.sink() {
                    let _ = sink.send(SinkLine::ToolInput(format!(
                        "{} {}",
                        call.function.name,
                        short_arg(&name, &input)
                    )));
                } else {
                    with_console(console.sink().is_some(), || {
                        eprintln!(
                            "{}[tool input] {} {}{}",
                            TOOL_INPUT_COLOR,
                            call.function.name,
                            terminal_preview(&input),
                            RESET
                        );
                    });
                }

                let cacheable = matches!(name.as_str(), "read" | "grep" | "find");
                let mut cache_hit = false;
                let result = if repeated_count >= 3 {
                    "Error: repeated identical tool call; choose a different action or finish."
                        .to_string()
                } else if cacheable && succeeded {
                    if let Some(cached) = state.cache.get(&cache_key) {
                        cache_hit = true;
                        if console.sink().is_none() {
                            with_console(console.sink().is_some(), || {
                                eprintln!("{}[tool cache hit]{}", TOOL_OUTPUT_COLOR, RESET)
                            });
                        }
                        cached.clone()
                    } else {
                        state.insert(cache_key, result.clone());
                        result
                    }
                } else {
                    if matches!(name.as_str(), "write" | "edit") {
                        state.clear();
                    }
                    result
                };
                if let Some(sink) = console.sink() {
                    let mut summary = tool_result_summary(&name, &result);
                    if cache_hit {
                        summary = format!("cached · {summary}");
                    }
                    let _ = sink.send(SinkLine::ToolOutput {
                        name: name.clone(),
                        summary,
                    });
                } else {
                    with_console(console.sink().is_some(), || {
                        eprintln!(
                            "{}[tool output] {}:\n{}{}",
                            TOOL_OUTPUT_COLOR,
                            name,
                            terminal_preview(&result),
                            RESET
                        );
                    });
                }
                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: Some(model_tool_result(&result)),
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                    name: None,
                });
                persist_pending(&mut session, messages, &mut persisted_cursor);
            }
            state.save();
        } else {
            let text = message.content.unwrap_or_default();
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: Some(text.clone()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
            if let Some(rx) = steering_rx {
                let steering: Vec<String> = rx.try_iter().collect();
                if !steering.is_empty() {
                    for content in steering {
                        if let Some(accepted) = &steering_accepted_tx {
                            let _ = accepted.send(content.clone());
                        }
                        messages.push(ChatMessage {
                            role: "user".to_string(),
                            content: Some(content),
                            tool_calls: None,
                            tool_call_id: None,
                            name: Some("steering".to_string()),
                        });
                    }
                    state.last_usage = last_usage;
                    continue;
                }
            }
            state.last_usage = last_usage;
            persist_pending(&mut session, messages, &mut persisted_cursor);
            return Ok(text);
        }
    }
    Err(
        "too many tool iterations: the task did not complete within the per-turn \
         tool-call budget. Partial progress (if any) is preserved in the conversation. \
         To continue, you can ask me to resume the task — optionally on a new path or \
         with a different approach — e.g. \"continue from where you left off\" or \
         \"try a different approach\"."
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::{CancellationSource, ToolState};
    use crate::core::types::{ApiProtocol, ChatMessage, PermissionMode, Provider};
    use crate::llm::client::ModelClient;

    #[derive(Clone)]
    struct MockModel;

    impl ModelClient for MockModel {
        fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &dyn CancellationSource,
        ) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
            Ok((
                ChatMessage {
                    role: "assistant".into(),
                    content: Some("hello from mock".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
                Some(1),
            ))
        }
    }

    #[derive(Clone)]
    struct NeverCancel;

    impl CancellationSource for NeverCancel {
        fn is_cancelled(&self) -> bool {
            false
        }
        fn take_cancelled(&self) -> bool {
            false
        }
    }

    fn test_config() -> LlmConfig {
        LlmConfig {
            provider: Provider::OpenCode,
            api_key: String::new(),
            base_url: String::new(),
            model: "mock".into(),
            available_models: vec!["mock".into()],
            api: ApiProtocol::Responses,
            account_id: None,
            thinking_effort: None,
            context_window: 128_000,
            permission: PermissionMode::Trusted,
            max_tool_iterations: 8,
            max_prompt_tokens: 128_000,
            max_turn_seconds: 60,
            client: reqwest::blocking::Client::new(),
        }
    }

    #[test]
    fn process_turn_completes_with_injected_client() {
        let config = test_config();
        let mut messages = vec![ChatMessage {
            role: "system".into(),
            content: Some("sys".into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }];
        let mut state = ToolState::default();
        let result = process_turn(
            &config,
            &mut messages,
            &mut state,
            None,
            None,
            None,
            &MockModel,
            &NeverCancel,
            &crate::core::console::Console::none(),
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "hello from mock");
        assert!(messages
            .iter()
            .any(|m| m.content.as_deref() == Some("hello from mock")));
    }
}
