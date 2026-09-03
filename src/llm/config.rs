use serde::Deserialize;
use std::collections::BTreeMap;
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
    /// Named OpenAI-compatible endpoints for /model autocomplete; selecting
    /// `name/model-id` routes requests to that endpoint's base_url.
    pub(crate) endpoints: Option<BTreeMap<String, String>>,
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
    pub(crate) verify_command: Option<String>,
}

pub(crate) fn provider_default_context_window(provider: Provider) -> u64 {
    match provider {
        Provider::OpenCode => 128_000,
        Provider::OpenAiCodex => 128_000,
    }
}

/// Auto-detect a verification command (P9) when none is configured: a
/// recognized manifest in the workspace selects the standard test command.
/// Called at the daemon/one-shot boundary (NOT inside the shared agent loop),
/// so a test harness with a real workspace CWD cannot accidentally re-run the
/// project's own test suite mid-turn.
pub(crate) fn detect_verify_command() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    if cwd.join("Cargo.toml").exists() {
        return Some("cargo test".into());
    }
    if cwd.join("go.mod").exists() {
        return Some("go test ./...".into());
    }
    if cwd.join("package.json").exists() {
        return Some("npm test".into());
    }
    None
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
    // YAML (a superset of JSON), so legacy config.json files keep working.
    serde_yaml::from_str(&contents)
        .map_err(|e| format!("invalid config file {}: {}", path.display(), e).into())
}

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// Fetch the provider's model list from its OpenAI-compatible `/models`
/// endpoint. Best-effort: any failure (unknown endpoint, no network, bad
/// auth) returns an empty list and the caller falls back to the configured
/// model alone.
fn fetch_provider_models(
    client: &reqwest::blocking::Client,
    provider: Provider,
    base_url: &str,
    api_key: &str,
) -> Vec<String> {
    // ponytail: only OpenCode-style endpoints expose /models; codex backend-api doesn't
    if provider != Provider::OpenCode {
        return Vec::new();
    }
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let parsed: Result<ModelsResponse, _> = client
        .get(url)
        .bearer_auth(api_key)
        .timeout(Duration::from_secs(5))
        .send()
        .and_then(|resp| resp.error_for_status())
        .and_then(|resp| resp.json());
    match parsed {
        Ok(response) => response.data.into_iter().map(|m| m.id).collect(),
        Err(_) => Vec::new(),
    }
}

