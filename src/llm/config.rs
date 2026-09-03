use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::time::Duration;

use crate::core::types::*;
use crate::llm::auth::*;

/// Config file location: `$DEX_CONFIG` > `$XDG_CONFIG_HOME/dex/config.yaml`
/// > `~/.config/dex/config.yaml`.
fn config_file_path() -> Option<std::path::PathBuf> {
    if let Some(p) = env::var_os("DEX_CONFIG") {
        return Some(std::path::PathBuf::from(p));
    }
    let dir = env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(dir.join("dex/config.yaml"))
}

/// Raw config file as YAML. Parsed as an untyped `Value` so unknown keys
/// survive the `/model` write-back. Missing/invalid file → None (env rules).
fn load_config_file() -> Option<serde_yaml::Value> {
    let path = config_file_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_yaml::from_str::<serde_yaml::Value>(&text) {
        Ok(value) => Some(value),
        Err(e) => {
            // A typo'd file must not silently disable every user setting.
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| eprintln!("dex: ignoring invalid config {}: {e}", path.display()));
            None
        }
    }
}

fn load_config_str(file: &Option<serde_yaml::Value>, key: &str) -> Option<String> {
    file.as_ref()
        .and_then(|f| f.get(key))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// True when the user explicitly pinned the wire protocol: the `OPENAI_API`
/// env var or an `api:` key in config.yaml. Per-model `DEX_MODEL_APIS`
/// entries don't count — they decide per model, but empirical fallback stays
/// available for unlisted models.
pub(crate) fn api_pinned() -> bool {
    if env::var("OPENAI_API").is_ok() {
        return true;
    }
    load_config_str(&load_config_file(), "api").is_some()
}

/// Persisted wire protocols learned empirically at runtime
/// (`XDG_CACHE_HOME/dex/learned-apis.json`): models that rejected
/// `/responses` and succeeded over `/chat/completions`. Keyed
/// `"<base_url>|<model>"`. Only consulted when nothing explicit pins the
/// protocol. ponytail: no expiry — a model that speaks completions keeps
/// working even after the provider adds responses support.
fn learned_apis_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = env::var_os("XDG_CACHE_HOME") {
        return Some(std::path::PathBuf::from(dir).join("dex/learned-apis.json"));
    }
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cache/dex/learned-apis.json"))
}

fn learned_api(base_url: &str, model: &str) -> Option<ApiProtocol> {
    let text = std::fs::read_to_string(learned_apis_path()?).ok()?;
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text).ok()?;
    map.get(format!("{base_url}|{model}").as_str())?
        .as_str()
        .and_then(ApiProtocol::parse)
}

/// Best-effort write; a lost race between concurrent learners just re-learns.
pub(crate) fn remember_learned_api(base_url: &str, model: &str, api: ApiProtocol) {
    let Some(path) = learned_apis_path() else {
        return;
    };
    let mut map: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    map.insert(
        format!("{base_url}|{model}"),
        serde_json::Value::from(api.name()),
    );
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(path, text);
    }
}

