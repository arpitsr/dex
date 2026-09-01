use crate::agent::state::CancellationSource;
use crate::core::format::*;
use crate::core::types::*;
use crate::llm::client::*;
use crate::llm::config::*;

/// Token overhead per message (role, formatting, turn boundary).
const PER_MESSAGE_OVERHEAD: u64 = 12;
/// Rough cost of the tool definitions sent with every request.
const TOOL_SCHEMA_TOKENS: u64 = 3200;

pub(crate) fn estimate_tokens(messages: &[ChatMessage]) -> u64 {
    let chars: usize = messages
        .iter()
        .map(|message| {
            // Content + tool call payload + name/role
            let mut len = message.content.as_deref().map_or(0, str::len)
                + message.tool_calls.as_ref().map_or(0, |calls| {
                    calls
                        .iter()
                        .map(|call| {
                            call.function.arguments.len() + call.function.name.len() + call.id.len()
                        })
                        .sum()
                });
            // Role and name framing
            len += message.role.len();
            if let Some(name) = &message.name {
                len += name.len();
            }
            if let Some(tid) = &message.tool_call_id {
                len += tid.len();
            }
            len
        })
        .sum();
    // ~4 chars per token plus per-message overhead, and tool schema when
    // the caller will inject it ( caller adds TOOL_SCHEMA_TOKENS separately;
    // we keep estimator honest for history alone ).
    (chars as u64) / 4 + (messages.len() as u64 * PER_MESSAGE_OVERHEAD)
}

/// Estimate tokens for the ephemeral preamble that is injected at call-time
/// but not stored in `messages`.
pub(crate) fn estimate_ephemeral_tokens(parts: &[Option<String>]) -> u64 {
    let chars: usize = parts
        .iter()
        .filter_map(|p| p.as_ref())
        .map(|s| s.len())
        .sum();
    (chars as u64) / 4 + (parts.len() as u64 * PER_MESSAGE_OVERHEAD)
}

/// Effective prompt size = persistent history + ephemeral preamble + tool schema.
pub(crate) fn effective_tokens(
    messages: &[ChatMessage],
    ephemeral: &[Option<String>],
    with_tools: bool,
) -> u64 {
    estimate_tokens(messages)
        + estimate_ephemeral_tokens(ephemeral)
        + if with_tools { TOOL_SCHEMA_TOKENS } else { 0 }
}

/// Compact the conversation history to keep requests bounded:
/// replace old turns with a short summary, keeping the system prompt,
/// the first user message, and everything from the recent window intact.
pub(crate) const KEEP_RECENT_MESSAGES: usize = 12;

pub(crate) const MIN_MESSAGES_TO_SUMMARIZE: usize = 8;

/// Render a message as a compact transcript line for summarization.
pub(crate) fn message_to_transcript(msg: &ChatMessage) -> String {
    let body = msg.content.clone().unwrap_or_default();
    let role = match msg.role.as_str() {
        "assistant" if msg.tool_calls.is_some() => "assistant (tool calls)",
        other => other,
    };
    format!("{}: {}", role, truncate_text(&body, 2_000, 50))
}

pub(crate) fn summarize_old_messages(
    config: &LlmConfig,
    old: &[ChatMessage],
    cancel: &dyn CancellationSource,
) -> Result<String, Box<dyn std::error::Error>> {
    let transcript: String = old
        .iter()
        .map(message_to_transcript)
        .collect::<Vec<_>>()
        .join("\n");
    // Preserve orientation anchors verbatim so compaction never erases the task.
    let has_plan = old.iter().any(|m| m.name.as_deref() == Some("plan"));
    let has_verify = old.iter().any(|m| m.name.as_deref() == Some("verify"));
    let extra = if has_plan || has_verify {
        " Preserve the current goal and plan verbatim and any recent verification failures."
    } else {
        ""
    };
    let prompt = vec![
        ChatMessage {
            role: "system".to_string(),
            content: Some(format!(
                "Summarize the following conversation excerpt between a coding agent and \
                 the user. Preserve: the user's goals and requests, key facts learned about \
                 the codebase (files, paths, important symbols), decisions made, actions \
                 already taken and their outcomes, and any unresolved tasks.{extra} Be concise — \
                 at most 15 lines. Output only the summary."
            )),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        },
        ChatMessage {
            role: "user".to_string(),
            content: Some(transcript),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        },
    ];
    let (summary, _) = call_llm(config, &prompt, false, None, cancel)?;
    Ok(summary.content.unwrap_or_default())
}

