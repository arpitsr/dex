use serde_json::json;
use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::agent::state::CancellationSource;
use crate::core::console::with_console;
use crate::core::types::{ChatMessage, ChatRequest, Provider, SinkLine, StreamOptions};
use crate::llm::auth::load_codex_credentials;
use crate::llm::config::LlmConfig;
use crate::llm::protocol::{responses_input, responses_tools, tools_schema};
use crate::llm::stream::{read_responses_stream, read_stream, Turn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModelCapabilities {
    pub streaming: bool,
    pub tools: bool,
    pub responses_api: bool,
}

pub(crate) fn discover_capabilities(config: &LlmConfig) -> ModelCapabilities {
    // Static per-provider table — no runtime probing.
    let (streaming, tools) = match config.provider {
        crate::core::types::Provider::OpenCode => (true, true),
        crate::core::types::Provider::OpenAiCodex => (true, true),
    };
    ModelCapabilities {
        streaming,
        tools,
        responses_api: matches!(config.api, crate::core::types::ApiProtocol::Responses),
    }
}

pub(crate) trait ModelClient {
    fn complete(
        &self,
        messages: &[ChatMessage],
        with_tools: bool,
        sink: Option<mpsc::Sender<SinkLine>>,
        cancel: &dyn CancellationSource,
    ) -> Result<Turn, Box<dyn std::error::Error>>;
}

impl ModelClient for LlmConfig {
    fn complete(
        &self,
        messages: &[ChatMessage],
        with_tools: bool,
        sink: Option<mpsc::Sender<SinkLine>>,
        cancel: &dyn CancellationSource,
    ) -> Result<Turn, Box<dyn std::error::Error>> {
        call_llm(self, messages, with_tools, sink, cancel)
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
    let path = base.join("dex/provider.jsonl");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let record =
            json!({"timestamp": chrono::Utc::now().to_rfc3339(), "event": event, "detail": detail});
        // Single write syscall so concurrent turns cannot interleave records.
        let mut line = record.to_string();
        line.push('\n');
        let _ = file.write_all(line.as_bytes());
    }
}
/// Send a provider request with shared retry/backoff, 401 credential refresh
/// (OpenAI Codex), and provider logging. The protocol-specific request body
/// and post-success reader are supplied by the caller.
fn post_with_retry(
    config: &LlmConfig,
    url: &str,
    body: &impl serde::Serialize,
    sink: Option<&mpsc::Sender<SinkLine>>,
) -> Result<reqwest::blocking::Response, Box<dyn std::error::Error>> {
    const MAX_RETRIES: u32 = 3;
    let mut active_config = config.clone();
    for attempt in 0..=MAX_RETRIES {
        let request = config.client.post(url);
        let resp = match authenticated_request(request, &active_config)
            .json(body)
            .send()
        {
            Ok(resp) => resp,
            Err(e) if attempt < MAX_RETRIES => {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                with_console(sink.is_some(), || {
                    eprintln!("[llm] request failed: {}; retrying in {:?}", e, delay)
                });
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
            let body_text = resp.text()?;
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
                with_console(sink.is_some(), || {
                    eprintln!("[llm] API error {}: retrying in {:?}", status, delay)
                });
                thread::sleep(delay);
                continue;
            }
            provider_log("api_error", &format!("{}: {}", status, body_text));
            // Opaque 5xx / missing route from `/responses` usually means the
            // model only speaks chat-completions (proven for e.g.
            // glm-5.3-flash on zen/go) — point at the per-model override
            // instead of a bare body. Auth and rate-limit failures say
            // nothing about the protocol, and a pinned `api:` means the user
            // already decided.
            if url.ends_with("/responses")
                && !crate::llm::config::api_pinned()
                && (status == reqwest::StatusCode::NOT_FOUND || status.is_server_error())
            {
                return Err(format!(
                    "API error: {} (hint: {} may speak openai-completions; set DEX_MODEL_APIS={}=openai-completions)",
                    body_text, config.model, config.model
                )
                .into());
            }
            return Err(format!("API error: {}", body_text).into());
        }

        return Ok(resp);
    }

    unreachable!()
}

pub(crate) fn call_chat_completions(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &dyn CancellationSource,
) -> Result<Turn, Box<dyn std::error::Error>> {
    let req = ChatRequest {
        model: &config.model,
        messages,
        tools: if with_tools {
            tools_schema()
        } else {
            Vec::new()
        },
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        reasoning_effort: &config.thinking_effort,
    };
    let resp = post_with_retry(
        config,
        &format!("{}/chat/completions", config.base_url),
        &req,
        sink.as_ref(),
    )?;
    read_stream(resp, sink, cancel)
}

pub(crate) fn call_responses(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &dyn CancellationSource,
) -> Result<Turn, Box<dyn std::error::Error>> {
    let (instructions, input) = responses_input(messages);
    let mut body = json!({
        "model": config.model,
        "input": input,
        "stream": true,
        "store": false,
        // State is never stored server-side, so ask for the encrypted
        // reasoning blobs — without them reasoning can't be replayed and
        // the model re-reasons from scratch on every tool call.
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(instructions) = instructions {
        body["instructions"] = json!(instructions);
    }
    if with_tools {
        body["tools"] = json!(responses_tools());
    }
    if let Some(effort) = &config.thinking_effort {
        body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
    }
    let resp = post_with_retry(
        config,
        &format!("{}/responses", config.base_url),
        &body,
        sink.as_ref(),
    )?;
    // Mid-stream failures (output already flowed) are marked by the stream
    // driver itself, so the protocol-fallback gate won't re-run a turn whose
    // partial text is already on the transcript — while a drop before the
    // first delta stays retryable.
    read_responses_stream(resp, sink, cancel)
}

pub(crate) fn call_llm(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &dyn CancellationSource,
) -> Result<Turn, Box<dyn std::error::Error>> {
    let capabilities = discover_capabilities(config);
    if with_tools && !capabilities.tools {
        return Err("configured model does not support tools".into());
    }
    crate::llm::streaming::complete(config, messages, with_tools, sink, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Usage;

    struct MockModel;

    impl ModelClient for MockModel {
        fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &dyn CancellationSource,
        ) -> Result<Turn, Box<dyn std::error::Error>> {
            Ok(Turn {
                message: ChatMessage {
                    role: "assistant".into(),
                    content: Some("mock response".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                    ..Default::default()
                },
                usage: Some(Usage {
                    prompt_tokens: 3,
                    completion_tokens: 0,
                    cached_tokens: None,
                }),
                stop_reason: None,
            })
        }
    }

    #[test]
    fn model_boundary_supports_deterministic_mock() {
        let turn = MockModel
            .complete(&[], false, None, &crate::agent::state::GlobalCancellation)
            .unwrap();
        assert_eq!(turn.message.content.as_deref(), Some("mock response"));
        assert_eq!(
            turn.usage,
            Some(Usage {
                prompt_tokens: 3,
                completion_tokens: 0,
                cached_tokens: None
            })
        );
    }
}
