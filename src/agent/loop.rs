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
/// Disabled by default (0) — set DEX_WRAPUP_NUDGE=1 to re-enable.
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

fn capture_git_context(cancel: &dyn CancellationSource) -> Option<String> {
    // Opt-in: pi has no per-turn git snapshot. Enable with DEX_GIT_CONTEXT=1
    // when you need branch/dirty awareness without paying shell cost.
    if std::env::var("DEX_GIT_CONTEXT").as_deref() != Ok("1") {
        return None;
    }
    let mut parts = Vec::new();
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
        PermissionMode::ReadOnly => !matches!(name, "read" | "ffgrep" | "fffind" | "git"),
        PermissionMode::AskWrites => matches!(name, "write" | "edit" | "bash"),
        PermissionMode::AskShell => name == "bash",
        PermissionMode::Trusted => false,
    };
    denied.then(|| format!("Error: tool '{}' requires approval; use --permission trusted or configure DEX_PERMISSION", name))
}

fn audit_approval(name: &str, input: &str, decision: &str) {
    if std::env::var("DEX_AUDIT").as_deref() != Ok("1") {
        return;
    }
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
            let title = crate::core::format::approval_title(name);
            let summary = crate::core::format::approval_summary(name, input);
            let details = crate::core::format::approval_details(name, input);
            eprintln!();
            eprintln!("  ┌─ Approval required ─────────────────────────────────");
            eprintln!("  │ {} — {}", title, name);
            eprintln!("  │ {}", summary);
            for line in details.iter().take(6) {
                if line == &summary {
                    continue;
                }
                eprintln!("  │ {}", line);
            }
            eprintln!("  └──────────────────────────────────────────────────────");
            eprint!("  [y] allow once  [s] allow for session  [n] deny > ");
        });
    }
    let _ = io::stdout().flush();
    let mut answer = String::new();
    let _ = io::stdin().read_line(&mut answer);
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => {
            audit_approval(name, input, "once");
            true
        }
        "s" | "session" => {
            audit_approval(name, input, "session");
            console.record_session_approval(name, input);
            true
        }
        _ => {
            audit_approval(name, input, "deny");
            false
        }
    }
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
    // Capture the write/edit before/after diff while the file still holds the
    // "before" state: it decorates the approval prompt and, on success, the
    // tool-result preview. Display-only — the model keeps seeing plain text.
    let change_diff = if matches!(name.as_str(), "write" | "edit") {
        crate::tools::change_diff(&name, &args)
    } else {
        None
    };
    // P8: show a diff preview before requiring approval for a write/edit on
    // the non-trusted path, so the user sees what would change.
    if matches!(name.as_str(), "write" | "edit")
        && matches!(
            permission,
            PermissionMode::AskWrites | PermissionMode::AskShell
        )
    {
        if let Some(diff) = &change_diff {
            if let Some(sink) = console.sink() {
                let _ = sink.send(SinkLine::System(format!("[change preview]\n{diff}")));
            }
        }
    }
    if !approve_tool(permission, &name, &input, console) {
        return (
            name.clone(),
            input,
            ToolOutcome {
                text: format!("Error: permission denied for tool '{}'", name),
                ok: false,
                diff: None,
            },
        );
    }
    let mut outcome = execute_outcome(&name, &args, cancel);
    if outcome.ok {
        outcome.diff = change_diff;
    }
    (name.clone(), input, outcome)
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
) -> std::io::Result<()> {
    if let Some(session) = session.as_deref_mut() {
        for message in messages.get(*cursor..).unwrap_or_default() {
            session.append_message(message.clone())?;
        }
        *cursor = messages.len();
    }
    Ok(())
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
) -> Result<(ChatMessage, Option<Usage>), Box<dyn std::error::Error>> {
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

    // Turn id for the trace journal: session entry counter at turn start.
    let turn_id = session.as_deref().map(|s| s.count()).unwrap_or(0);

    // Task contract may tighten the per-turn budget (P8+): the plan's budget
    // wins when stricter than config; otherwise config applies.
    let turn_plan = load_plan(&session);
    let plan_budget = turn_plan.budget.clone();
    let mut limits = crate::agent::state::TurnLimits {
        elapsed_seconds: config.max_turn_seconds,
    };
    if let Some(b) = &plan_budget {
        if let Some(s) = b.max_seconds {
            limits.elapsed_seconds = limits.elapsed_seconds.min(s);
        }
    }
    let iteration_cap = plan_budget
        .as_ref()
        .and_then(|b| b.max_tool_iterations)
        .map(|i| (i as usize).min(config.max_tool_iterations))
        .unwrap_or(config.max_tool_iterations);
    // Cost budget: accumulated prompt-token spend (approximate rate below).
    #[allow(clippy::cast_precision_loss, clippy::cast_lossless)]
    let cost_budget = plan_budget.as_ref().and_then(|b| b.max_cost_usd);
    let mut spend_usd = 0.0f64;
    let turn_deadline = crate::agent::r#loop::deadline(limits);
    console.trace_span(serde_json::json!({
        "kind": "turn",
        "turn_id": turn_id,
        "budget_seconds": limits.elapsed_seconds,
        "budget_iterations": iteration_cap,
    }));

    // Turn-scoped context, computed once: plan snapshot + git status/diff.
    // These are EPHEMERAL — injected into each model call but never pushed
    // into `messages`, so they don't accumulate per iteration and don't
    // survive into the persisted session.
    if !turn_plan.is_empty() {
        if let Some(sink) = console.sink() {
            let _ = sink.send(SinkLine::Plan(turn_plan.clone()));
        }
    }
    let plan_text = plan_injection(&turn_plan);
    let git_ctx = capture_git_context(cancel);
    // Verification command comes from config only (P9). Auto-detection lives
    // at the daemon/one-shot boundary (llm::config::detect_verify_command) so
    // the shared loop can never re-run a project's own test suite by accident.
    let verify_command = config.verify_command.clone();
    for iteration in 0..iteration_cap {
        if !within_budget(turn_deadline) {
            return Err(format!(
                "turn exceeded the configured time limit ({} seconds); \
                 raise DEX_MAX_TURN_SECONDS or clear the task budget to allow longer turns",
                limits.elapsed_seconds
            )
            .into());
        }
        if cost_budget.is_some_and(|b| spend_usd > b) {
            return Err(format!(
                "task cost budget exceeded (${spend_usd:.2} > ${:.2}); clear the budget or increase it",
                cost_budget.unwrap_or(0.0)
            )
            .into());
        }
        persist_pending(&mut session, messages, &mut persisted_cursor)?;
        if cancellation.is_cancelled() {
            let _ = cancellation.take_cancelled();
            return Err("cancelled by user".into());
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
                persist_pending(&mut session, messages, &mut persisted_cursor)?;
            }
        }
        // Ephemeral preamble this iteration: plan + optional wrap-up nudge.
        // pi has no nudge; dex enables it only with DEX_WRAPUP_NUDGE=1.
        let nudge_text: Option<String> = if std::env::var("DEX_WRAPUP_NUDGE").as_deref() == Ok("1")
            && iteration_cap.saturating_sub(iteration) == WRAP_UP_THRESHOLD
        {
            Some(
                concat!(
                "[System] You are approaching the tool-call limit for this turn. ",
                "Before continuing, briefly re-evaluate: (1) why so many tool calls were needed ",
                "— e.g. repeated reads, failed edits, or exploring the wrong paths; (2) what the ",
                "user's actual task goal is and the shortest path remaining to reach it. Then ",
                "recover toward that goal: avoid repeating failed approaches, prefer ",
                "batched/broader tool calls over many small ones, and if the goal is already ",
                "(partially) met, state what was accomplished, what remains, and produce your ",
                "final answer now."
            )
                .to_string(),
            )
        } else {
            None
        };

        // -------- Proactive compaction BEFORE the model call --------
        // Estimate effective prompt = persistent history + ephemeral preamble + tool schema
        let ephemerals: [Option<String>; 3] = [
            git_ctx.clone().map(|s| format!("[Turn context]\n{s}")),
            plan_text.clone(),
            nudge_text.clone(),
        ];
        let ephemeral_tokens = estimate_ephemeral_tokens(&ephemerals);
        // Compact loop: keep trying until under threshold or nothing left to compact.
        // This replaces the old post-call reactive compaction that allowed
        // overflow between call and next check.
        let mut compaction_attempts = 0;
        while compaction_attempts < 3 {
            let eff = effective_tokens(messages, &ephemerals, true);
            let need_by_tokens = eff > config.compaction_threshold()
                || eff
                    > config
                        .max_prompt_tokens
                        .saturating_sub(config.reserve_tokens());
            let need_by_count = messages.len() > 1 + KEEP_RECENT_MESSAGES;
            if !need_by_tokens && !need_by_count {
                break;
            }
            match compact_history(config, messages, cancel) {
                Ok(true) => {
                    compaction_attempts += 1;
                    // Re-persist the compacted history so a session reload
                    // sees the same compacted view as memory: clear marker,
                    // then the summary + recent window. The system prompt is
                    // rebuilt at load, so skip index 0.
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
                Err(e) => {
                    // Non-cancel failures already fell back to the
                    // deterministic summary inside compact_history; an Err
                    // here means cancellation. Propagate.
                    return Err(e.into());
                }
            }
        }

        // Hard-limit check AFTER compaction, including ephemeral overhead.
        let eff_after = effective_tokens(messages, &ephemerals, true);
        if eff_after > config.max_prompt_tokens {
            return Err(format!(
                "prompt ({} tokens) exceeds max_prompt_tokens ({}); {} history messages \
                 plus {} tokens of turn context. Compaction already ran — narrow the task \
                 or raise DEX_CONTEXT_WINDOW / DEX_MAX_PROMPT_TOKENS.",
                eff_after,
                config.max_prompt_tokens,
                messages.len(),
                ephemeral_tokens
            )
            .into());
        }

        // Build effective messages for this model call (persistent + ephemeral).
        let mut effective_messages: Vec<ChatMessage> = messages.clone();
        if let Some(ctx) = &git_ctx {
            effective_messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(format!("[Turn context]\n{ctx}")),
                tool_calls: None,
                tool_call_id: None,
                name: Some("context".to_string()),
            });
        }
        if let Some(text) = &plan_text {
            effective_messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(text.clone()),
                tool_calls: None,
                tool_call_id: None,
                name: Some("plan".to_string()),
            });
        }
        if let Some(nudge) = &nudge_text {
            effective_messages.push(ChatMessage {
                role: "user".to_string(),
                content: Some(nudge.clone()),
                tool_calls: None,
                tool_call_id: None,
                name: Some("system-nudge".to_string()),
            });
        }

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
            // Persist promptly so a cancelled turn still keeps an accurate
            // context figure, and push it to the UI: the status bar tracks
            // usage after every LLM call, not once per turn.
            state.last_usage = last_usage;
            state.last_cached = u.cached_tokens;
            if let Some(sink) = console.sink() {
                let _ = sink.send(SinkLine::Usage {
                    tokens: u.prompt_tokens,
                    cached: u.cached_tokens,
                });
            }
            // Cost accounting (P9): a static approximate prompt-token rate —
            // `DEX_COST_PER_1K` overrides; default $2/M. Real pricing needs
            // provider tables, so this is a redacted, order-of-magnitude
            // figure that answers "did it cost cents or dollars", not a bill.
            #[allow(clippy::cast_precision_loss)]
            let rate_per_1k = env::var("DEX_COST_PER_1K")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.002);
            #[allow(clippy::cast_precision_loss)]
            let cost = u.prompt_tokens as f64 * rate_per_1k / 1000.0;
            spend_usd += cost;
            console.trace_span(serde_json::json!({
                "kind": "llm",
                "turn_id": turn_id,
                "iteration": iteration,
                "prompt_tokens": u.prompt_tokens,
                "cached_tokens": u.cached_tokens,
                "cost_usd": (cost * 1000.0).round() / 1000.0,
            }));
        }
        // Post-call compaction is intentionally removed: the next iteration's
        // pre-call compaction handles any overflow from tool results just pushed.
        // This prevents the old order-bug where overflow between push and next
        // call's hard-limit check was fatal.
        if let Some(calls) = message.tool_calls.clone() {
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: message.content,
                tool_calls: Some(calls.clone()),
                tool_call_id: None,
                name: None,
            });
            // Durable effect journal (P8): record intent for every call
            // BEFORE executing and snapshot mutation before-content, so a
            // crash mid-batch leaves `effect_start` rows that a restart can
            // reconcile against `effect_result`. Content capped to the same
            // limit the change ledger stores.
            struct EffectPlan {
                effect_hash: String,
                before_path: Option<String>,
                before: Option<String>,
                before_hash: String,
            }
            let mut effect_plans: Vec<EffectPlan> = Vec::new();
            for call in &calls {
                let raw = call.function.arguments.clone();
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                use std::hash::{Hash, Hasher};
                raw.hash(&mut hasher);
                let effect_hash = format!("{:016x}", hasher.finish());
                if let Some(s) = session.as_deref_mut() {
                    s.effect_start(&call.id, &call.function.name, &effect_hash)?;
                }
                let mut plan = EffectPlan {
                    effect_hash,
                    before_path: None,
                    before: None,
                    before_hash: crate::tools::hash_file(""),
                };
                if matches!(call.function.name.as_str(), "write" | "edit") {
                    if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                        if let Some(p) = v.get("path").and_then(Value::as_str) {
                            plan.before_path = Some(p.to_string());
                            plan.before_hash = crate::tools::hash_file(p);
                            plan.before = std::fs::read_to_string(p)
                                .ok()
                                .filter(|c| c.len() <= 64 * 1024);
                        }
                    }
                }
                effect_plans.push(plan);
            }

            // Execute all tool calls in this assistant message in parallel.
            // pi parallelizes everything except conflicting paths / sequential tools.
            // dex previously serialized the whole batch on any mutation; now only
            // bash (unscoped mutation) or conflicting paths force serialization -
            // write/edit on distinct files run in parallel.
            let has_bash = calls.iter().any(|c| c.function.name == "bash");
            let serialize_batch = has_bash || tool_calls_conflict(&calls);
            let batch_has_mutation = calls
                .iter()
                .any(|call| crate::tools::is_mutating(&call.function.name));
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
                                    diff: None,
                                },
                                Duration::ZERO,
                            )
                        })
                    })
                    .collect()
            };

            for (index, (call, (name, input, mut outcome, elapsed))) in
                calls.iter().zip(results).enumerate()
            {
                let change_diff = outcome.diff.take();
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
                    // write/edit shows the full captured diff (color-coded in
                    // the TUI); everything else keeps the 3-line preview.
                    let preview = match (ok, change_diff.as_deref()) {
                        (true, Some(diff)) => diff_preview_lines(diff, 400),
                        _ => tool_result_preview(&result, 3, skip_first),
                    };
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
                });
                persist_pending(&mut session, messages, &mut persisted_cursor)?;
                // — durable outcome + change ledger + trace (P8/P9) —
                if let Some(s) = session.as_deref_mut() {
                    let _ = s.effect_result(&call.id, ok);
                }
                if matches!(name.as_str(), "write" | "edit") && ok {
                    if let Some(plan) = effect_plans.get(index) {
                        if let Some(path) = &plan.before_path {
                            let after_hash = crate::tools::hash_file(path);
                            let after = std::fs::read_to_string(path)
                                .ok()
                                .filter(|c| c.len() <= 64 * 1024);
                            let record = crate::session::make_change_record(
                                &name,
                                path,
                                plan.before.as_deref(),
                                after.as_deref(),
                                &plan.before_hash,
                                &after_hash,
                            );
                            if let Some(s) = session.as_deref_mut() {
                                let _ = crate::session::record_change(s, record);
                            }
                        }
                    }
                }
                console.trace_span(serde_json::json!({
                    "kind": "tool",
                    "turn_id": turn_id,
                    "tool_call_id": call.id,
                    "name": name,
                    "ok": ok,
                    "duration_ms": elapsed.as_millis() as u64,
                    "approval_required": matches!(
                        config.permission,
                        PermissionMode::AskWrites | PermissionMode::AskShell
                    ),
                    "effect_hash": effect_plans
                        .get(index)
                        .map(|p| p.effect_hash.clone())
                        .unwrap_or_default(),
                }));
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
            // — stuck detection — opt-in with DEX_STUCK_DETECT=1 (pi has none).
            if std::env::var("DEX_STUCK_DETECT").as_deref() == Ok("1") {
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
                    persist_pending(&mut session, messages, &mut persisted_cursor)?;
                    if escalation_count == 3 {
                        search_streak = 0;
                    }
                }
            }
            // — verification hook — opt-in only when verify_command is explicitly
            // configured (DEX_VERIFY / config file). No auto-detect by default.
            // pi has no verify hook; dex keeps it for correctness gates when asked.
            if let Some(cmd) = verify_command.clone() {
                if state.verify_dirty
                    && within_budget(turn_deadline)
                    && iteration + 1 < iteration_cap
                {
                    if cancel.is_cancelled() {
                        return Err("cancelled by user".into());
                    }
                    state.verify_dirty = false;
                    let started = Instant::now();
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
                        // P9: persist the green disposition so "done" links
                        // to a recorded check.
                        if let Some(s) = session.as_deref_mut() {
                            let _ = s.set_state(
                                "verify",
                                &serde_json::json!({
                                    "disposition": "pass",
                                    "command": cmd,
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                })
                                .to_string(),
                            );
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
                        persist_pending(&mut session, messages, &mut persisted_cursor)?;
                        // P9: persist the failing disposition so the next
                        // turn (and the trace) can see verification failed.
                        if let Some(s) = session.as_deref_mut() {
                            let _ = s.set_state(
                                "verify",
                                &serde_json::json!({
                                    "disposition": "fail",
                                    "command": cmd,
                                    "tail_hash": format!("{hash:016x}"),
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                })
                                .to_string(),
                            );
                        }
                    }
                    console.trace_span(serde_json::json!({
                        "kind": "verify",
                        "turn_id": turn_id,
                        "command": cmd,
                        "ok": vres.ok,
                        "duration_ms": started.elapsed().as_millis() as u64,
                    }));
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
            persist_pending(&mut session, messages, &mut persisted_cursor)?;
            return Ok(text);
        }
    }
    Err(format!(
        "too many tool iterations (budget: {}): the task did not complete within the per-turn \
             tool-call budget. Partial progress (if any) is preserved in the conversation. \
             To continue, you can ask me to resume the task — optionally on a new path or \
             with a different approach — e.g. \"continue from where you left off\" or \
             \"try a different approach\".",
        iteration_cap
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
        ) -> Result<(ChatMessage, Option<Usage>), Box<dyn std::error::Error>> {
            Ok((
                ChatMessage {
                    role: "assistant".into(),
                    content: Some("hello from mock".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
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
                SinkLine::Usage { tokens, .. } => Some(*tokens),
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
            ) -> Result<(ChatMessage, Option<Usage>), Box<dyn std::error::Error>> {
                self.captured.lock().unwrap().push(messages.to_vec());
                Ok((
                    ChatMessage {
                        role: "assistant".into(),
                        content: Some("done".into()),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    },
                    Some(Usage {
                        prompt_tokens: 1,
                        cached_tokens: None,
                    }),
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
            constraints: vec!["no new deps".into()],
            acceptance: vec![("tests pass".into(), true)],
            budget: None,
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
        // The full task contract (constraints + acceptance) rides the same injection.
        let injected = first
            .iter()
            .find(|m| m.name.as_deref() == Some("plan"))
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        assert!(
            injected.contains("Constraints:\n- no new deps"),
            "{injected}"
        );
        assert!(injected.contains("[x] 1. tests pass"), "{injected}");
        // Turn-start git context is now opt-in (DEX_GIT_CONTEXT=1), so not
        // required for correctness. Verify that at least the plan was injected.
        // When DEX_GIT_CONTEXT=1 the context message will also be present.
        assert!(first.iter().any(|m| m.name.as_deref() == Some("plan")));
    }

    /// Mock that plays a scripted sequence of responses and captures every
    /// prompt it was called with.
    #[derive(Clone)]
    struct ScriptedMock {
        responses: std::sync::Arc<Vec<ChatMessage>>,
        round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        captured: std::sync::Arc<std::sync::Mutex<Vec<Vec<ChatMessage>>>>,
    }

    impl ScriptedMock {
        fn new(responses: Vec<ChatMessage>) -> Self {
            Self {
                responses: std::sync::Arc::new(responses),
                round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                captured: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    impl ModelClient for ScriptedMock {
        fn complete(
            &self,
            messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &dyn CancellationSource,
        ) -> Result<(ChatMessage, Option<Usage>), Box<dyn std::error::Error>> {
            let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.captured.lock().unwrap().push(messages.to_vec());
            let reply = self
                .responses
                .get(round)
                .cloned()
                .unwrap_or_else(|| ChatMessage {
                    role: "assistant".into(),
                    content: Some("done".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            Ok((reply, None))
        }
    }

    fn tool_call_msg(name: &str, args: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![LlmToolCall {
                id: "call-x".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: name.into(),
                    arguments: args.into(),
                },
            }]),
            tool_call_id: None,
            name: None,
        }
    }

    fn text_msg(text: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".into(),
            content: Some(text.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn plan_context_is_ephemeral_injected_every_call_but_never_persisted() {
        let cwd = format!(
            "/tmp/dex-ephemeral-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        );
        let mut session = Session::new(cwd.clone(), None).unwrap();
        let plan = crate::core::types::Plan {
            goal: Some("ephemeral goal".into()),
            steps: vec![("step".into(), false)],
            ..Default::default()
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
        // Two tool rounds then a text answer: the plan must be injected into
        // all three prompts, but must not accumulate in `messages`.
        let mock = ScriptedMock::new(vec![
            tool_call_msg("bash", r#"{"command":"echo hi"}"#),
            tool_call_msg("bash", r#"{"command":"echo again"}"#),
            text_msg("done"),
        ]);
        let captured = mock.captured.clone();
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
        assert_eq!(res.unwrap(), "done");

        let all = captured.lock().unwrap();
        assert_eq!(all.len(), 3, "three model calls");
        for (i, prompt) in all.iter().enumerate() {
            assert!(
                prompt.iter().any(|m| m.name.as_deref() == Some("plan")
                    && m.content
                        .as_deref()
                        .unwrap_or("")
                        .contains("ephemeral goal")),
                "prompt {i} must carry the plan"
            );
        }
        // The persistent history contains NO plan/nudge/context messages —
        // they are call-time injections, not accumulated turns.
        for m in &messages {
            assert_ne!(m.name.as_deref(), Some("plan"), "plan leaked into history");
            assert_ne!(
                m.name.as_deref(),
                Some("system-nudge"),
                "nudge leaked into history"
            );
        }
    }

    #[test]
    fn compaction_runs_before_the_model_call_and_session_stays_consistent() {
        let cwd = format!(
            "/tmp/dex-compact-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        );
        let mut session = Session::new(cwd.clone(), None).unwrap();
        let mut messages = vec![ChatMessage {
            role: "system".into(),
            content: Some("sys".into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }];
        // 30 history messages: over the KEEP_RECENT threshold, so the FIRST
        // iteration must compact before calling the model. (This is the
        // regression for the old order bug where the hard-limit check ran
        // before compaction and killed the turn.)
        for i in 0..15 {
            messages.push(ChatMessage {
                role: "user".into(),
                content: Some(format!("question {i} about module file_{i}.rs")),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
            messages.push(ChatMessage {
                role: "assistant".into(),
                content: Some(format!("answer {i} edited file_{i}.rs")),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        let initial_len = messages.len();
        let mut state = ToolState::default();
        let mock = ScriptedMock::new(vec![
            tool_call_msg("bash", r#"{"command":"echo hi"}"#),
            text_msg("done"),
        ]);
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
        let reloaded = session
            .path()
            .and_then(|p| crate::session::load_messages_from_session(p).ok());
        if let Some(p) = session_path {
            let _ = std::fs::remove_file(p);
        }
        assert_eq!(res.unwrap(), "done");
        // The first model call already saw compacted history.
        {
            let all = mock.captured.lock().unwrap();
            assert!(
                all[0].len() < initial_len,
                "first model call must use compacted history: {} vs {initial_len}",
                all[0].len()
            );
            assert!(
                all[0].iter().any(|m| m.name.as_deref() == Some("summary")),
                "first model call must contain the summary"
            );
        }
        // In-memory history shrank and carries the summary.
        assert!(messages.len() < initial_len);
        assert_eq!(messages[1].name.as_deref(), Some("summary"));
        // Session reload sees the same compacted view as memory (no stale
        // pre-compaction turns resurrected).
        let reloaded = reloaded.expect("session reload");
        let expected: Vec<serde_json::Value> = messages[1..]
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        let got: Vec<serde_json::Value> = reloaded
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        assert_eq!(got, expected, "session file must match compacted memory");
    }

    /// Mock that always issues a bash tool call — forces iteration growth.
    #[derive(Clone)]
    struct AlwaysTool;
    impl ModelClient for AlwaysTool {
        fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &dyn CancellationSource,
        ) -> Result<(ChatMessage, Option<Usage>), Box<dyn std::error::Error>> {
            Ok((
                tool_call_msg("bash", "{\"command\":\"echo hi\"}"),
                Some(Usage {
                    prompt_tokens: 1,
                    cached_tokens: None,
                }),
            ))
        }
    }

    #[test]
    fn plan_budget_caps_iterations_earlier_than_config() {
        let cwd = format!(
            "/tmp/dex-budget-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        );
        let mut session = Session::new(cwd.clone(), None).unwrap();
        let plan = crate::core::types::Plan {
            goal: Some("budgeted".into()),
            steps: vec![("s".into(), false)],
            constraints: vec![],
            acceptance: vec![],
            budget: Some(crate::core::types::Budget {
                max_seconds: None,
                max_tool_iterations: Some(2),
                max_cost_usd: None,
            }),
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
        let config = test_config(); // max_tool_iterations = 8
        let session_path = session.path().map(|p| p.to_path_buf());
        let res = process_turn(
            &config,
            &mut messages,
            &mut state,
            None,
            None,
            Some(&mut session),
            &AlwaysTool,
            &NeverCancel,
            &crate::core::console::Console::none(),
        );
        if let Some(p) = session_path {
            let _ = std::fs::remove_file(p);
        }
        // The turn-level iteration cap from the plan (2) beats config (8).
        let err = res.unwrap_err().to_string();
        assert!(err.contains("2"), "{err}");
    }

    #[test]
    fn effect_journal_verify_disposition_and_trace_are_written() {
        let cwd = format!(
            "/tmp/dex-trace-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        );
        let mut session = Session::new(cwd.clone(), None).unwrap();
        let session_dir = session.path().unwrap().parent().unwrap().to_path_buf();
        let trace_path = session.path().unwrap().with_extension("trace.jsonl");
        let mut messages = vec![ChatMessage {
            role: "system".into(),
            content: Some("sys".into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }];
        let mut state = ToolState::default();
        let mut config = test_config();
        // A verification command that passes immediately.
        config.verify_command = Some("true".into());
        let (sink_tx, _sink_rx) = mpsc::channel::<SinkLine>();
        let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequest>();
        let console = crate::core::console::Console::daemon(sink_tx, approval_tx).with_trace(Some(
            crate::core::console::TraceWriter::open(trace_path.clone()).unwrap(),
        ));
        let res = process_turn(
            &config,
            &mut messages,
            &mut state,
            None,
            None,
            Some(&mut session),
            &ToolThenAnswer::new(),
            &NeverCancel,
            &console,
        );
        assert_eq!(res.unwrap(), "done");

        // P8: effect intent + outcome recorded durably around the tool call.
        let log = std::fs::read_to_string(session.path().unwrap()).unwrap();
        assert!(log.contains("effect_start"), "{log}");
        assert!(log.contains("effect_result"), "{log}");
        // P9: green verification disposition persisted.
        let state_map = crate::session::load_session_state(session.path().unwrap()).unwrap();
        let verify = state_map.get("verify").unwrap();
        assert!(verify.contains("\"disposition\":\"pass\""), "{verify}");
        // P9: redacted trace journal holds llm/tool/verify/turn spans.
        let trace = std::fs::read_to_string(&trace_path).unwrap();
        for kind in ["turn", "llm", "tool", "verify"] {
            assert!(
                trace.contains(&format!("\"kind\":\"{kind}\"")),
                "missing {kind} span:\n{trace}"
            );
        }
        assert!(trace.contains("cost_usd"), "{trace}");
        assert!(
            !trace.contains("echo hi"),
            "trace must be redacted: {trace}"
        );

        let _ = std::fs::remove_file(session.path().unwrap());
        let _ = std::fs::remove_file(&trace_path);
        let _ = &session_dir;
    }
}
