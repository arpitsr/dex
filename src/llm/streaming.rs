//! Shared streaming boundary. The concrete SSE readers remain compatible with
//! both provider protocols and are called through these typed entry points.

use crate::{ChatMessage, LlmConfig};

pub(crate) fn complete(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<::std::sync::mpsc::Sender<crate::SinkLine>>,
) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    match config.api {
        crate::ApiProtocol::ChatCompletions => {
            crate::llm::chat_completions::complete(config, messages, with_tools, sink)
        }
        crate::ApiProtocol::Responses => {
            crate::llm::responses::complete(config, messages, with_tools, sink)
        }
    }
}
