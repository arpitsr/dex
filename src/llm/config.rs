use serde::Deserialize;
use std::env;
use std::fs;
use std::io;
use std::time::Duration;

use crate::core::types::*;
use crate::llm::auth::*;

#[derive(Default, Deserialize)]
pub(crate) struct FileConfig {
    pub(crate) provider: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) models: Option<Vec<String>>,
    pub(crate) api: Option<String>,
    pub(crate) thinking_effort: Option<String>,
    pub(crate) context_window: Option<u64>,
    pub(crate) permission: Option<String>,
    pub(crate) max_tool_iterations: Option<usize>,
    pub(crate) max_prompt_tokens: Option<u64>,
    pub(crate) max_tool_output_bytes: Option<usize>,
    pub(crate) max_turn_seconds: Option<u64>,
    pub(crate) http_connect_timeout_secs: Option<u64>,
    pub(crate) http_request_timeout_secs: Option<u64>,
}

pub(crate) fn load_file_config() -> Result<FileConfig, Box<dyn std::error::Error>> {
    let Some(path) = crate::config::path() else {
        return Ok(FileConfig::default());
    };
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(FileConfig::default()),
        Err(e) => return Err(format!("could not read {}: {}", path.display(), e).into()),
    };
    serde_json::from_str(&contents)
        .map_err(|e| format!("invalid config file {}: {}", path.display(), e).into())
}

pub(crate) fn permission_from_env_or_file(
    file: &FileConfig,
) -> Result<PermissionMode, Box<dyn std::error::Error>> {
    let value = env::var("OYE_PERMISSION")
        .ok()
        .or_else(|| file.permission.clone())
        .unwrap_or_else(|| "ask-writes".to_string());
    PermissionMode::parse(&value).map_err(Into::into)
}

#[derive(Clone)]
pub(crate) struct LlmConfig {
    pub(crate) provider: Provider,
    pub(crate) api_key: String,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) available_models: Vec<String>,
    pub(crate) api: ApiProtocol,
    pub(crate) account_id: Option<String>,
    pub(crate) thinking_effort: Option<String>,
    /// Model context window size in tokens (used for compaction + status bar).
    pub(crate) context_window: u64,
    pub(crate) permission: PermissionMode,
    pub(crate) max_tool_iterations: usize,
    pub(crate) max_prompt_tokens: u64,
    pub(crate) max_turn_seconds: u64,
    pub(crate) client: reqwest::blocking::Client,
}

impl LlmConfig {
    pub(crate) fn from_env(
        base_url_override: Option<String>,
        model_override: Option<String>,
        permission_override: Option<PermissionMode>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let file = load_file_config()?;
        crate::tools::set_output_limit(
            env::var("OYE_TOOL_OUTPUT_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(file.max_tool_output_bytes)
                .unwrap_or(1_048_576),
        );
        let permission = match permission_override {
            Some(mode) => mode,
            None => permission_from_env_or_file(&file)?,
        };
        let provider_name = env::var("OYE_PROVIDER")
            .ok()
            .or(file.provider)
            .unwrap_or_else(|| "opencode".to_string());
        let provider = Provider::parse(&provider_name)?;
        let model = model_override
            .or_else(|| env::var("OPENAI_MODEL").ok())
            .or(file.model)
            .unwrap_or_else(|| match provider {
                Provider::OpenCode => "gpt-5.6-luna".to_string(),
                Provider::OpenAiCodex => "gpt-5.6-luna".to_string(),
            });
        let mut available_models = env::var("OYE_MODELS")
            .ok()
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .or(file.models.clone())
            .unwrap_or_default();
        if !available_models.iter().any(|candidate| candidate == &model) {
            available_models.insert(0, model.clone());
        }
        let env_base_url = env::var("OPENAI_BASE_URL").ok().filter(|v| !v.is_empty());
        let base_url = match provider {
            Provider::OpenCode => base_url_override
                .or(env_base_url)
                .or(file.base_url)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            Provider::OpenAiCodex => base_url_override
                .or(env_base_url)
                .unwrap_or_else(|| "https://chatgpt.com/backend-api/codex".to_string()),
        };
        let api_name = env::var("OPENAI_API")
            .ok()
            .or(file.api)
            .unwrap_or_else(|| match provider {
                Provider::OpenCode => "openai-responses".to_string(),
                Provider::OpenAiCodex => "openai-responses".to_string(),
            });
        let api = match api_name.as_str() {
            "responses" | "openai-responses" => ApiProtocol::Responses,
            "chat" | "chat-completions" | "openai-completions" => ApiProtocol::ChatCompletions,
            other => {
                return Err(format!(
                    "unsupported api '{}'; use openai-completions or openai-responses",
                    other
                )
                .into())
            }
        };
        let (api_key, account_id) = match provider {
            Provider::OpenCode => (
                env::var("OPENAI_API_KEY")
                    .ok()
                    .or(file.api_key)
                    .ok_or("OPENAI_API_KEY not set and no api_key in config file")?,
                None,
            ),
            Provider::OpenAiCodex => load_codex_credentials()?,
        };
        Ok(Self {
            provider,
            api_key,
            base_url,
            model,
            available_models,
            api,
            account_id,
            thinking_effort: file.thinking_effort,
            // Context window in tokens; configurable via file (`context_window`)
            // or OYE_CONTEXT_WINDOW env, with a conservative default.
            context_window: env::var("OYE_CONTEXT_WINDOW")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(file.context_window)
                .unwrap_or(128_000),
            permission,
            max_tool_iterations: env::var("OYE_MAX_TOOL_ITERATIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(file.max_tool_iterations)
                .unwrap_or(60),
            max_prompt_tokens: env::var("OYE_MAX_PROMPT_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(file.max_prompt_tokens)
                .unwrap_or(128_000),
            max_turn_seconds: env::var("OYE_MAX_TURN_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(file.max_turn_seconds)
                .unwrap_or(3600),
            client: reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(
                    env::var("OYE_HTTP_CONNECT_TIMEOUT_SECS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .or(file.http_connect_timeout_secs)
                        .unwrap_or(10),
                ))
                // Note: for the blocking client this deadline applies to the
                // connect and to each individual body read (not to the whole
                // streamed response), so long-lived SSE streams are safe.
                .timeout(Duration::from_secs(
                    env::var("OYE_HTTP_REQUEST_TIMEOUT_SECS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .or(file.http_request_timeout_secs)
                        .unwrap_or(300),
                ))
                .build()?,
        })
    }

    pub(crate) fn switch_provider(
        &mut self,
        provider: Provider,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let file = load_file_config()?;
        let env_base_url = env::var("OPENAI_BASE_URL").ok().filter(|v| !v.is_empty());
        let (api_key, account_id) = match provider {
            Provider::OpenCode => (
                env::var("OPENAI_API_KEY")
                    .ok()
                    .or(file.api_key)
                    .ok_or("OPENAI_API_KEY not set and no api_key in config file")?,
                None,
            ),
            Provider::OpenAiCodex => load_codex_credentials()?,
        };
        self.provider = provider;
        self.api_key = api_key;
        self.account_id = account_id;
        self.base_url = match provider {
            Provider::OpenCode => env_base_url
                .or(file.base_url)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            Provider::OpenAiCodex => "https://chatgpt.com/backend-api/codex".to_string(),
        };
        self.api = match env::var("OPENAI_API").ok().or(file.api).as_deref() {
            Some("chat" | "chat-completions" | "openai-completions") => {
                ApiProtocol::ChatCompletions
            }
            _ => ApiProtocol::Responses,
        };
        Ok(())
    }
}
