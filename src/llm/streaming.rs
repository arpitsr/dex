//! Shared streaming boundary. The concrete SSE readers remain compatible with
//! both provider protocols and are called through these typed entry points.

use crate::core::types::ChatMessage;
use crate::llm::config::LlmConfig;

pub(crate) fn complete(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<::std::sync::mpsc::Sender<crate::core::types::SinkLine>>,
    cancel: &dyn crate::agent::state::CancellationSource,
) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    match config.api {
        crate::core::types::ApiProtocol::ChatCompletions => {
            crate::llm::chat_completions::complete(config, messages, with_tools, sink, cancel)
        }
        crate::core::types::ApiProtocol::Responses => {
            crate::llm::responses::complete(config, messages, with_tools, sink, cancel)
        }
    }
}
