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

fn load_plan(session: &Option<&mut Session>) -> crate::core::types::Plan {
    session
        .as_ref()
        .and_then(|s| s.path().map(crate::session::load_plan))
        .unwrap_or_default()
}

fn plan_injection(plan: &crate::core::types::Plan) -> Option<String> {
    plan.summary().map(|s| format!("[Plan context]\n{s}"))
}

fn turn_start_context(
    plan: &crate::core::types::Plan,
    cancel: &dyn CancellationSource,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(p) = plan.summary() {
        parts.push(p);
    }
    // Git snapshot — reuse tool_git via direct execute so timeout/cancellation applies.
    for mode in ["status", "diff"] {
        let mut args = serde_json::Map::new();
        args.insert("mode".into(), serde_json::Value::String(mode.into()));
        if let Ok(out) = crate::tools::execute("git", &args, cancel) {
            let trimmed = out.trim();
            if !trimmed.is_empty()
                && !trimmed.contains("not a git repository")
                && !trimmed.contains("fatal:")
            {
                let header = if mode == "status" {
                    "git status:"
                } else {
                    "git diff --stat:"
                };
                parts.push(format!("{header}\n{trimmed}"));
            }
        }
        if parts.join("\n").lines().count() >= 10 {
            break;
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut msg = parts.join("\n\n");
    // Cap ~10 lines
    let lines: Vec<&str> = msg.lines().collect();
    if lines.len() > 10 {
        msg = format!("{}\n[... truncated]", lines[..10].join("\n"));
    }
    Some(format!("[Turn context]\n{msg}"))
}

fn stuck_nudge(attempt: usize, reason: &str) -> String {
    let guidance = match attempt {
        1 => "Try a direct fix for the immediate error (check arguments, file paths, or exact oldText).",
        2 => "Inspect surrounding architecture/files for context you may be missing.",
        3 => "Question the assumption behind the approach — maybe the goal needs a different path.",
        _ => "Propose a re-plan to the user: summarize progress, state what failed, and suggest the next steps.",
    };
    format!("[System] Stuck detected ({reason}) — escalation {attempt}: {guidance}")
}

pub(crate) fn permission_denied(mode: PermissionMode, name: &str) -> Option<String> {
    let denied = match mode {
        PermissionMode::ReadOnly => !matches!(name, "read" | "grep" | "find" | "git"),
        PermissionMode::AskWrites => matches!(name, "write" | "edit" | "bash"),
        PermissionMode::AskShell => name == "bash",
        PermissionMode::Trusted => false,
    };
    denied.then(|| format!("Error: tool '{}' requires approval; use --permission trusted or configure DEX_PERMISSION", name))
}

fn audit_approval(name: &str, input: &str, decision: &str) {
    let Some(base) = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
    else {
        return;
    };
    let path = base.join("dex/audit.jsonl");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&input, &mut hasher);
    use std::hash::Hasher;
    let input_hash = format!("{:016x}", hasher.finish());
    let record = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "cwd": std::env::current_dir().ok().map(|p| p.display().to_string()),
        "tool": name,
        "input_hash": input_hash,
        "decision": decision,
        "actor": "local",
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let mut line = record.to_string();
        line.push('\n');
        let _ = std::io::Write::write_all(&mut file, line.as_bytes());
    }
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
    if console.session_approved(name, input) {
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
                    ApprovalDecision::Once => {
                        audit_approval(name, input, "once");
                        true
                    }
                    ApprovalDecision::Session => {
                        audit_approval(name, input, "session");
                        console.record_session_approval(name, input);
                        true
                    }
                    ApprovalDecision::Deny => {
                        audit_approval(name, input, "deny");
                        false
                    }
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
                    ApprovalDecision::Once => {
                        audit_approval(name, input, "once");
                        true
                    }
                    ApprovalDecision::Session => {
                        audit_approval(name, input, "session");
                        console.record_session_approval(name, input);
                        true
                    }
                    ApprovalDecision::Deny => {
                        audit_approval(name, input, "deny");
                        false
                    }
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
            },
        );
    };
    let input = serde_json::to_string(&args).unwrap_or_default();
    if !approve_tool(permission, &name, &input, console) {
        return (
            name.clone(),
            input,
            ToolOutcome {
                text: format!("Error: permission denied for tool '{}'", name),
                ok: false,
            },
        );
    }
    (name.clone(), input, execute_outcome(&name, &args, cancel))
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
        // A panic inside the provider stack must reach the channel as an
        // error (with its message), not surface as an opaque "worker
        // disconnected" after the sender silently drops.
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
    let mut last_failed: Vec<String> = Vec::new();
    let mut edit_paths: Vec<String> = Vec::new();
    let mut search_streak: usize = 0;
    let mut last_verify_hash: Option<u64> = None;
    let mut escalation_count: usize = 0;
    let mut last_usage: Option<u64> = state.last_usage;
    let cancellation = cancel;
    let mut persisted_cursor = messages.len();
    let mut turn_start_done = false;

    let limits = crate::agent::state::TurnLimits {
        elapsed_seconds: config.max_turn_seconds,
    };
    let turn_deadline = crate::agent::r#loop::deadline(limits);
    for iteration in 0..config.max_tool_iterations {
        if !within_budget(turn_deadline) {
            return Err(format!(
                "turn exceeded the configured time limit ({} seconds); \
                 raise DEX_MAX_TURN_SECONDS to allow longer turns",
                limits.elapsed_seconds
            )
            .into());
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
        // Turn-start context injection (once): goal/plan + git snapshot.
        if !turn_start_done {
            turn_start_done = true;
            let plan = load_plan(&session);
            if let Some(ctx) = turn_start_context(&plan, cancel) {
                messages.push(ChatMessage {
                    role: "system".to_string(),
                    content: Some(ctx),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some("context".to_string()),
                });
                persist_pending(&mut session, messages, &mut persisted_cursor);
            }
            // Sync plan to UI once at turn start.
            let plan = load_plan(&session);
            if !plan.is_empty() {
                if let Some(sink) = console.sink() {
                    let _ = sink.send(SinkLine::Plan(plan));
                }
            }
        }
        // Plan injection before each model call.
        {
            let plan = load_plan(&session);
            if let Some(text) = plan_injection(&plan) {
                messages.push(ChatMessage {
                    role: "system".to_string(),
                    content: Some(text),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some("plan".to_string()),
                });
                persist_pending(&mut session, messages, &mut persisted_cursor);
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
        let (message, usage) =
            match call_client_cancellable(client, cancel, messages, true, console) {
                Ok(result) => result,
                Err(e) if e.to_string() == "interrupted" || e.to_string() == "cancelled" => {
                    return Err("cancelled by user".into());
                }
                Err(e) => return Err(e),
            };
        if let Some(tokens) = usage {
            last_usage = Some(tokens);
            // Persist promptly so a cancelled turn still keeps an accurate
            // context figure, and push it to the UI: the status bar tracks
            // usage after every LLM call, not once per turn.
            state.last_usage = last_usage;
            if let Some(sink) = console.sink() {
                let _ = sink.send(SinkLine::Usage(tokens));
            }
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
                    .map(|call| {
                        let started = Instant::now();
                        let (name, input, outcome) =
                            execute_tool_call(call, config.permission, cancel, console);
                        (name, input, outcome, started.elapsed())
                    })
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
                            let started = Instant::now();
                            let (name, input, outcome) =
                                execute_tool_call(&call, permission, &cancel, &console);
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

                let cacheable = matches!(name.as_str(), "read" | "grep" | "find");
                let mut cache_hit = false;
                let mut ok = succeeded;
                let result = if repeated_count >= 3 {
                    ok = false;
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
                    // Tools whose summary is a bare count get a preview from
                    // the top of their output; for the rest the summary
                    // already shows the first line, so preview continues
                    // after it. Failed calls always continue past the first
                    // line to expose the actual error detail.
                    let counts_only = matches!(name.as_str(), "read" | "grep" | "find" | "chain");
                    let skip_first = !counts_only || !ok;
                    let _ = sink.send(SinkLine::ToolOutput {
                        name: name.clone(),
                        summary,
                        success: ok,
                        preview: tool_result_preview(&result, 3, skip_first),
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
                });
                persist_pending(&mut session, messages, &mut persisted_cursor);
                // — stuck detection ledger —
                if !ok {
                    if last_failed.len() >= 6 {
                        last_failed.remove(0);
                    }
                    last_failed.push(cache_key.clone());
                }
                if matches!(name.as_str(), "edit" | "write") {
                    if let Ok(v) = serde_json::from_str::<Value>(&input) {
                        if let Some(p) = v.get("path").and_then(Value::as_str) {
                            if edit_paths.len() >= 6 {
                                edit_paths.remove(0);
                            }
                            edit_paths.push(p.to_string());
                        }
                    }
                }
                if matches!(name.as_str(), "grep" | "find") {
                    search_streak += 1;
                } else if name == "read" {
                    search_streak = 0;
                }
            }
            if batch_has_mutation {
                state.verify_dirty = true;
            }
            // — stuck detection — check patterns and escalate
            let mut stuck_reason: Option<String> = None;
            if last_failed
                .iter()
                .filter(|k| **k == last_failed.last().cloned().unwrap_or_default())
                .count()
                >= 3
            {
                stuck_reason = Some("identical failed tool calls".into());
            } else if edit_paths.len() >= 3
                && edit_paths[edit_paths.len() - 1] == edit_paths[edit_paths.len() - 2]
                && edit_paths[edit_paths.len() - 2] == edit_paths[edit_paths.len() - 3]
            {
                stuck_reason = Some(format!(
                    "repeated edits to {}",
                    edit_paths.last().unwrap_or(&String::new())
                ));
            } else if search_streak >= 4 {
                stuck_reason = Some("consecutive searches without a read".into());
            }
            if let Some(reason) = stuck_reason {
                escalation_count += 1;
                if escalation_count > 3 {
                    return Err(format!(
                        "stuck: {reason} — escalation limit reached; aborting turn. {}",
                        "Partial progress preserved; please re-plan."
                    )
                    .into());
                }
                let nudge = stuck_nudge(escalation_count, &reason);
                messages.push(ChatMessage {
                    role: "user".into(),
                    content: Some(nudge),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some("system-nudge".into()),
                });
                persist_pending(&mut session, messages, &mut persisted_cursor);
                if escalation_count == 3 {
                    search_streak = 0;
                }
            }
            // — verification hook —
            if let Some(cmd) = config.verify_command.clone() {
                if state.verify_dirty
                    && within_budget(turn_deadline)
                    && iteration + 1 < config.max_tool_iterations
                {
                    if cancel.is_cancelled() {
                        return Err("cancelled by user".into());
                    }
                    state.verify_dirty = false;
                    let mut vargs = serde_json::Map::new();
                    vargs.insert("command".into(), Value::String(cmd.clone()));
                    let vres = crate::tools::execute_outcome(
                        "bash",
                        &vargs,
                        cancel as &dyn CancellationSource,
                    );
                    let tail = vres
                        .text
                        .lines()
                        .rev()
                        .take(20)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n");
                    if vres.ok {
                        if let Some(sink) = console.sink() {
                            let _ = sink.send(SinkLine::System("verify \u{2713}".into()));
                        }
                    } else {
                        let hash = {
                            use std::collections::hash_map::DefaultHasher;
                            use std::hash::{Hash, Hasher};
                            let mut h = DefaultHasher::new();
                            vres.text.lines().next().unwrap_or("").hash(&mut h);
                            h.finish()
                        };
                        let same_sig = last_verify_hash == Some(hash);
                        last_verify_hash = Some(hash);
                        if same_sig {
                            escalation_count += 1;
                            if escalation_count > 3 {
                                return Err("verification repeatedly failed with same error — aborting turn.".into());
                            }
                            let nudge =
                                stuck_nudge(escalation_count, "verify failing with same signature");
                            messages.push(ChatMessage {
                                role: "user".into(),
                                content: Some(nudge),
                                tool_calls: None,
                                tool_call_id: None,
                                name: Some("system-nudge".into()),
                            });
                        }
                        messages.push(ChatMessage {
                            role: "user".into(),
                            content: Some(format!("[verify failed]\n{tail}")),
                            tool_calls: None,
                            tool_call_id: None,
                            name: Some("verify".into()),
                        });
                        persist_pending(&mut session, messages, &mut persisted_cursor);
                    }
                }
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
    Err(format!(
        "too many tool iterations (budget: {}): the task did not complete within the per-turn \
             tool-call budget. Partial progress (if any) is preserved in the conversation. \
             To continue, you can ask me to resume the task — optionally on a new path or \
             with a different approach — e.g. \"continue from where you left off\" or \
             \"try a different approach\".",
        config.max_tool_iterations
    )
    .into())
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

    /// Calls a tool on the first round, then answers with plain text.
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
        ) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
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
                }
            } else {
                ChatMessage {
                    role: "assistant".into(),
                    content: Some("done".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                }
            };
            Ok((message, Some(1)))
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
        }];
        let mut state = ToolState::default();
        let (sink_tx, sink_rx) = mpsc::channel();
        let (approval_tx, _approval_rx) = mpsc::channel();
        let console = crate::core::console::Console::daemon(sink_tx, approval_tx);

        let result = process_turn(
            &config,
            &mut messages,
            &mut state,
            None,
            None,
            None,
            &ToolThenAnswer::new(),
            &NeverCancel,
            &console,
        );
        assert_eq!(result.unwrap(), "done");

        let lines: Vec<SinkLine> = sink_rx.try_iter().collect();
        let outputs: Vec<&SinkLine> = lines
            .iter()
            .filter(|line| matches!(line, SinkLine::ToolOutput { .. }))
            .collect();
        let [SinkLine::ToolOutput {
            name,
            summary,
            success,
            preview,
            ..
        }] = outputs.as_slice()
        else {
            panic!("expected exactly one tool output, got {outputs:?}");
        };
        assert_eq!(name, "bash");
        assert!(success);
        // Summary carries the first output line; the preview continues after
        // it instead of repeating it.
        assert_eq!(summary, "line-one");
        assert_eq!(
            preview,
            &[
                "line-two".to_string(),
                "line-three".to_string(),
                "line-four".to_string()
            ]
        );

        // The mock makes two LLM calls (tool round, then answer), each
        // reporting usage: the status bar must get a Usage event per call.
        let usage_events: Vec<u64> = lines
            .iter()
            .filter_map(|line| match line {
                SinkLine::Usage(tokens) => Some(*tokens),
                _ => None,
            })
            .collect();
        assert_eq!(usage_events, vec![1, 1]);
    }

    #[test]
    fn plan_is_injected_before_first_model_call() {
        use std::sync::{Arc, Mutex};
        #[derive(Clone)]
        struct CapturingMock {
            captured: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
        }
        impl ModelClient for CapturingMock {
            fn complete(
                &self,
                messages: &[ChatMessage],
                _with_tools: bool,
                _sink: Option<mpsc::Sender<SinkLine>>,
                _cancel: &dyn CancellationSource,
            ) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
                self.captured.lock().unwrap().push(messages.to_vec());
                Ok((
                    ChatMessage {
                        role: "assistant".into(),
                        content: Some("done".into()),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    },
                    Some(1),
                ))
            }
        }
        // Create a persisted session with a plan (no global cwd change — leaves parallel tests alone).
        let cwd = format!(
            "/tmp/dex-plan-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        );
        let mut session = Session::new(cwd.clone(), None).unwrap();
        let plan = crate::core::types::Plan {
            goal: Some("test goal".into()),
            steps: vec![("step one".into(), false), ("step two".into(), true)],
        };
        crate::session::save_plan(&mut session, &plan).unwrap();
        let mut messages = vec![ChatMessage {
            role: "system".into(),
            content: Some("sys".into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }];
        let mut state = ToolState::default();
        let captured: Arc<Mutex<Vec<Vec<ChatMessage>>>> = Arc::new(Mutex::new(Vec::new()));
        let mock = CapturingMock {
            captured: captured.clone(),
        };
        let config = test_config();
        let session_path = session.path().map(|p| p.to_path_buf());
        let res = process_turn(
            &config,
            &mut messages,
            &mut state,
            None,
            None,
            Some(&mut session),
            &mock,
            &NeverCancel,
            &crate::core::console::Console::none(),
        );
        if let Some(p) = session_path {
            let _ = std::fs::remove_file(p);
        }
        assert!(res.is_ok());
        let all = captured.lock().unwrap();
        assert!(!all.is_empty());
        let first = &all[0];
        // First model call must contain the plan context.
        assert!(first.iter().any(|m| m.name.as_deref() == Some("plan")
            && m.content.as_deref().unwrap_or("").contains("test goal")));
        // And the turn-start context (plan summary) as well.
        assert!(first.iter().any(|m| m.name.as_deref() == Some("context")));
    }
}
