use serde_json::json;
use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crate::core::console::*;
use crate::core::types::*;
use crate::llm::auth::*;
use crate::llm::config::*;
use crate::llm::protocol::*;
use crate::llm::stream::*;

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

pub(crate) fn authenticated_request(
    request: reqwest::blocking::RequestBuilder,
    config: &LlmConfig,
) -> reqwest::blocking::RequestBuilder {
    let request = request.bearer_auth(&config.api_key);
    if config.provider == Provider::OpenAiCodex {
        let request = request.header("originator", "codex_cli_rs");
        if let Some(account_id) = &config.account_id {
            return request.header("ChatGPT-Account-ID", account_id);
        }
        return request;
    }
    request
}

pub(crate) fn retryable_status(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
}

pub(crate) fn provider_log(event: &str, detail: &str) {
    let Some(base) = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    else {
        return;
    };
    let path = base.join("ak/provider.jsonl");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let record =
            json!({"timestamp": chrono::Utc::now().to_rfc3339(), "event": event, "detail": detail});
        let _ = writeln!(file, "{}", record);
    }
}

pub(crate) fn call_chat_completions(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    const MAX_RETRIES: u32 = 3;
    let req = ChatRequest {
        model: config.model.clone(),
        messages: messages.to_vec(),
        tools: if with_tools {
            tools_schema()
        } else {
            Vec::new()
        },
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        reasoning_effort: config.thinking_effort.clone(),
    };

    let mut active_config = config.clone();
    for attempt in 0..=MAX_RETRIES {
        let request = config
            .client
            .post(format!("{}/chat/completions", config.base_url));
        let resp = match authenticated_request(request, &active_config)
            .json(&req)
            .send()
        {
            Ok(resp) => resp,
            Err(e) if attempt < MAX_RETRIES => {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                with_console(|| eprintln!("[llm] request failed: {}; retrying in {:?}", e, delay));
                thread::sleep(delay);
                continue;
            }
            Err(e) => {
                provider_log("request_failed", &e.to_string());
                return Err(e.into());
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text()?;
            if status == reqwest::StatusCode::UNAUTHORIZED
                && active_config.provider == Provider::OpenAiCodex
                && attempt < MAX_RETRIES
            {
                if let Ok((token, account)) = load_codex_credentials() {
                    active_config.api_key = token;
                    active_config.account_id = account;
                    continue;
                }
            }
            let retryable = retryable_status(status);
            if retryable && attempt < MAX_RETRIES {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                with_console(|| eprintln!("[llm] API error {}: retrying in {:?}", status, delay));
                thread::sleep(delay);
                continue;
            }
            provider_log("api_error", &format!("{}: {}", status, body));
            return Err(format!("API error: {}", body).into());
        }

        return read_stream(resp);
    }

    unreachable!()
}

pub(crate) fn call_responses(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    let (instructions, input) = responses_input(messages);
    let mut body = json!({
        "model": config.model,
        "input": input,
        "stream": true,
        "store": false,
    });
    if let Some(instructions) = instructions {
        body["instructions"] = json!(instructions);
    }
    if with_tools {
        body["tools"] = json!(responses_tools());
    }
    if let Some(effort) = &config.thinking_effort {
        body["reasoning"] = json!({ "effort": effort });
    }
    const MAX_RETRIES: u32 = 3;
    let mut active_config = config.clone();
    for attempt in 0..=MAX_RETRIES {
        let request = config.client.post(format!("{}/responses", config.base_url));
        let resp = match authenticated_request(request, &active_config)
            .json(&body)
            .send()
        {
            Ok(resp) => resp,
            Err(e) if attempt < MAX_RETRIES => {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                with_console(|| eprintln!("[llm] request failed: {}; retrying in {:?}", e, delay));
                thread::sleep(delay);
                continue;
            }
            Err(e) => {
                provider_log("request_failed", &e.to_string());
                return Err(e.into());
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let error_body = resp.text()?;
            if status == reqwest::StatusCode::UNAUTHORIZED
                && active_config.provider == Provider::OpenAiCodex
                && attempt < MAX_RETRIES
            {
                if let Ok((token, account)) = load_codex_credentials() {
                    active_config.api_key = token;
                    active_config.account_id = account;
                    continue;
                }
            }
            let retryable = retryable_status(status);
            if retryable && attempt < MAX_RETRIES {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                with_console(|| eprintln!("[llm] API error {}: retrying in {:?}", status, delay));
                thread::sleep(delay);
                continue;
            }
            provider_log("api_error", &format!("{}: {}", status, error_body));
            return Err(format!("API error: {}", error_body).into());
        }
        return read_responses_stream(resp);
    }
    unreachable!()
}

pub(crate) fn call_llm(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    let capabilities = crate::llm::discover_capabilities(config);
    if with_tools && !capabilities.tools {
        return Err("configured model does not support tools".into());
    }
    crate::llm::streaming::complete(config, messages, with_tools)
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
