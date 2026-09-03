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
    pub(crate) reserve_tokens: Option<u64>,
    pub(crate) keep_recent_tokens: Option<u64>,
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

/// Per-model context window, matching pi's catalog (models-store.json).
/// Kept static to avoid network probe; DEX_CONTEXT_WINDOW / file override wins.
/// Pi catalog: gpt-5* 400k (5.6* 1050k), claude 200k/1M, muse 1048576, deepseek/glm 1M.
/// Update when pi catalog changes — grep models-store.json for contextWindow.
pub(crate) fn model_context_window(model: &str, provider: Provider) -> u64 {
    let m = model.to_ascii_lowercase();
    // ponytail: static table, pi's models-store.json is the source of truth
    if m.contains("muse-spark") {
        return 1_048_576;
    }
    if m.contains("gpt-5.6") {
        return 1_050_000;
    }
    if m.contains("gpt-5.5") {
        return 1_050_000;
    }
    if m.contains("gpt-5.4-pro") {
        return 1_050_000;
    }
    if m.contains("gpt-5.4") {
        return 272_000;
    }
    if m.contains("gpt-5") {
        return 400_000;
    }
    if m.contains("claude-fable") {
        return 1_000_000;
    }
    if m.contains("claude") {
        return 200_000;
    }
    if m.contains("deepseek") {
        return 1_000_000;
    }
    if m.contains("glm-5.3") || m.contains("glm-5.2") {
        return 1_000_000;
    }
    if m.contains("glm-5") {
        return 202_752;
    }
    if m.contains("kimi") || m.contains("qwen") {
        return 128_000;
    }
    provider_default_context_window(provider)
}

pub(crate) fn dex_models_cache_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(std::path::PathBuf::from(dir).join("dex/models.json"));
    }
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache/dex/models.json"))
}

fn dex_catalog_cache_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(std::path::PathBuf::from(dir).join("dex/models.dev.json"));
    }
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache/dex/models.dev.json"))
}

fn load_dex_catalog() -> Option<serde_json::Value> {
    let path = dex_catalog_cache_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn catalog_context_window(model: &str, catalog: &serde_json::Value) -> Option<u64> {
    let needle = model.to_ascii_lowercase();
    // catalog is api.json (providers) or catalog.json (models+providers) — try both shapes
    if let Some(providers) = catalog.as_object() {
        // api.json shape: { "opencode": { models: { "id": { limit:{context} } } } }
        for (_prov, entry) in providers {
            if let Some(models) = entry.get("models").and_then(|m| m.as_object()) {
                if let Some(m) = models.get(needle.as_str()).or_else(|| {
                    // fallback case-insensitive scan
                    models
                        .iter()
                        .find(|(k, _)| k.to_ascii_lowercase() == needle)
                        .map(|(_, v)| v)
                }) {
                    if let Some(ctx) = m
                        .get("limit")
                        .and_then(|l| l.get("context"))
                        .and_then(|c| c.as_u64())
                    {
                        return Some(ctx);
                    }
                }
            }
        }
        // catalog.json shape: { models: { "id": { limit } }, providers: { } }
        if let Some(models) = catalog.get("models").and_then(|m| m.as_object()) {
            if let Some(m) = models.get(needle.as_str()) {
                if let Some(ctx) = m
                    .get("limit")
                    .and_then(|l| l.get("context"))
                    .and_then(|c| c.as_u64())
                {
                    return Some(ctx);
                }
            }
            for (k, m) in models {
                if k.to_ascii_lowercase() == needle {
                    if let Some(ctx) = m
                        .get("limit")
                        .and_then(|l| l.get("context"))
                        .and_then(|c| c.as_u64())
                    {
                        return Some(ctx);
                    }
                }
            }
        }
    }
    None
}

fn load_dex_models_cache() -> Option<Vec<String>> {
    // dex cache is now models.dev catalog — extract opencode ids from it
    if let Some(catalog) = load_dex_catalog() {
        if let Some(providers) = catalog.as_object() {
            if let Some(op) = providers.get("opencode") {
                if let Some(models) = op.get("models").and_then(|m| m.as_object()) {
                    let mut ids: Vec<String> = models.keys().cloned().collect();
                    if let Some(go) = providers.get("opencode-go") {
                        if let Some(gmodels) = go.get("models").and_then(|m| m.as_object()) {
                            for id in gmodels.keys() {
                                ids.push(format!("go/{id}"));
                                ids.push(id.clone());
                            }
                        }
                    }
                    if !ids.is_empty() {
                        return Some(ids);
                    }
                }
            }
        }
        if let Some(models) = catalog.get("models").and_then(|m| m.as_object()) {
            let ids: Vec<String> = models.keys().cloned().collect();
            if !ids.is_empty() {
                return Some(ids);
            }
        }
    }
    // fallback: dex cache array
    if let Some(p) = dex_models_cache_path() {
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(arr) = v.as_array() {
                    let ids: Vec<String> = arr
                        .iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect();
                    if !ids.is_empty() {
                        return Some(ids);
                    }
                }
            }
        }
    }
    None
}

