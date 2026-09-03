use serde_json::Value;
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
) -> std::io::Result<()> {
    if let Some(session) = session.as_deref_mut() {
        for message in messages.get(*cursor..).unwrap_or_default() {
            session.append_message(message.clone())?;
        }
        *cursor = messages.len();
    }
    Ok(())
}

fn call_client_cancellable(
    client: &(impl ModelClient + Sync + Send + Clone + 'static),
    cancel: &(impl CancellationSource + Clone + Send + Sync + 'static),
    messages: &[ChatMessage],
    with_tools: bool,
    console: &Console,
) -> Result<(ChatMessage, Option<Usage>), Box<dyn std::error::Error>> {
    let client = (*client).clone();
    let messages = messages.to_vec();
    let sink = console.sink().cloned();
    let handle = cancel.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client
                .complete(&messages, with_tools, sink, &handle)
                .map_err(|error| error.to_string())
        }))
        .unwrap_or_else(|payload| {
            let detail = payload
                .downcast_ref::<&str>()
                .map(|message| (*message).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            Err(format!("provider worker panicked: {}", detail))
        });
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

fn execute_tool_call(
    call: &LlmToolCall,
    cancel: &dyn CancellationSource,
) -> (String, String, ToolOutcome) {
    let name = call.function.name.clone();
    let raw_args = call.function.arguments.clone();
    let value: Value = match serde_json::from_str(&raw_args) {
        Ok(value) => value,
        Err(error) => {
            return (
                name,
                raw_args,
                ToolOutcome {
                    text: format!("Error: invalid tool arguments: {}", error),
                    ok: false,
                    diff: None,
                },
            )
        }
    };
    let Some(args) = value.as_object().cloned() else {
        return (
            name,
            raw_args,
            ToolOutcome {
                text: "Error: tool arguments must be a JSON object".into(),
                ok: false,
                diff: None,
            },
        );
    };
    let input = serde_json::to_string(&args).unwrap_or_default();
    let outcome = execute_outcome(&name, &args, cancel);
    (name, input, outcome)
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
    let _working = SpinnerGuard::start(console, "Working");
    let mut last_tools: Vec<String> = Vec::new();
    let mut last_usage: Option<u64> = state.last_usage;
    let cancellation = cancel;
    let mut persisted_cursor = messages.len();

    for _iteration in 0..1_000_000 {
        persist_pending(&mut session, messages, &mut persisted_cursor)?;
        if cancellation.is_cancelled() {
            let _ = cancellation.take_cancelled();
            return Err("cancelled by user".into());
        }
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
                    ..Default::default()
                });
                persist_pending(&mut session, messages, &mut persisted_cursor)?;
            }
        }

        // Proactive compaction BEFORE model call
        let ephemerals: [Option<String>; 0] = [];
        let mut compaction_attempts = 0;
        while compaction_attempts < 3 {
            let eff = effective_tokens(messages, &ephemerals, true);
            let need_by_tokens = eff > config.compaction_threshold();
            let need_by_count = messages.len() > 1 + KEEP_RECENT_MESSAGES;
            if !need_by_tokens && !need_by_count {
                break;
            }
            match compact_history(config, messages, cancel) {
                Ok(true) => {
                    compaction_attempts += 1;
                    if let Some(session) = session.as_deref_mut() {
                        session.clear_messages()?;
                        for message in messages.iter().skip(1) {
                            session.append_message(message.clone())?;
                        }
                    }
                    persisted_cursor = messages.len();
                    continue;
                }
                Ok(false) => break,
                Err(e) if e.contains("cancelled") => return Err(e.into()),
                Err(e) => return Err(e.into()),
            }
        }

        let effective_messages: Vec<ChatMessage> = messages.clone();

        let (message, usage) =
            match call_client_cancellable(client, cancel, &effective_messages, true, console) {
                Ok(result) => result,
                Err(e) if e.to_string() == "interrupted" || e.to_string() == "cancelled" => {
                    return Err("cancelled by user".into());
                }
                Err(e) => return Err(e),
            };
        if let Some(u) = usage {
            last_usage = Some(u.prompt_tokens);
            state.last_usage = last_usage;
            state.last_cached = u.cached_tokens;
            if let Some(sink) = console.sink() {
                let _ = sink.send(SinkLine::Usage {
                    tokens: u.prompt_tokens,
                    cached: u.cached_tokens,
                });
            }
            let cost = crate::llm::config::cost_for_prompt(
                &config.model,
                u.prompt_tokens,
                u.cached_tokens,
            )
            .unwrap_or_else(|| {
                #[allow(clippy::cast_precision_loss)]
                let rate_per_1k = std::env::var("DEX_COST_PER_1K")
                    .ok()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.002);
                #[allow(clippy::cast_precision_loss)]
                {
                    u.prompt_tokens as f64 * rate_per_1k / 1000.0
                }
            });
            state.total_cost += cost;
        }

        if let Some(calls) = message.tool_calls.clone() {
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: message.content,
                tool_calls: Some(calls.clone()),
                tool_call_id: None,
                name: None,
                reasoning_items: message.reasoning_items.clone(),
                reasoning_content: message.reasoning_content.clone(),
            });

            let serialize_batch = tool_calls_conflict(&calls);
            let results: Vec<_> = if serialize_batch {
                let _guard = TOOL_MUTATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                calls
                    .iter()
                    .map(|call| {
                        let started = Instant::now();
                        let (name, input, outcome) = execute_tool_call(call, cancel);
                        (name, input, outcome, started.elapsed())
                    })
                    .collect()
            } else {
                calls
                    .iter()
                    .map(|call| {
                        let call = call.clone();
                        let cancel = cancel.clone();
                        thread::spawn(move || {
                            let started = Instant::now();
                            let (name, input, outcome) = execute_tool_call(&call, &cancel);
                            (name, input, outcome, started.elapsed())
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|handle| {
                        handle.join().unwrap_or_else(|_| {
                            (
                                String::new(),
                                String::new(),
                                ToolOutcome {
                                    text: "Error: tool worker panicked".into(),
                                    ok: false,
                                    diff: None,
                                },
                                Duration::ZERO,
                            )
                        })
                    })
                    .collect()
            };

            for (call, (name, input, outcome, elapsed)) in calls.iter().zip(results) {
                let cache_key = format!(
                    "{}:{}:{}{}",
                    std::env::current_dir()
                        .ok()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    name,
                    input,
                    cache_fingerprint(&name, &input)
                );
                let succeeded = outcome.ok;
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

                let cacheable = matches!(
                    name.as_str(),
                    "read" | "grep" | "ffgrep" | "find" | "fffind" | "ls"
                );
                let mut cache_hit = false;
                let mut ok = succeeded;
                let result = if repeated_count >= 3 {
                    ok = false;
                    "Error: repeated identical tool call; choose a different action or finish."
                        .to_string()
                } else if cacheable && succeeded {
                    if let Some(cached) = state.cache.get(&cache_key) {
                        cache_hit = true;
                        cached.clone()
                    } else {
                        state.insert(cache_key.clone(), outcome.text.clone());
                        outcome.text
                    }
                } else {
                    if matches!(name.as_str(), "write" | "edit") {
                        state.clear();
                    }
                    outcome.text
                };
                if let Some(sink) = console.sink() {
                    let mut summary = tool_result_summary(&name, &input, &result, ok);
                    if cache_hit {
                        summary = format!("cached · {summary}");
                    }
                    let counts_only = matches!(
                        name.as_str(),
                        "read" | "grep" | "ffgrep" | "find" | "fffind" | "ls" | "chain"
                    );
                    let skip_first = !counts_only || !ok;
                    let preview = tool_result_preview(&result, 3, skip_first);
                    let _ = sink.send(SinkLine::ToolOutput {
                        name: name.clone(),
                        summary,
                        success: ok,
                        preview,
                        duration: elapsed.as_secs_f64(),
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
                    ..Default::default()
                });
                persist_pending(&mut session, messages, &mut persisted_cursor)?;
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
                reasoning_items: message.reasoning_items.clone(),
                reasoning_content: message.reasoning_content.clone(),
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
                            ..Default::default()
                        });
                    }
                    state.last_usage = last_usage;
                    continue;
                }
            }
            state.last_usage = last_usage;
            persist_pending(&mut session, messages, &mut persisted_cursor)?;
            return Ok(text);
        }
    }
    Err("turn did not complete after many tool iterations; partial progress preserved.".into())
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
        ) -> Result<(ChatMessage, Option<Usage>), Box<dyn std::error::Error>> {
            Ok((
                ChatMessage {
                    role: "assistant".into(),
                    content: Some("hello from mock".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                    ..Default::default()
                },
                Some(Usage {
                    prompt_tokens: 1,
                    cached_tokens: None,
                }),
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
            endpoints: Default::default(),
            api: ApiProtocol::Responses,
            account_id: None,
            thinking_effort: None,
            context_window: 128_000,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            permission: PermissionMode::Trusted,
            verify_command: None,
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
            ..Default::default()
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

    #[derive(Clone)]
    struct ToolThenAnswer {
        round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ToolThenAnswer {
        fn new() -> Self {
            Self {
                round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl ModelClient for ToolThenAnswer {
        fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &dyn CancellationSource,
        ) -> Result<(ChatMessage, Option<Usage>), Box<dyn std::error::Error>> {
            let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let message = if round == 0 {
                ChatMessage {
                    role: "assistant".into(),
                    content: None,
                    tool_calls: Some(vec![crate::core::types::LlmToolCall {
                        id: "call-1".into(),
                        call_type: "function".into(),
                        function: crate::core::types::FunctionCall {
                            name: "bash".into(),
                            arguments: r#"{"command":"echo line-one; echo line-two; echo line-three; echo line-four"}"#.into(),
                        },
                    }]),
                    tool_call_id: None,
                    name: None,
                                    ..Default::default()
                }
            } else {
                ChatMessage {
                    role: "assistant".into(),
                    content: Some("done".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                    ..Default::default()
                }
            };
            Ok((
                message,
                Some(Usage {
                    prompt_tokens: 1,
                    cached_tokens: None,
                }),
            ))
        }
    }

    #[test]
    fn tool_result_streams_summary_preview_and_success() {
        let config = test_config();
        let mut messages = vec![ChatMessage {
            role: "system".into(),
            content: Some("sys".into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            ..Default::default()
        }];
        let mut state = ToolState::default();
        let (sink_tx, sink_rx) = mpsc::channel();
        let (approval_tx, _approval_rx) = mpsc::channel();
        let _ = process_turn(
            &config,
            &mut messages,
            &mut state,
            None,
            None,
            None,
            &ToolThenAnswer::new(),
            &NeverCancel,
            &crate::core::console::Console::daemon(sink_tx, approval_tx),
        );
        let events: Vec<_> = sink_rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, SinkLine::ToolInput(_))));
        assert!(events
            .iter()
            .any(|e| matches!(e, SinkLine::ToolOutput { .. })));
    }
}
