use crate::{ChatMessage, LlmConfig};

pub(crate) fn complete(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<::std::sync::mpsc::Sender<crate::SinkLine>>,
) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    crate::call_chat_completions(config, messages, with_tools, sink)
}