/// Refresh dex models cache — like `pi update --models`, now via models.dev.
/// Fetches https://models.dev/api.json (no auth) and caches to
/// XDG_CACHE_HOME/dex/models.dev.json. Next startup uses it for contextWindow
/// and autocomplete without network. Falls back to opencode /models if needed.
pub(crate) fn refresh_models_cache() -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    // Primary: models.dev catalog (provider-agnostic, no auth, has limit.context)
    let mut fetched = false;
    for url in [
        "https://models.dev/api.json",
        "https://models.dev/catalog.json",
    ] {
        if let Ok(resp) = client.get(url).send().and_then(|r| r.error_for_status()) {
            if let Ok(text) = resp.text() {
                if serde_json::from_str::<serde_json::Value>(&text).is_ok() {
                    if let Some(path) = dex_catalog_cache_path() {
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        std::fs::write(&path, &text)?;
                        println!("cached models.dev {} to {}", url, path.display());
                        fetched = true;
                        break;
                    }
                }
            }
        }
    }
    if fetched {
        // also refresh legacy ids cache for fast autocomplete
        if let Some(catalog) = load_dex_catalog() {
            if let Some(ids) = load_dex_models_cache() {
                let _ = ids; // already derived from catalog
            }
            let _ = catalog; // keep file
        }
        return Ok(());
    }
    // Fallback: opencode /models (needs auth) — old behavior
    let file = load_file_config()?;
    let provider_name = std::env::var("DEX_PROVIDER")
        .ok()
        .or(file.provider.clone())
        .unwrap_or_else(|| "opencode".to_string());
    let provider = Provider::parse(&provider_name)?;
    if provider != Provider::OpenCode {
        return Err("models.dev fetch failed and provider is not opencode".into());
    }
    let env_base_url = std::env::var("OPENAI_BASE_URL")
        .ok()
        .filter(|v| !v.is_empty());
    let base_url = env_base_url
        .or(file.base_url.clone())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .or(file.api_key.clone())
        .ok_or("models.dev unavailable and OPENAI_API_KEY not set for fallback")?;
    let endpoints: BTreeMap<String, String> = match &file.endpoints {
        Some(map) => map.clone(),
        None if base_url.starts_with("https://opencode.ai/") => [
            ("zen".to_string(), "https://opencode.ai/zen/v1".to_string()),
            (
                "go".to_string(),
                "https://opencode.ai/zen/go/v1".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        None => BTreeMap::new(),
    };
    let mut all: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in fetch_provider_models(&client, provider, &base_url, &api_key) {
        if seen.insert(id.clone()) {
            all.push(id);
        }
    }
    for (name, url) in &endpoints {
        for id in fetch_provider_models(&client, provider, url, &api_key) {
            let prefixed = format!("{name}/{id}");
            if seen.insert(prefixed.clone()) {
                all.push(prefixed);
            }
            if seen.insert(id.clone()) {
                all.push(id);
            }
        }
    }
    if all.is_empty() {
        return Err("no models fetched — check network".into());
    }
    all.sort();
    all.dedup();
    let path = dex_models_cache_path().ok_or("could not determine cache path")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&all)?)?;
    println!(
        "cached {} models to {} (fallback)",
        all.len(),
        path.display()
    );
    Ok(())
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
    /// Per-model (pi catalog) unless overridden by DEX_CONTEXT_WINDOW / file.
    pub(crate) context_window: u64,
    pub(crate) reserve_tokens: u64,
    pub(crate) keep_recent_tokens: u64,
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
        // Dex: models.dev catalog > static table. DEX_CONTEXT_WINDOW / file override wins. No pi dependency.
        let context_window = env::var("DEX_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(file.context_window)
            .or_else(|| load_dex_catalog().and_then(|c| catalog_context_window(&model, &c)))
            .unwrap_or_else(|| model_context_window(&model, provider));
        let derived_max_prompt = context_window
            .saturating_sub(16_000)
            .min(context_window * 3 / 4);
        // Pi: reserve 16384, keep 20000 tokens recent (not 12 messages)
        let reserve_tokens = env::var("DEX_RESERVE_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(file.reserve_tokens)
            .unwrap_or(16_384);
        let keep_recent_tokens = env::var("DEX_KEEP_RECENT_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(file.keep_recent_tokens)
            .unwrap_or(20_000);
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
        // Dex standalone: no network at startup — models come from config/DEX_MODELS
        // or `dex update --models` cache (XDG_DATA_HOME/dex/models.json). Removed
        // live /models fetch (was 5s+ blocking per endpoint).
        let endpoints: BTreeMap<String, String> = match &file.endpoints {
            Some(map) => map.clone(),
            // Already pointed at opencode.ai: expose its zen/go gateways by
            // default so autocomplete shows which endpoint each model is on.
            None if provider == Provider::OpenCode
                && base_url.starts_with("https://opencode.ai/") =>
            {
                [
                    ("zen".to_string(), "https://opencode.ai/zen/v1".to_string()),
                    (
                        "go".to_string(),
                        "https://opencode.ai/zen/go/v1".to_string(),
                    ),
                ]
                .into_iter()
                .collect()
            }
            None => BTreeMap::new(),
        };
        // Dex own cache (no pi dependency) — bootstraps with just current model if missing.
        if available_models.is_empty() {
            if let Some(cached) = load_dex_models_cache() {
                available_models = cached;
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
            reserve_tokens,
            keep_recent_tokens,
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
        let result = if let Some((name, rest)) = selection.split_once('/') {
            if let Some(url) = self.endpoints.get(name) {
                self.base_url = url.clone();
                self.model = rest.to_string();
                Some(name.to_string())
            } else {
                self.model = selection.to_string();
                None
            }
        } else {
            self.model = selection.to_string();
            None
        };
        // Dex standalone: contextWindow from models.dev catalog > static table; refresh unless env/file pinned it.
        if env::var("DEX_CONTEXT_WINDOW").is_err() {
            let file_ctx = load_file_config().ok().and_then(|f| f.context_window);
            if file_ctx.is_none() {
                if let Some(catalog) = load_dex_catalog() {
                    if let Some(ctx) = catalog_context_window(&self.model, &catalog) {
                        self.context_window = ctx;
                    } else {
                        self.context_window = model_context_window(&self.model, self.provider);
                    }
                } else {
                    self.context_window = model_context_window(&self.model, self.provider);
                }
                self.max_prompt_tokens = self
                    .context_window
                    .saturating_sub(16_000)
                    .min(self.context_window * 3 / 4);
            }
        }
        result
    }

    /// Trigger compaction when prompt exceeds this many tokens.
    /// Pi: contextWindow - reserveTokens (16384) leaves room for reply.
    pub(crate) fn compaction_threshold(&self) -> u64 {
        self.context_window.saturating_sub(self.reserve_tokens)
    }

    /// Tokens reserved for the model's reply (pi default 16384).
    pub(crate) fn reserve_tokens(&self) -> u64 {
        self.reserve_tokens
    }

    pub(crate) fn keep_recent_tokens(&self) -> u64 {
        self.keep_recent_tokens
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
                (
                    "go".to_string(),
                    "https://opencode.ai/zen/go/v1".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
            api: ApiProtocol::Responses,
            account_id: None,
            thinking_effort: None,
            context_window: 128_000,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
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
        assert_eq!(
            cfg.apply_model("go/moonshotai/kimi-k2").as_deref(),
            Some("go")
        );
        assert_eq!(cfg.model, "moonshotai/kimi-k2");
        // Unknown prefix is a plain model id.
        assert_eq!(cfg.apply_model("unknown/m"), None);
        assert_eq!(cfg.model, "unknown/m");
    }

    #[test]
    fn provider_parse_accepts_aliases() {
        assert_eq!(Provider::parse("opencode").unwrap(), Provider::OpenCode);
        assert_eq!(Provider::parse("codex").unwrap(), Provider::OpenAiCodex);
        assert_eq!(
            Provider::parse("openai-codex").unwrap(),
            Provider::OpenAiCodex
        );
        assert!(Provider::parse("unknown").is_err());
    }

    #[test]
    fn permission_parse_and_ordering() {
        assert_eq!(
            PermissionMode::parse("read-only").unwrap(),
            PermissionMode::ReadOnly
        );
        assert_eq!(
            PermissionMode::parse("readonly").unwrap(),
            PermissionMode::ReadOnly
        );
        assert_eq!(
            PermissionMode::parse("ask_writes").unwrap(),
            PermissionMode::AskWrites
        );
        assert_eq!(
            PermissionMode::parse("trusted").unwrap(),
            PermissionMode::Trusted
        );
        assert!(PermissionMode::parse("nope").is_err());
        assert!(
            PermissionMode::ReadOnly.permissiveness() < PermissionMode::Trusted.permissiveness()
        );
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
        assert_eq!(
            cfg.models.as_deref(),
            Some(&["a".to_string(), "b".to_string()][..])
        );
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