/// Deterministic fallback when the LLM summarizer fails or is cancelled.
/// Keeps the user goal, touched paths, and verification failures without
/// needing a model call.
fn deterministic_summary(old: &[ChatMessage]) -> String {
    let mut goal: Option<String> = None;
    let mut files: Vec<String> = Vec::new();
    let mut verifies: Vec<String> = Vec::new();
    let mut decisions: Vec<String> = Vec::new();

    for msg in old {
        if msg.name.as_deref() == Some("plan") && goal.is_none() {
            if let Some(c) = &msg.content {
                goal = Some(truncate_text(c, 800, 10));
            }
        }
        if msg.name.as_deref() == Some("summary") && goal.is_none() {
            if let Some(c) = &msg.content {
                goal = Some(truncate_text(c, 800, 10));
            }
        }
        // Collect verify failures verbatim (last 2)
        if msg.name.as_deref() == Some("verify") {
            if let Some(c) = &msg.content {
                verifies.push(truncate_text(c, 600, 8));
            }
        }
        // Rough file path extraction from tool results / assistant content
        if let Some(c) = &msg.content {
            for token in c.split_whitespace() {
                if token.contains('/') && token.contains('.') && token.len() < 80 {
                    let clean = token.trim_matches(|ch: char| ",;:()[]\"'".contains(ch));
                    if clean.contains('/')
                        && !files.contains(&clean.to_string())
                        && files.len() < 20
                    {
                        files.push(clean.to_string());
                    }
                }
            }
        }
        if msg.role == "assistant" && msg.tool_calls.is_none() {
            if let Some(c) = &msg.content {
                if c.len() < 300 && decisions.len() < 5 {
                    decisions.push(truncate_text(c, 400, 4));
                }
            }
        }
    }
    let mut out = String::new();
    if let Some(g) = goal {
        out.push_str(&g);
        out.push('\n');
    } else if let Some(first) = old.iter().find(|m| m.role == "user") {
        if let Some(c) = &first.content {
            out.push_str(&truncate_text(c, 600, 8));
            out.push('\n');
        }
    }
    if !files.is_empty() {
        out.push_str(&format!("Touched: {}\n", files.join(", ")));
    }
    if !decisions.is_empty() {
        out.push_str("Progress: ");
        out.push_str(&decisions.join(" | "));
        out.push('\n');
    }
    if !verifies.is_empty() {
        let tail = verifies
            .iter()
            .rev()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        out.push_str(&format!("Verify failures:\n{}\n", tail));
    }
    if out.trim().is_empty() {
        out = format!(
            "Truncated {} earlier messages (no goal extracted).",
            old.len()
        );
    }
    out.trim().to_string()
}

/// Find the splice point that keeps the recent window intact and never
/// orphans a tool_call/tool pair: if the window would start on a `tool`
/// message, back up to the assistant that issued the batch. Anything at or
/// after the cutoff is kept whole, so an assistant-with-tool_calls at the
/// cutoff is fine (its results follow it). Returns None if there is nothing
/// large enough to summarize.
pub(crate) fn find_cutoff(messages: &[ChatMessage]) -> Option<usize> {
    let total = messages.len();
    if total <= 1 + KEEP_RECENT_MESSAGES {
        return None;
    }
    let mut cutoff = total - KEEP_RECENT_MESSAGES;
    while cutoff > 1 && messages[cutoff].role == "tool" {
        cutoff -= 1;
    }
    if cutoff <= 1 + MIN_MESSAGES_TO_SUMMARIZE {
        return None;
    }
    Some(cutoff)
}

