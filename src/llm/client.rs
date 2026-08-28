//! Testable model boundary. Provider wire formats are kept behind this trait.

use crate::{call_llm, ChatMessage, LlmConfig};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModelCapabilities {
    pub streaming: bool,
    pub tools: bool,
    pub responses_api: bool,
}

pub(crate) fn discover_capabilities(config: &LlmConfig) -> ModelCapabilities {
    ModelCapabilities {
        streaming: true,
        tools: true,
        responses_api: matches!(config.api, crate::ApiProtocol::Responses),
    }
}

pub(crate) trait ModelClient {
    fn complete(
        &self,
        messages: &[ChatMessage],
        with_tools: bool,
    ) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>>;
}

impl ModelClient for LlmConfig {
    fn complete(
        &self,
        messages: &[ChatMessage],
        with_tools: bool,
    ) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
        call_llm(self, messages, with_tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct MockModel;
    impl ModelClient for MockModel {
        fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
        ) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
            Ok((
                ChatMessage {
                    role: "assistant".into(),
                    content: Some("mock response".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
                Some(3),
            ))
        }
    }
    #[test]
    fn model_boundary_supports_deterministic_mock() {
        let (message, usage) = MockModel.complete(&[], false).unwrap();
        assert_eq!(message.content.as_deref(), Some("mock response"));
        assert_eq!(usage, Some(3));
    }
}
