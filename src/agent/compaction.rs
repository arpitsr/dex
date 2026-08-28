use crate::ChatMessage;

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