pub(crate) fn compact_history(
    config: &LlmConfig,
    messages: &mut Vec<ChatMessage>,
    cancel: &dyn CancellationSource,
) -> Result<bool, String> {
    let cutoff = match find_cutoff(messages) {
        Some(c) => c,
        None => return Ok(false),
    };

    // Summarize the old segment (between the first user message and cutoff).
    let old: Vec<ChatMessage> = messages[1..cutoff].to_vec();
    let summarized = match summarize_old_messages(config, &old, cancel) {
        Ok(s) if !s.trim().is_empty() => s,
        Ok(_) => deterministic_summary(&old),
        Err(e) => {
            let msg = e.to_string();
            // Cancellation or transient API error should not abort the turn
            // if we can still make progress deterministically.
            if msg.contains("cancelled") || msg.contains("cancellation") {
                return Err(format!("history compaction cancelled: {msg}"));
            }
            deterministic_summary(&old)
        }
    };

    // If a previous summary exists, it sits at index 1 and is part of `old`,
    // so the fresh summary subsumes it. Replace everything before cutoff
    // with a single summary user message.
    let summary_msg = ChatMessage {
        role: "user".to_string(),
        content: Some(format!(
            "[Summary of earlier conversation]\n{}\n[End of summary. Recent messages follow.]",
            summarized.trim()
        )),
        tool_calls: None,
        tool_call_id: None,
        name: Some("summary".to_string()),
    };
    messages.splice(1..cutoff, std::iter::once(summary_msg));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn cutoff_never_lands_on_a_tool_message() {
        let mut messages = vec![msg("system", "sys")];
        for i in 0..20 {
            messages.push(msg("user", &format!("u{i}")));
            messages.push(msg("assistant", &format!("a{i}")));
        }
        // Force the naive window to start on a tool message: assistant with
        // calls, then tools, right at total - KEEP_RECENT.
        let call = LlmToolCall {
            id: "c1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: "{}".into(),
            },
        };
        messages.push(ChatMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![call]),
            tool_call_id: None,
            name: None,
        });
        for i in 0..KEEP_RECENT_MESSAGES - 1 {
            messages.push(ChatMessage {
                role: "tool".into(),
                content: Some(format!("t{i}")),
                tool_calls: None,
                tool_call_id: Some(format!("c1-{i}")),
                name: None,
            });
        }
        let cutoff = find_cutoff(&messages).expect("should have a cutoff");
        assert_ne!(
            messages[cutoff].role, "tool",
            "cutoff would orphan tool calls"
        );
        // Everything from cutoff on must be self-contained: first kept
        // message is either a user/assistant text message or an assistant
        // that owns the tool calls that follow it.
        let kept = &messages[cutoff..];
        assert!(
            kept[0].role != "tool"
                && (kept[0].tool_calls.is_some()
                    || kept
                        .iter()
                        .all(|m| m.tool_call_id.is_none()
                            || kept.iter().any(|a| a.tool_calls.is_some()))),
            "kept segment must start a coherent turn"
        );
    }

    #[test]
    fn cutoff_is_none_when_history_is_small() {
        let messages: Vec<ChatMessage> = std::iter::once(msg("system", "sys"))
            .chain((0..KEEP_RECENT_MESSAGES).map(|i| msg("user", &format!("m{i}"))))
            .collect();
        assert!(find_cutoff(&messages).is_none());
    }

    #[test]
    fn estimator_counts_tool_calls_and_message_overhead() {
        let call = LlmToolCall {
            id: "abc".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: "{\"path\":\"src/main.rs\"}".into(),
            },
        };
        let messages = vec![
            msg("system", "hello"),
            ChatMessage {
                role: "assistant".into(),
                content: None,
                tool_calls: Some(vec![call]),
                tool_call_id: None,
                name: None,
            },
        ];
        // 5 content chars + 30 tool-call chars + framing, /4 + overhead.
        let est = estimate_tokens(&messages);
        assert!(est >= 2, "estimator must not undercount to zero: {est}");
        // Empty history estimates to zero, not garbage.
        assert_eq!(estimate_tokens(&[]), 0);
    }
}
