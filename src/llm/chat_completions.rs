use crate::core::types::ChatMessage;
use crate::llm::config::LlmConfig;
use crate::llm::stream::Turn;

pub(crate) fn complete(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<::std::sync::mpsc::Sender<crate::core::types::SinkLine>>,
    cancel: &dyn crate::agent::state::CancellationSource,
) -> Result<Turn, Box<dyn std::error::Error>> {
    crate::llm::client::call_chat_completions(config, messages, with_tools, sink, cancel)
}
