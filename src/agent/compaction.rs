
use crate::core::format::*;
use crate::llm::client::*;
use crate::llm::config::*;
use crate::core::types::*;

pub(crate) fn estimate_tokens(messages: &[ChatMessage]) -> u64 {
    let chars: usize = messages
        .iter()
        .map(|message| {
            message.content.as_deref().map_or(0, str::len)
                + message.tool_calls.as_ref().map_or(0, |calls| {
                    calls
                        .iter()
                        .map(|call| call.function.arguments.len() + call.function.name.len())
                        .sum()
                })
        })
        .sum();
    (chars as u64) / 4
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
) -> Result<String, Box<dyn std::error::Error>> {
    let transcript: String = old
        .iter()
        .map(message_to_transcript)
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = vec![
        ChatMessage {
            role: "system".to_string(),
            content: Some(
                "Summarize the following conversation excerpt between a coding agent and \
                 the user. Preserve: the user's goals and requests, key facts learned about \
                 the codebase (files, paths, important symbols), decisions made, actions \
                 already taken and their outcomes, and any unresolved tasks. Be concise — \
                 at most 15 lines. Output only the summary."
                    .to_string(),
            ),
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
    let (summary, _) = call_llm(config, &prompt, false)?;
    Ok(summary.content.unwrap_or_default())
}

pub(crate) fn compact_history(config: &LlmConfig, messages: &mut Vec<ChatMessage>) -> Result<(), String> {
    // Find where the protected recent window begins (never split a
    // tool_call/tool pairing, so back up to the last non-tool message).
    let total = messages.len();
    if total <= 1 + KEEP_RECENT_MESSAGES {
        return Ok(());
    }
    let mut cutoff = total - KEEP_RECENT_MESSAGES;
    while cutoff > 1 && matches!(messages[cutoff].role.as_str(), "tool") {
        cutoff -= 1;
    }
    if cutoff <= 1 + MIN_MESSAGES_TO_SUMMARIZE {
        return Ok(()); // too little to summarize
    }
    if messages[cutoff].role == "assistant" && messages[cutoff].tool_calls.is_some() {
        return Ok(()); // would orphan tool calls; skip compaction this round
    }

    // Summarize the old segment (between the first user message and cutoff).
    let old: Vec<ChatMessage> = messages[1..cutoff].to_vec();
    let summarized = match summarize_old_messages(config, &old) {
        Ok(s) => s,
        Err(e) => {
            return Err(format!(
                "history compaction failed: {}; no context was discarded",
                e
            ))
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
    Ok(())
}