/// Write a model/provider selection back to the config file: `model:` keeps
/// the raw selection (endpoint prefixes like `go/…` re-route on restart),
/// `provider:`/`base_url:` the resolved values. Everything else (api_key,
/// api, comments excepted) is preserved verbatim. Best-effort: a read-only
/// or missing file silently skips the write.
/// ponytail: serde_yaml drops comments on write-back; restructure the file
/// if round-tripping comments ever matters.
fn persist_selection(selection: &str, provider: Provider, base_url: &str) {
    let Some(path) = config_file_path() else {
        return;
    };
    let mut root: serde_yaml::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_yaml::from_str(&text).ok())
        .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    if let Some(map) = root.as_mapping_mut() {
        for (k, v) in [
            ("model", serde_yaml::Value::from(selection)),
            ("provider", serde_yaml::Value::from(provider.name())),
            ("base_url", serde_yaml::Value::from(base_url)),
        ] {
            map.insert(serde_yaml::Value::from(k), v);
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_yaml::to_string(&root) {
        let _ = std::fs::write(path, text);
    }
}

pub(crate) fn provider_default_context_window(provider: Provider) -> u64 {
    match provider {
        Provider::OpenCode => 128_000,
        Provider::OpenAiCodex => 128_000,
    }
}

pub(crate) fn provider_endpoints(provider: Provider) -> BTreeMap<String, String> {
    if provider == Provider::OpenCode {
        // ponytail: static table, add dynamic registry if more than 3 providers/endpoints
        [
            ("zen".to_string(), "https://opencode.ai/zen/v1".to_string()),
            (
                "go".to_string(),
                "https://opencode.ai/zen/go/v1".to_string(),
            ),
        ]
        .into_iter()
        .collect()
    } else {
        BTreeMap::new()
    }
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

/// Cost for one prompt, using models.dev pricing when available.
/// `input`/`cache_read` are per 1M tokens in the catalog; output tiers
/// and context-tier pricing are omitted for now (ponytail: add when a
/// model actually bills them differently enough to matter).
pub(crate) fn cost_for_prompt(
    model: &str,
    prompt_tokens: u64,
    cached_tokens: Option<u64>,
) -> Option<f64> {
    let catalog = load_dex_catalog()?;
    let needle = model.to_ascii_lowercase();
    let mut cost_val: Option<&serde_json::Value> = None;
    if let Some(providers) = catalog.as_object() {
        for (_prov, entry) in providers {
            if let Some(models) = entry.get("models").and_then(|m| m.as_object()) {
                if let Some(m) = models.get(needle.as_str()).or_else(|| {
                    models
                        .iter()
                        .find(|(k, _)| k.to_ascii_lowercase() == needle)
                        .map(|(_, v)| v)
                }) {
                    if let Some(c) = m.get("cost") {
                        cost_val = Some(c);
                        break;
                    }
                }
            }
        }
    }
    let cost = cost_val?;
    let input_rate = cost.get("input").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let cache_read_rate = cost
        .get("cache_read")
        .or_else(|| cost.get("cacheRead"))
        .and_then(|v| v.as_f64())
        .unwrap_or(input_rate);
    let cached = cached_tokens.unwrap_or(0).min(prompt_tokens);
    let fresh = prompt_tokens.saturating_sub(cached);
    let total =
        fresh as f64 * input_rate / 1_000_000.0 + cached as f64 * cache_read_rate / 1_000_000.0;
    Some(total)
}

fn load_dex_models_cache() -> Option<Vec<String>> {
    // dex cache is models.dev api.json — expose both bare and provider-qualified ids
    // so /model shows provider and can set base_url without env
    if let Some(catalog) = load_dex_catalog() {
        if let Some(providers) = catalog.as_object() {
            let mut ids: Vec<String> = Vec::new();
            for (prov_key, entry) in providers {
                if let Some(models) = entry.get("models").and_then(|m| m.as_object()) {
                    // Only dex-known providers get a qualified variant; others just bare
                    let dex_prefix: Option<&str> = match prov_key.as_str() {
                        "opencode" => Some("opencode"),
                        "opencode-go" => Some("go"),
                        "openai" => Some("opencode"),
                        "openai-codex" | "codex" => Some("openai-codex"),
                        _ => None,
                    };
                    for id in models.keys() {
                        ids.push(id.clone());
                        if let Some(prefix) = dex_prefix {
                            if prefix != id.as_str() {
                                ids.push(format!("{prefix}/{id}"));
                            }
                        }
                    }
                }
            }
            if !ids.is_empty() {
                ids.sort();
                ids.dedup();
                return Some(ids);
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
    let provider_name = std::env::var("DEX_PROVIDER")
        .ok()
        .unwrap_or_else(|| "opencode".to_string());
    let provider = Provider::parse(&provider_name)?;
    if provider != Provider::OpenCode {
        return Err("models.dev fetch failed and provider is not opencode".into());
    }
    let env_base_url = std::env::var("OPENAI_BASE_URL")
        .ok()
        .filter(|v| !v.is_empty());
    let base_url = env_base_url
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .ok_or("models.dev unavailable and OPENAI_API_KEY not set for fallback")?;
    let endpoints: BTreeMap<String, String> = if base_url.starts_with("https://opencode.ai/") {
        [
            ("zen".to_string(), "https://opencode.ai/zen/v1".to_string()),
            (
                "go".to_string(),
                "https://opencode.ai/zen/go/v1".to_string(),
            ),
        ]
        .into_iter()
        .collect()
    } else {
        BTreeMap::new()
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

/// Dex-owned per-model table (`DEX_MODEL_APIS="id=api,..."`, e.g.
/// `"kimi-k2.6=openai-completions"`); full `endpoint/id` selection first,
/// then the bare id. None keeps the current/global api.
pub(crate) fn model_api_from_env(selection: &str, bare_id: &str) -> Option<ApiProtocol> {
    let raw = env::var("DEX_MODEL_APIS").ok()?;
    let mut bare = None;
    for entry in raw.split(',') {
        let Some((id, name)) = entry.split_once('=') else {
            continue;
        };
        // Full `endpoint/id` selection wins regardless of entry order.
        if id.trim() == selection {
            if let Some(api) = ApiProtocol::parse(name.trim()) {
                return Some(api);
            }
        } else if bare.is_none() && id.trim() == bare_id {
            bare = ApiProtocol::parse(name.trim());
        }
    }
    bare
}

pub(crate) fn permission_from_env() -> Result<PermissionMode, Box<dyn std::error::Error>> {
    let value = env::var("DEX_PERMISSION")
        .ok()
        .unwrap_or_else(|| "trusted".to_string());
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
    /// Per-model (models.dev catalog) unless overridden by DEX_CONTEXT_WINDOW / file.
    pub(crate) context_window: u64,
    pub(crate) reserve_tokens: u64,
    pub(crate) keep_recent_tokens: u64,
    pub(crate) permission: PermissionMode,
    pub(crate) verify_command: Option<String>,
    pub(crate) client: reqwest::blocking::Client,
}

impl LlmConfig {
    pub(crate) fn from_env(
        base_url_override: Option<String>,
        model_override: Option<String>,
        permission_override: Option<PermissionMode>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        crate::tools::set_output_limit(
            env::var("DEX_TOOL_OUTPUT_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1_048_576),
        );
        let permission = match permission_override {
            Some(mode) => mode,
            None => permission_from_env()?,
        };
        let file = load_config_file();
        let provider_name = env::var("DEX_PROVIDER")
            .ok()
            .or_else(|| load_config_str(&file, "provider"))
            .unwrap_or_else(|| "opencode".to_string());
        let provider = Provider::parse(&provider_name)?;
        let model = model_override
            .or_else(|| env::var("OPENAI_MODEL").ok())
            .or_else(|| load_config_str(&file, "model"))
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
            .unwrap_or_default();
        let env_base_url = env::var("OPENAI_BASE_URL").ok().filter(|v| !v.is_empty());
        let explicit_base_url = base_url_override.is_some();
        let base_url = base_url_override
            .or(env_base_url)
            .or_else(|| load_config_str(&file, "base_url"))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| provider.default_base_url().to_string());
        let api_name = env::var("OPENAI_API")
            .ok()
            .or_else(|| load_config_str(&file, "api"))
            .unwrap_or_else(|| "openai-responses".to_string());
        let api = match ApiProtocol::parse(&api_name) {
            Some(api) => api,
            None => {
                return Err(format!(
                    "unsupported api '{}'; use openai-completions or openai-responses",
                    api_name
                )
                .into())
            }
        };
        // The model carries its own wire protocol; the global `api` above is
        // only the default. `OPENAI_API` pins everything.
        let api = if env::var("OPENAI_API").is_ok() {
            api
        } else {
            model_api_from_env(&model, &model).unwrap_or(api)
        };
        let (api_key, account_id) = match provider {
            Provider::OpenCode => (
                env::var("OPENAI_API_KEY")
                    .ok()
                    .or_else(|| load_config_str(&file, "api_key"))
                    .ok_or("OPENAI_API_KEY not set (export it or add it to your shell profile)")?,
                None,
            ),
            Provider::OpenAiCodex => load_codex_credentials()?,
        };
        let context_window = env::var("DEX_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| load_dex_catalog().and_then(|c| catalog_context_window(&model, &c)))
            .unwrap_or_else(|| provider_default_context_window(provider));
        // Pi: reserve 16384, keep 20000 tokens recent (not 12 messages)
        let reserve_tokens = env::var("DEX_RESERVE_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(16_384);
        let keep_recent_tokens = env::var("DEX_KEEP_RECENT_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20_000);
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(
                env::var("DEX_HTTP_CONNECT_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10),
            ))
            // Note: for the blocking client this deadline applies to the
            // connect and to each individual body read (not to the whole
            // streamed response), so long-lived SSE streams are safe.
            .timeout(Duration::from_secs(
                env::var("DEX_HTTP_REQUEST_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(300),
            ))
            .build()?;
        // Dex standalone: no network at startup — models come from config/DEX_MODELS
        // or `dex update --models` cache (XDG_DATA_HOME/dex/models.json). Removed
        // live /models fetch (was 5s+ blocking per endpoint).
        // Always expose zen/go for OpenCode so /model can set base_url without env
        let endpoints: BTreeMap<String, String> = provider_endpoints(provider);
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
            thinking_effort: env::var("DEX_THINKING_EFFORT")
                .ok()
                .filter(|v| !v.is_empty()),
            context_window,
            reserve_tokens,
            keep_recent_tokens,
            verify_command: env::var("DEX_VERIFY").ok(),
            permission,
            client,
            endpoints,
        };
        // A prefixed model override (--model go/foo or the daemon's
        // per-request model) routes to its endpoint; bare ids keep the
        // resolved base_url. An explicit --base-url wins over routing.
        if !explicit_base_url {
            this.apply_model(&model, false);
        }
        Ok(this)
    }

    /// Apply a `/model` selection. `provider/model` switches provider (and its
    /// base_url) when `provider` parses as a known provider; `endpoint/model`
    /// routes to the named endpoint's base_url. Provider prefix is stripped
    /// first, then endpoint routing runs on the remainder. Returns the endpoint
    /// name when an endpoint route was taken. With `persist`, the selection is
    /// written back to the config file (it becomes the new default).
    pub(crate) fn apply_model(&mut self, selection: &str, persist: bool) -> Option<String> {
        let prev_model = self.model.clone();
        let prev_provider = self.provider;
        // Provider-qualified: "opencode/gpt-..." or "openai-codex/gpt-..." sets base_url without env
        let mut sel = selection;
        if let Some((prefix, rest)) = selection.split_once('/') {
            if let Ok(new_provider) = Provider::parse(prefix) {
                if new_provider != self.provider {
                    // Best-effort credential switch; failure surfaces at next LLM call
                    let creds = match new_provider {
                        Provider::OpenCode => env::var("OPENAI_API_KEY").ok().map(|k| (k, None)),
                        Provider::OpenAiCodex => load_codex_credentials().ok(),
                    };
                    if let Some((k, acct)) = creds {
                        self.api_key = k;
                        self.account_id = acct;
                    }
                    // Switch even without creds so /model shows intent;
                    // auth failure surfaces at the next LLM call.
                    self.provider = new_provider;
                    self.base_url = new_provider.default_base_url().to_string();
                    self.endpoints = provider_endpoints(new_provider);
                }
                sel = rest;
            }
        }
        let result = if let Some((name, rest)) = sel.split_once('/') {
            if let Some(url) = self.endpoints.get(name).cloned() {
                self.base_url = url.clone();
                self.model = rest.to_string();
                Some(name.to_string())
            } else {
                self.model = sel.to_string();
                None
            }
        } else {
            self.model = sel.to_string();
            None
        };
        // The model carries its wire protocol; the global `api` is the
        // fallback and `OPENAI_API` pins it.
        if env::var("OPENAI_API").is_err() {
            if let Some(api) = model_api_from_env(selection, &self.model) {
                self.api = api;
            } else if let Some(learned) = learned_api(&self.base_url, &self.model) {
                self.api = learned;
            }
        }
        if persist && (self.model != prev_model || self.provider != prev_provider) {
            persist_selection(selection, self.provider, &self.base_url);
        }
        // Dex standalone: contextWindow from models.dev catalog > provider default; refresh unless env pinned it.
        if env::var("DEX_CONTEXT_WINDOW").is_err() {
            if let Some(catalog) = load_dex_catalog() {
                if let Some(ctx) = catalog_context_window(&self.model, &catalog) {
                    self.context_window = ctx;
                } else {
                    self.context_window = provider_default_context_window(self.provider);
                }
            } else {
                self.context_window = provider_default_context_window(self.provider);
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
    #[allow(dead_code)]
    pub(crate) fn reserve_tokens(&self) -> u64 {
        self.reserve_tokens
    }

    pub(crate) fn keep_recent_tokens(&self) -> u64 {
        self.keep_recent_tokens
    }

    pub(crate) fn switch_provider(
        &mut self,
        provider: Provider,
        persist: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let env_base_url = env::var("OPENAI_BASE_URL").ok().filter(|v| !v.is_empty());
        let (api_key, account_id) = match provider {
            Provider::OpenCode => (
                env::var("OPENAI_API_KEY")
                    .ok()
                    .ok_or("OPENAI_API_KEY not set (export it or add it to your shell profile)")?,
                None,
            ),
            Provider::OpenAiCodex => load_codex_credentials()?,
        };
        self.provider = provider;
        self.api_key = api_key;
        self.account_id = account_id;
        self.base_url = env_base_url
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| provider.default_base_url().to_string());
        self.endpoints = provider_endpoints(provider);
        let api_name = env::var("OPENAI_API").ok();
        self.api = api_name
            .as_deref()
            .and_then(ApiProtocol::parse)
            .unwrap_or(ApiProtocol::Responses);
        // Keep the current model's own protocol when the global `api` is not
        // explicitly pinned via `OPENAI_API`.
        if env::var("OPENAI_API").is_err() {
            if let Some(api) = model_api_from_env(&self.model, &self.model) {
                self.api = api;
            }
        }
        if persist {
            persist_selection(&self.model, provider, &self.base_url);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
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
            verify_command: None,
            client: reqwest::blocking::Client::new(),
        };
        // Bare id keeps the current base_url.
        assert_eq!(cfg.apply_model("gpt-5.6-luna", false), None);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        // Prefixed id routes to the endpoint and strips the prefix.
        assert_eq!(cfg.apply_model("go/kimi-k2", false).as_deref(), Some("go"));
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.model, "kimi-k2");
        // Provider-native slashes in model ids survive under an endpoint.
        assert_eq!(
            cfg.apply_model("go/moonshotai/kimi-k2", false).as_deref(),
            Some("go")
        );
        assert_eq!(cfg.model, "moonshotai/kimi-k2");
        // Unknown prefix is a plain model id.
        assert_eq!(cfg.apply_model("unknown/m", false), None);
        assert_eq!(cfg.model, "unknown/m");
    }

    pub(crate) fn test_cfg() -> LlmConfig {
        LlmConfig {
            provider: Provider::OpenCode,
            api_key: "k".into(),
            base_url: "https://opencode.ai/zen/v1".into(),
            model: "m-r".into(),
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
            verify_command: None,
            client: reqwest::blocking::Client::new(),
        }
    }

    /// Save/restore process env around tests that redirect dex env vars.
    struct EnvRestore {
        vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvRestore {
        fn take(keys: &[&'static str]) -> Self {
            Self {
                vars: keys.iter().map(|k| (*k, std::env::var_os(k))).collect(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, prev) in self.vars.drain(..) {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// DEX_MODEL_APIS table with `OPENAI_API` unpinned so resolution runs.
    fn model_apis_env(table: &str) -> EnvRestore {
        let guard = EnvRestore::take(&["DEX_MODEL_APIS", "OPENAI_API"]);
        std::env::set_var("DEX_MODEL_APIS", table);
        std::env::remove_var("OPENAI_API");
        guard
    }

    #[test]
    fn apply_model_follows_model_apis() {
        // Serializes process-env redirection (DEX_MODEL_APIS/OPENAI_API)
        // against daemon tests holding the same guard.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = model_apis_env("m-c=openai-completions");
        let mut cfg = test_cfg();
        cfg.apply_model("m-c", false);
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        // Unknown ids keep the current protocol.
        cfg.apply_model("m-r", false);
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
    }

    #[test]
    fn apply_model_full_selection_key_wins() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = model_apis_env("m-x=openai-completions,go/m-x=openai-responses");
        let mut cfg = test_cfg();
        cfg.apply_model("go/m-x", false);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.model, "m-x");
        assert_eq!(cfg.api, ApiProtocol::Responses);
        cfg.apply_model("m-x", false);
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
    }

    #[test]
    fn model_api_from_env_ignores_malformed_entries() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["DEX_MODEL_APIS"]);
        std::env::remove_var("DEX_MODEL_APIS");
        assert_eq!(
            model_api_from_env("m-c", "m-c"),
            None,
            "unset table resolves nothing"
        );
        std::env::set_var("DEX_MODEL_APIS", "junk-no-equals,m-c=bogus-api,m-c=chat");
        assert_eq!(
            model_api_from_env("m-c", "m-c"),
            Some(ApiProtocol::ChatCompletions),
            "first parseable match wins"
        );
    }

    #[test]
    fn provider_parse_accepts_aliases() {
        assert_eq!(Provider::parse("opencode").unwrap(), Provider::OpenCode);
        assert_eq!(Provider::parse("openai").unwrap(), Provider::OpenCode);
        assert_eq!(Provider::parse("codex").unwrap(), Provider::OpenAiCodex);
        assert_eq!(
            Provider::parse("openai-codex").unwrap(),
            Provider::OpenAiCodex
        );
        assert!(Provider::parse("unknown").is_err());
        assert_eq!(
            Provider::OpenCode.default_base_url(),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            Provider::OpenAiCodex.default_base_url(),
            "https://chatgpt.com/backend-api/codex"
        );
    }

    #[test]
    fn apply_model_switches_provider_and_sets_base_url_without_env() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = EnvRestore::take(&[
            "OPENAI_API_KEY",
            "CODEX_ACCESS_TOKEN",
            "CODEX_ACCOUNT_ID",
            "OPENAI_BASE_URL",
        ]);
        std::env::set_var("OPENAI_API_KEY", "test-key");
        std::env::set_var("CODEX_ACCESS_TOKEN", "codex-tok");
        std::env::remove_var("OPENAI_BASE_URL");
        let mut cfg = test_cfg();
        // starts as OpenCode @ zen
        assert_eq!(cfg.provider, Provider::OpenCode);
        // Switch to codex via provider-qualified model — no env base_url required
        cfg.apply_model("openai-codex/gpt-5.6-luna", false);
        assert_eq!(cfg.provider, Provider::OpenAiCodex);
        assert_eq!(cfg.base_url, Provider::OpenAiCodex.default_base_url());
        assert_eq!(cfg.model, "gpt-5.6-luna");
        // Switch back via alias "openai"
        cfg.apply_model("openai/gpt-4o", false);
        assert_eq!(cfg.provider, Provider::OpenCode);
        assert_eq!(cfg.base_url, Provider::OpenCode.default_base_url());
        assert_eq!(cfg.model, "gpt-4o");
        // Provider + endpoint: opencode/go/kimi -> go endpoint
        cfg.apply_model("opencode/go/kimi-k2", false);
        assert_eq!(cfg.provider, Provider::OpenCode);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.model, "kimi-k2");
        // Bare endpoint still works without provider prefix
        cfg = test_cfg();
        cfg.apply_model("go/kimi-k2", false);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.model, "kimi-k2");
    }

    #[test]
    fn endpoints_always_available_for_opencode_without_env() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = EnvRestore::take(&[
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "OPENAI_MODEL",
            "OPENAI_API",
            "DEX_PROVIDER",
            "DEX_MODELS",
            "DEX_CONFIG",
        ]);
        std::env::set_var("OPENAI_API_KEY", "test-key2");
        std::env::remove_var("OPENAI_BASE_URL");
        std::env::remove_var("DEX_MODELS");
        std::env::set_var("DEX_PROVIDER", "opencode");
        // No config file: point DEX_CONFIG at a missing path.
        std::env::set_var(
            "DEX_CONFIG",
            std::env::temp_dir().join(format!("dex-missing-{}", std::process::id())),
        );
        let cfg = LlmConfig::from_env(None, None, None).unwrap();
        assert_eq!(cfg.base_url, Provider::OpenCode.default_base_url());
        assert!(cfg.endpoints.contains_key("go"));
        assert!(cfg.endpoints.contains_key("zen"));
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
        // Restore the cwd *before* deleting the temp dir: this runs
        // concurrently with other tests, and a deleted process cwd makes
        // `current_dir()` return None for them.
        std::env::set_current_dir(prev).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn models_response_parses_openai_shape() {
        let json = r#"{"object":"list","data":[{"id":"a"},{"id":"b"}]}"#;
        let parsed: ModelsResponse = serde_json::from_str(json).unwrap();
        let ids: Vec<String> = parsed.data.into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn config_file_defaults_apply_and_env_wins() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENAI_MODEL",
            "OPENAI_API",
            "OPENAI_BASE_URL",
            "OPENAI_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_CONTEXT_WINDOW",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-filecfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(
            &path,
            "provider: opencode\napi_key: file-key\nbase_url: https://file.example/v1\nmodel: file-model\napi: openai-completions\ncustom_key: keep-me\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", &path);
        std::env::set_var("OPENAI_API_KEY", "env-key");
        // No OPENAI_MODEL: file model + file base_url + file api apply.
        let cfg = LlmConfig::from_env(None, None, None).unwrap();
        assert_eq!(cfg.model, "file-model");
        assert_eq!(cfg.base_url, "https://file.example/v1");
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        // Env model beats the file.
        std::env::set_var("OPENAI_MODEL", "env-model");
        let cfg = LlmConfig::from_env(None, None, None).unwrap();
        assert_eq!(cfg.model, "env-model");
        // Write-back: selection lands in the file, unknown keys survive.
        let mut cfg = LlmConfig::from_env(None, None, None).unwrap();
        cfg.apply_model("go/new-model", true);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("model: go/new-model"));
        assert!(text.contains("custom_key: keep-me"));
        assert!(text.contains("api_key: file-key"));
        // Learned protocol: remembered to the cache, picked up on the next
        // config build (nothing explicit pins this model's protocol).
        let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
        let cache = dir.join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        remember_learned_api(
            "https://file.example/v1",
            "file-model",
            ApiProtocol::ChatCompletions,
        );
        assert!(cache.join("dex/learned-apis.json").exists());
        let cfg = LlmConfig::from_env(None, None, None).unwrap();
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
