use crate::core::types::{ChatMessage, Usage};
use crate::llm::config::LlmConfig;

pub(crate) fn complete(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<::std::sync::mpsc::Sender<crate::core::types::SinkLine>>,
    cancel: &dyn crate::agent::state::CancellationSource,
) -> Result<(ChatMessage, Option<Usage>), Box<dyn std::error::Error>> {
    crate::llm::client::call_responses(config, messages, with_tools, sink, cancel)
}