pub(crate) fn permission_from_env_or_file(
    file: &FileConfig,
) -> Result<PermissionMode, Box<dyn std::error::Error>> {
    let value = env::var("DEX_PERMISSION")
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
    pub(crate) endpoints: BTreeMap<String, String>,
    pub(crate) api: ApiProtocol,
    pub(crate) account_id: Option<String>,
    pub(crate) thinking_effort: Option<String>,
    /// Model context window size in tokens (used for compaction + status bar).
    pub(crate) context_window: u64,
    pub(crate) permission: PermissionMode,
    pub(crate) max_tool_iterations: usize,
    pub(crate) max_prompt_tokens: u64,
    pub(crate) max_turn_seconds: u64,
    pub(crate) verify_command: Option<String>,
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
            env::var("DEX_TOOL_OUTPUT_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(file.max_tool_output_bytes)
                .unwrap_or(1_048_576),
        );
        let permission = match permission_override {
            Some(mode) => mode,
            None => permission_from_env_or_file(&file)?,
        };
        let provider_name = env::var("DEX_PROVIDER")
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
        let mut available_models = env::var("DEX_MODELS")
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
        let env_base_url = env::var("OPENAI_BASE_URL").ok().filter(|v| !v.is_empty());
        let explicit_base_url = base_url_override.is_some();
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
        // Resolve context window once, then derive max_prompt_tokens from it
        // so the two limits can never disagree (old default: both 128k).
        let context_window = env::var("DEX_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(file.context_window)
            .unwrap_or_else(|| provider_default_context_window(provider));
        let derived_max_prompt = context_window
            .saturating_sub(16_000)
            .min(context_window * 3 / 4);
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(
                env::var("DEX_HTTP_CONNECT_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .or(file.http_connect_timeout_secs)
                    .unwrap_or(10),
            ))
            // Note: for the blocking client this deadline applies to the
            // connect and to each individual body read (not to the whole
            // streamed response), so long-lived SSE streams are safe.
            .timeout(Duration::from_secs(
                env::var("DEX_HTTP_REQUEST_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .or(file.http_request_timeout_secs)
                    .unwrap_or(300),
            ))
            .build()?;
        // No static model list (DEX_MODELS / `models:`) and the resolved
        // base_url isn't itself a named endpoint -> live /models fetch so
        // /model autocomplete lists everything.
        let endpoints: BTreeMap<String, String> = match &file.endpoints {
            Some(map) => map.clone(),
            // Already pointed at opencode.ai: expose its zen/go gateways by
            // default so autocomplete shows which endpoint each model is on.
            None if provider == Provider::OpenCode && base_url.starts_with("https://opencode.ai/") => [
                ("zen".to_string(), "https://opencode.ai/zen/v1".to_string()),
                ("go".to_string(), "https://opencode.ai/zen/go/v1".to_string()),
            ]
            .into_iter()
            .collect(),
            None => BTreeMap::new(),
        };
        if available_models.is_empty() && !endpoints.values().any(|url| url == &base_url) {
            available_models = fetch_provider_models(&client, provider, &base_url, &api_key);
        }
        // Each named endpoint contributes `<name>/<model-id>` entries; picking
        // one routes requests to that endpoint (apply_model).
        for (name, url) in &endpoints {
            for id in fetch_provider_models(&client, provider, url, &api_key) {
                available_models.push(format!("{name}/{id}"));
            }
        }
        if !available_models.iter().any(|candidate| candidate == &model) {
            available_models.insert(0, model.clone());
        }
        let mut this = Self {
            provider,
            api_key,
            base_url,
            model: model.clone(),
            available_models,
            api,
            account_id,
            thinking_effort: file.thinking_effort,
            context_window,
            verify_command: env::var("DEX_VERIFY").ok().or(file.verify_command.clone()),
            permission,
            max_tool_iterations: env::var("DEX_MAX_TOOL_ITERATIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(file.max_tool_iterations)
                .unwrap_or(60),
            max_prompt_tokens: env::var("DEX_MAX_PROMPT_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(file.max_prompt_tokens)
                .unwrap_or(derived_max_prompt),
            max_turn_seconds: env::var("DEX_MAX_TURN_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(file.max_turn_seconds)
                .unwrap_or(3600),
            client,
            endpoints,
        };
        // A prefixed model override (--model go/foo or the daemon's
        // per-request model) routes to its endpoint; bare ids keep the
        // resolved base_url. An explicit --base-url wins over routing.
        if !explicit_base_url {
            this.apply_model(&model);
        }
        Ok(this)
    }

    /// Apply a `/model` selection. `name/model-id` routes to the named
    /// endpoint's base_url when `name` matches a configured endpoint (the
    /// prefix is stripped — the provider only knows the bare id); anything
    /// else is a plain model id on the current base_url. Returns the endpoint
    /// name when a route was taken.
    pub(crate) fn apply_model(&mut self, selection: &str) -> Option<String> {
        if let Some((name, rest)) = selection.split_once('/') {
            if let Some(url) = self.endpoints.get(name) {
                self.base_url = url.clone();
                self.model = rest.to_string();
                return Some(name.to_string());
            }
        }
        self.model = selection.to_string();
        None
    }

    /// Trigger compaction when prompt exceeds this many tokens.
    /// Half the window leaves room for completion + tool overhead.
    pub(crate) fn compaction_threshold(&self) -> u64 {
        self.context_window / 2
    }

    /// Tokens reserved for the model's reply.
    pub(crate) fn reserve_tokens(&self) -> u64 {
        8192
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_model_routes_prefixed_selection_to_endpoint() {
        let mut cfg = LlmConfig {
            provider: Provider::OpenCode,
            api_key: "k".into(),
            base_url: "https://opencode.ai/zen/v1".into(),
            model: "gpt-5.6-luna".into(),
            available_models: Vec::new(),
            endpoints: [
                ("zen".to_string(), "https://opencode.ai/zen/v1".to_string()),
                ("go".to_string(), "https://opencode.ai/zen/go/v1".to_string()),
            ]
            .into_iter()
            .collect(),
            api: ApiProtocol::Responses,
            account_id: None,
            thinking_effort: None,
            context_window: 128_000,
            permission: PermissionMode::AskWrites,
            max_tool_iterations: 60,
            max_prompt_tokens: 96_000,
            max_turn_seconds: 3600,
            verify_command: None,
            client: reqwest::blocking::Client::new(),
        };
        // Bare id keeps the current base_url.
        assert_eq!(cfg.apply_model("gpt-5.6-luna"), None);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        // Prefixed id routes to the endpoint and strips the prefix.
        assert_eq!(cfg.apply_model("go/kimi-k2").as_deref(), Some("go"));
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.model, "kimi-k2");
        // Provider-native slashes in model ids survive under an endpoint.
        assert_eq!(cfg.apply_model("go/moonshotai/kimi-k2").as_deref(), Some("go"));
        assert_eq!(cfg.model, "moonshotai/kimi-k2");
        // Unknown prefix is a plain model id.
        assert_eq!(cfg.apply_model("unknown/m"), None);
        assert_eq!(cfg.model, "unknown/m");
    }

    #[test]
    fn provider_parse_accepts_aliases() {
        assert_eq!(Provider::parse("opencode").unwrap(), Provider::OpenCode);
        assert_eq!(Provider::parse("codex").unwrap(), Provider::OpenAiCodex);
        assert_eq!(Provider::parse("openai-codex").unwrap(), Provider::OpenAiCodex);
        assert!(Provider::parse("unknown").is_err());
    }

    #[test]
    fn permission_parse_and_ordering() {
        assert_eq!(PermissionMode::parse("read-only").unwrap(), PermissionMode::ReadOnly);
        assert_eq!(PermissionMode::parse("readonly").unwrap(), PermissionMode::ReadOnly);
        assert_eq!(PermissionMode::parse("ask_writes").unwrap(), PermissionMode::AskWrites);
        assert_eq!(PermissionMode::parse("trusted").unwrap(), PermissionMode::Trusted);
        assert!(PermissionMode::parse("nope").is_err());
        assert!(PermissionMode::ReadOnly.permissiveness() < PermissionMode::Trusted.permissiveness());
    }

    #[test]
    fn detect_verify_command_selects_by_manifest() {
        // Isolated temp dir without manifests -> None
        let prev = std::env::current_dir().unwrap();
        let tmp = std::env::temp_dir().join(format!("dex-verify-none-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        std::env::set_current_dir(&tmp).unwrap();
        assert_eq!(detect_verify_command(), None);
        // Cargo.toml -> cargo test
        std::fs::write(tmp.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detect_verify_command().as_deref(), Some("cargo test"));
        let _ = std::fs::remove_file(tmp.join("Cargo.toml"));
        std::fs::write(tmp.join("go.mod"), "module x").unwrap();
        assert_eq!(detect_verify_command().as_deref(), Some("go test ./..."));
        let _ = std::fs::remove_file(tmp.join("go.mod"));
        std::fs::write(tmp.join("package.json"), "{}").unwrap();
        assert_eq!(detect_verify_command().as_deref(), Some("npm test"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_current_dir(prev).unwrap();
    }

    #[test]
    fn models_response_parses_openai_shape() {
        let json = r#"{"object":"list","data":[{"id":"a"},{"id":"b"}]}"#;
        let parsed: ModelsResponse = serde_json::from_str(json).unwrap();
        let ids: Vec<String> = parsed.data.into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn file_config_parses_yaml() {
        let path = "/tmp/dex-config-yaml-test.yaml";
        std::fs::write(
            path,
            "provider: opencode\nmodel: muse-spark-1.2\napi: openai-completions\nmodels: [a, b]\n",
        )
        .unwrap();
        let prev = std::env::var_os("DEX_CONFIG");
        std::env::set_var("DEX_CONFIG", path);
        let cfg = load_file_config().unwrap();
        assert_eq!(cfg.provider.as_deref(), Some("opencode"));
        assert_eq!(cfg.model.as_deref(), Some("muse-spark-1.2"));
        assert_eq!(cfg.api.as_deref(), Some("openai-completions"));
        assert_eq!(cfg.models.as_deref(), Some(&["a".to_string(), "b".to_string()][..]));
        match prev {
            Some(v) => std::env::set_var("DEX_CONFIG", v),
            None => std::env::remove_var("DEX_CONFIG"),
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_config_defaults_when_no_file() {
        let prev = std::env::var_os("DEX_CONFIG");
        std::env::set_var("DEX_CONFIG", "/tmp/dex-no-such-config-12345.json");
        let cfg = load_file_config().unwrap();
        assert!(cfg.provider.is_none());
        match prev {
            Some(v) => std::env::set_var("DEX_CONFIG", v),
            None => std::env::remove_var("DEX_CONFIG"),
        }
    }
}
