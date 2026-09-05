use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use crate::core::types::{ApiProtocol, PermissionMode, Provider};
use crate::llm::auth::load_codex_credentials;
use crate::llm::provider::{DEFAULT_CONTEXT_WINDOW, DEFAULT_MODEL};

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
/// Cached process-wide and invalidated by file identity (path + mtime +
/// length): `from_env` runs per chat turn on the daemon, and each call was
/// re-reading + re-parsing the file (plus once more via `api_pinned`).
struct CachedConfigFile {
    path: std::path::PathBuf,
    mtime: SystemTime,
    len: u64,
    value: Option<serde_yaml::Value>,
}

static CONFIG_CACHE: OnceLock<Mutex<Option<CachedConfigFile>>> = OnceLock::new();

fn load_config_file() -> Option<serde_yaml::Value> {
    let path = config_file_path()?;
    let meta = std::fs::metadata(&path).ok()?;
    let (mtime, len) = (meta.modified().ok()?, meta.len());
    if let Some(hit) = CONFIG_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|cached| cached.path == path && cached.mtime == mtime && cached.len == len)
    {
        return hit.value.clone();
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let value = match serde_yaml::from_str::<serde_yaml::Value>(&text) {
        Ok(value) => Some(value),
        Err(e) => {
            // A typo'd file must not silently disable every user setting.
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| eprintln!("dex: ignoring invalid config {}: {e}", path.display()));
            None
        }
    };
    CONFIG_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(CachedConfigFile {
            path,
            mtime,
            len,
            value: value.clone(),
        });
    value
}

fn invalidate_config_cache() {
    if let Some(cache) = CONFIG_CACHE.get() {
        cache.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

fn load_config_str(file: &Option<serde_yaml::Value>, key: &str) -> Option<String> {
    file.as_ref()
        .and_then(|f| f.get(key))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// A configured generic provider (`providers:` map in config.yaml): the
/// deposit place for that provider's API key plus optional overrides.
/// Endpoint, models, pricing, context windows and reasoning options come
/// from the models.dev catalog entry of the same key; the wire protocol is
/// learned empirically unless pinned here.
#[derive(Clone, Default)]
pub(crate) struct ProviderEntry {
    pub(crate) api_key: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) api: Option<ApiProtocol>,
}

fn load_provider_entries(file: &Option<serde_yaml::Value>) -> BTreeMap<String, ProviderEntry> {
    let Some(map) = file
        .as_ref()
        .and_then(|f| f.get("providers"))
        .and_then(|p| p.as_mapping())
    else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (key, value) in map {
        let Some(name) = key
            .as_str()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        let entry = Some(value.clone());
        out.insert(
            name,
            ProviderEntry {
                api_key: load_config_str(&entry, "api_key"),
                base_url: load_config_str(&entry, "base_url"),
                api: load_config_str(&entry, "api")
                    .as_deref()
                    .and_then(ApiProtocol::parse),
            },
        );
    }
    out
}

/// Catalog `api` URL for a provider key ("zai" → its serving endpoint).
/// Entries without one (native-API providers like anthropic) are not usable
/// as generic OpenAI-compatible providers — that absence is the gate.
fn catalog_api(key: &str, catalog: &serde_json::Value) -> Option<String> {
    catalog
        .get(key)
        .and_then(|e| e.get("api"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// The provider's own conventional API-key env var (first key of the catalog
/// `env` map: ZHIPU_API_KEY, OPENROUTER_API_KEY, …), so deposits work with
/// the names each provider already documents instead of dex-invented ones.
fn catalog_env_var(key: &str, catalog: &serde_json::Value) -> Option<String> {
    catalog
        .get(key)
        .and_then(|e| e.get("env"))
        .and_then(|v| v.as_object())
        .and_then(|m| m.keys().next().cloned())
}

/// Landing base URL when nothing explicit is set: the builtin default, or
/// for a generic provider its config entry override > catalog `api` URL.
fn landing_base_url_for(
    provider: &Provider,
    entries: &BTreeMap<String, ProviderEntry>,
) -> Option<String> {
    match provider {
        Provider::Generic(name) => entries
            .get(name)
            .and_then(|e| e.base_url.clone())
            .or_else(|| load_dex_catalog().and_then(|c| catalog_api(name, &c))),
        other => other.default_base_url().map(str::to_string),
    }
}

/// Endpoint table for `/model` routing: builtin endpoints plus, for generic
/// providers, their single catalog/config endpoint under the provider name.
fn endpoints_for(
    provider: &Provider,
    entries: &BTreeMap<String, ProviderEntry>,
) -> BTreeMap<String, String> {
    let mut out = provider.endpoints();
    if let Provider::Generic(name) = provider {
        if let Some(url) = landing_base_url_for(provider, entries) {
            out.insert(name.clone(), url);
        }
    }
    out
}

/// Per-provider credentials: config `providers.<name>.api_key` > the
/// provider's own conventional env var from the catalog (`ZHIPU_API_KEY`,
/// `OPENROUTER_API_KEY`, …) > legacy fallbacks. Codex reads its own
/// credential file and ignores all of this.
pub(crate) fn resolve_credentials(
    provider: &Provider,
    entries: &BTreeMap<String, ProviderEntry>,
    file_api_key: Option<&str>,
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    match provider {
        Provider::OpenCode => Ok((
            env::var("OPENAI_API_KEY")
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| file_api_key.map(str::to_string))
                .filter(|v| !v.is_empty())
                .ok_or("OPENAI_API_KEY not set (export it or add api_key to config.yaml)")?,
            None,
        )),
        Provider::OpenAiCodex => load_codex_credentials(),
        Provider::Generic(name) => {
            if let Some(key) = entries
                .get(name)
                .and_then(|e| e.api_key.clone())
                .filter(|k| !k.is_empty())
            {
                return Ok((key, None));
            }
            let env_name = load_dex_catalog().and_then(|c| catalog_env_var(name, &c));
            if let Some(var) = &env_name {
                if let Ok(key) = env::var(var) {
                    if !key.trim().is_empty() {
                        return Ok((key, None));
                    }
                }
            }
            Err(format!(
                "no API key for provider '{name}': set providers.{name}.api_key in config.yaml{}",
                env_name.map(|v| format!(" or export {v}")).unwrap_or_else(|| {
                    " or export the provider's key env var (run `dex update --models` to learn its name)"
                        .to_string()
                })
            )
            .into())
        }
    }
}

/// True when the user explicitly pinned the wire protocol: the `OPENAI_API`
/// env var or an `api:` key in config.yaml. Per-model `DEX_MODEL_APIS`
/// entries don't count — they decide per model, but empirical fallback stays
/// available for unlisted models.
pub(crate) fn api_pinned(active_provider: Option<&str>) -> bool {
    if env::var("OPENAI_API").is_ok() {
        return true;
    }
    let file = load_config_file();
    if load_config_str(&file, "api").is_some() {
        return true;
    }
    active_provider
        .and_then(|name| {
            load_provider_entries(&file)
                .get(name.to_ascii_lowercase().as_str())
                .cloned()
        })
        .and_then(|e| e.api)
        .is_some()
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

/// Learned wire protocols, cached process-wide and invalidated by file
/// identity. `from_env` consulted this file on every turn (one read + parse
/// per turn); hits are now a mutex bump.
struct CachedLearnedApis {
    path: std::path::PathBuf,
    mtime: SystemTime,
    len: u64,
    map: serde_json::Map<String, serde_json::Value>,
}

static LEARNED_CACHE: OnceLock<Mutex<Option<CachedLearnedApis>>> = OnceLock::new();

fn learned_api_map() -> serde_json::Map<String, serde_json::Value> {
    let Some(path) = learned_apis_path() else {
        return Default::default();
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return Default::default();
    };
    let (Ok(mtime), len) = (meta.modified(), meta.len()) else {
        return Default::default();
    };
    if let Some(hit) = LEARNED_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|cached| cached.path == path && cached.mtime == mtime && cached.len == len)
    {
        return hit.map.clone();
    }
    let map: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    LEARNED_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(CachedLearnedApis {
            path,
            mtime,
            len,
            map: map.clone(),
        });
    map
}

fn learned_api(base_url: &str, model: &str) -> Option<ApiProtocol> {
    learned_api_map()
        .get(format!("{base_url}|{model}").as_str())?
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
        // The file changed under us; drop the cached map so the next
        // lookup re-reads instead of serving the pre-write copy.
        if let Some(cache) = LEARNED_CACHE.get() {
            cache.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
    }
}

/// Write a model/provider selection back to the config file: `model:` keeps
/// the raw selection (endpoint prefixes like `go/…` re-route on restart),
/// `provider:`/`base_url:` the resolved values. Everything else (api_key,
/// api, comments excepted) is preserved verbatim. Best-effort: a read-only
/// or missing file silently skips the write.
/// ponytail: serde_yaml drops comments on write-back; restructure the file
/// if round-tripping comments ever matters.
/// Reasoning-effort options the selected model advertises (models.dev
/// `reasoning_options`, e.g. glm-5.3-flash: low/high/max). Shown in `/model`
/// replies so the thinking knob is discoverable per model; `None` when the
/// catalog has no entry or advertises no effort options.
pub(crate) fn reasoning_options_for(model: &str) -> Option<Vec<String>> {
    let catalog = load_dex_catalog()?;
    let needle = model.to_ascii_lowercase();
    let entry = catalog.as_object()?.values().find_map(|provider| {
        provider
            .get("models")
            .and_then(|m| m.as_object())
            .and_then(|models| {
                models
                    .iter()
                    .find(|(id, _)| id.to_ascii_lowercase() == needle)
                    .map(|(_, v)| v)
            })
    })?;
    let options = entry.get("reasoning_options")?.as_array()?;
    let values: Vec<String> = options
        .iter()
        .filter_map(|o| o.get("values"))
        .filter_map(|v| v.as_array())
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (!values.is_empty()).then_some(values)
}

fn persist_selection(selection: &str, provider: &Provider, base_url: &str) {
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
        invalidate_config_cache();
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

/// Slim cross-process context index (`models.ctx.json`): `lowercased model
/// id → context window`. The full catalog is 4+ MB, so every fresh process
/// paid a full read + parse (~180ms) just to look up one model. The index is
/// KBs; warm launches (and every daemon turn) hit it and skip the catalog.
/// Written by `refresh_models_cache` and lazily rebuilt whenever the catalog
/// is newer than the index.
fn dex_ctx_index_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(std::path::PathBuf::from(dir).join("dex/models.ctx.json"));
    }
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache/dex/models.ctx.json"))
}

fn ctx_from_index(model: &str) -> Option<u64> {
    // KB-sized file: one small read + parse instead of the 4MB catalog.
    let path = dex_ctx_index_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text).ok()?;
    map.get(model.to_ascii_lowercase().as_str())?
        .as_u64()
        .filter(|ctx| *ctx > 0)
}

/// Collect every known `model id → context` pair from the catalog (both the
/// `api.json` providers shape and the `catalog.json` models shape) so the
/// slim index answers without the 4MB parse.
fn build_ctx_map(catalog: &serde_json::Value) -> BTreeMap<String, u64> {
    fn context_of(entry: &serde_json::Value) -> Option<u64> {
        entry
            .get("limit")
            .and_then(|l| l.get("context"))
            .and_then(|c| c.as_u64())
            .filter(|ctx| *ctx > 0)
    }
    let mut map = BTreeMap::new();
    if let Some(providers) = catalog.as_object() {
        for (_prov, entry) in providers {
            if let Some(models) = entry.get("models").and_then(|m| m.as_object()) {
                for (id, m) in models {
                    if let Some(ctx) = context_of(m) {
                        map.insert(id.to_ascii_lowercase(), ctx);
                    }
                }
            }
        }
    }
    if let Some(models) = catalog.get("models").and_then(|m| m.as_object()) {
        for (id, m) in models {
            if let Some(ctx) = context_of(m) {
                map.insert(id.to_ascii_lowercase(), ctx);
            }
        }
    }
    map
}

/// Best-effort index write; failures just mean the next launch re-parses.
/// Atomic (tmp file + rename) so a concurrent `refresh` never leaves a torn
/// `models.ctx.json` for a reader mid-turn; a stale reader just falls back to
/// the full catalog parse on JSON error.
fn write_ctx_index(map: &BTreeMap<String, u64>) {
    let Some(path) = dex_ctx_index_path() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(map) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Rebuild the slim index when it is missing or older than the catalog.
/// Runs only on the slow path (right after the full catalog parse), so warm
/// launches never pay for it.
fn ensure_ctx_index(catalog: &serde_json::Value) {
    let (Some(index_path), Some(catalog_path)) = (dex_ctx_index_path(), dex_catalog_cache_path())
    else {
        return;
    };
    let catalog_mtime = std::fs::metadata(&catalog_path)
        .ok()
        .and_then(|m| m.modified().ok());
    let index_mtime = std::fs::metadata(&index_path)
        .ok()
        .and_then(|m| m.modified().ok());
    let stale = match (catalog_mtime, index_mtime) {
        (Some(c), Some(i)) => c > i,
        _ => true,
    };
    if stale {
        write_ctx_index(&build_ctx_map(catalog));
    }
}

/// Parsed `models.dev.json` catalog, cached process-wide and invalidated by
/// file identity (path + mtime + length). The catalog is 4+ MB and was
/// re-parsed on every `LlmConfig::from_env` — i.e. on each TUI launch (via
/// `/api/config`) and each chat turn (~180ms a pop). Shared through an `Arc`
/// so cache hits are an atomic bump, not a deep clone of the whole tree.
struct CachedCatalog {
    path: std::path::PathBuf,
    mtime: SystemTime,
    len: u64,
    value: std::sync::Arc<serde_json::Value>,
}

static CATALOG_CACHE: OnceLock<Mutex<Option<CachedCatalog>>> = OnceLock::new();

fn load_dex_catalog() -> Option<std::sync::Arc<serde_json::Value>> {
    let path = dex_catalog_cache_path()?;
    let meta = std::fs::metadata(&path).ok()?;
    let (mtime, len) = (meta.modified().ok()?, meta.len());
    if let Some(hit) = CATALOG_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|cached| cached.path == path && cached.mtime == mtime && cached.len == len)
    {
        return Some(hit.value.clone());
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let value: std::sync::Arc<serde_json::Value> =
        std::sync::Arc::new(serde_json::from_str(&text).ok()?);
    CATALOG_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(CachedCatalog {
            path,
            mtime,
            len,
            value: value.clone(),
        });
    Some(value)
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

/// Endpoint URL serving `model` per the models.dev catalog, for bare model
/// picks (`/model kimi-k2.6` with no `endpoint/` prefix). The catalog entry's
/// `api` URL is matched against the provider's endpoint table, so a catalog
/// rename can't silently misroute. Returns `None` (keep the current URL)
/// when the current endpoint already serves the model, the model is unknown,
/// or the current URL is custom (not a known endpoint — explicit wins).
/// ponytail: linear scan of a cached 4MB parse; runs on `/model` switches
/// and on every config rebuild (`from_env` runs per daemon chat turn) —
/// cheap behind the cached parse + endpoint guard.
fn catalog_endpoint_for_model(
    catalog: &serde_json::Value,
    model: &str,
    endpoints: &BTreeMap<String, String>,
    current_base_url: &str,
) -> Option<String> {
    if !endpoints.values().any(|url| url == current_base_url) {
        return None;
    }
    let needle = model.to_ascii_lowercase();
    let serves = |entry: &serde_json::Value| {
        entry
            .get("models")
            .and_then(|m| m.as_object())
            .is_some_and(|models| models.keys().any(|id| id.to_ascii_lowercase() == needle))
    };
    let mut fallback = None;
    // api.json shape nests providers at the top level; catalog.json shape
    // nests them under `providers` (a top-level `models` dict has no
    // `models` child per entry, so it is skipped by `serves`).
    let mut entries: Vec<&serde_json::Value> = Vec::new();
    if let Some(obj) = catalog.as_object() {
        entries.extend(obj.values());
    }
    if let Some(obj) = catalog.get("providers").and_then(|p| p.as_object()) {
        entries.extend(obj.values());
    }
    for entry in entries {
        let url = entry.get("api").and_then(|v| v.as_str()).unwrap_or("");
        if !endpoints.values().any(|known| known == url) || !serves(entry) {
            continue;
        }
        if url == current_base_url {
            return None;
        }
        if fallback.is_none() {
            fallback = Some(url.to_string());
        }
    }
    fallback
}

/// First `cost` object for `needle` among catalog providers accepted by
/// `pick`, in the catalog's provider order.
fn catalog_cost<'a>(
    catalog: &'a serde_json::Value,
    needle: &str,
    pick: impl Fn(&str, &serde_json::Value) -> bool,
) -> Option<&'a serde_json::Value> {
    let providers = catalog.as_object()?;
    for (key, entry) in providers {
        if !pick(key, entry) {
            continue;
        }
        if let Some(models) = entry.get("models").and_then(|m| m.as_object()) {
            if let Some(m) = models.get(needle).or_else(|| {
                models
                    .iter()
                    .find(|(k, _)| k.to_ascii_lowercase() == needle)
                    .map(|(_, v)| v)
            }) {
                if let Some(c) = m.get("cost") {
                    return Some(c);
                }
            }
        }
    }
    None
}

/// Cost for one LLM call, using models.dev pricing when available. The same
/// model id is listed by many resellers at different prices, so the entry
/// whose `api` matches the configured endpoint wins, then any catalog entry
/// of the configured provider, then any provider. `input`/`cache_read`/
/// `output` are USD per 1M tokens in the catalog; cache-write tokens are not
/// reported by the OpenAI-compatible endpoints dex speaks, so there is no
/// cache-write term. Cache hits and a missing output rate both fall back to
/// the full input price.
pub(crate) fn usage_cost(
    model: &str,
    provider: &Provider,
    base_url: &str,
    usage: &crate::core::types::Usage,
) -> Option<f64> {
    let catalog = load_dex_catalog()?;
    let needle = model.to_ascii_lowercase();
    let keys = provider.catalog_keys();
    let cost_val = catalog_cost(&catalog, &needle, |_, entry| {
        entry.get("api").and_then(|v| v.as_str()) == Some(base_url)
    })
    .or_else(|| catalog_cost(&catalog, &needle, |key, _| keys.iter().any(|k| k == key)))
    .or_else(|| catalog_cost(&catalog, &needle, |_, _| true))?;
    let input_rate = cost_val
        .get("input")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let cache_read_rate = cost_val
        .get("cache_read")
        .or_else(|| cost_val.get("cacheRead"))
        .and_then(|v| v.as_f64())
        .unwrap_or(input_rate);
    let output_rate = cost_val
        .get("output")
        .and_then(|v| v.as_f64())
        .unwrap_or(input_rate);
    let cached = usage.cached_tokens.unwrap_or(0).min(usage.prompt_tokens);
    let fresh = usage.prompt_tokens.saturating_sub(cached);
    #[allow(clippy::cast_precision_loss)]
    let total = fresh as f64 * input_rate / 1_000_000.0
        + cached as f64 * cache_read_rate / 1_000_000.0
        + usage.completion_tokens as f64 * output_rate / 1_000_000.0;
    Some(total)
}

fn load_dex_models_cache() -> Option<Vec<String>> {
    // dex cache is models.dev api.json — expose bare ids plus endpoint-qualified
    // variants (`zen/<id>`, `go/<id>`) so a pick names the endpoint it targets;
    // the prefixes are exactly the names `apply_model` routes on.
    if let Some(catalog) = load_dex_catalog() {
        if let Some(providers) = catalog.as_object() {
            let mut ids: Vec<String> = Vec::new();
            // Endpoint-qualified prefixes: builtins map to their named
            // endpoints, configured generic providers to their own name.
            let configured: BTreeMap<String, String> = load_provider_entries(&load_config_file())
                .into_keys()
                .map(|n| (n.clone(), n))
                .collect();
            for (prov_key, entry) in providers {
                if let Some(models) = entry.get("models").and_then(|m| m.as_object()) {
                    let dex_prefix: Option<String> = match prov_key.as_str() {
                        "opencode" => Some("zen".to_string()),
                        "opencode-go" => Some("go".to_string()),
                        "openai-codex" | "codex" => Some("openai-codex".to_string()),
                        other => configured.get(other).cloned(),
                    };
                    for id in models.keys() {
                        ids.push(id.clone());
                        if let Some(prefix) = &dex_prefix {
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
        // Refresh the slim context index too so the next launch skips the
        // 4MB catalog parse.
        if let Some(catalog) = load_dex_catalog() {
            write_ctx_index(&build_ctx_map(&catalog));
            if let Some(ids) = load_dex_models_cache() {
                let _ = ids; // already derived from catalog
            }
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
        .unwrap_or_else(|| provider.default_base_url().unwrap_or_default().to_string());
    let api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .ok_or("models.dev unavailable and OPENAI_API_KEY not set for fallback")?;
    let endpoints: BTreeMap<String, String> = if base_url.starts_with("https://opencode.ai/") {
        provider.endpoints()
    } else {
        BTreeMap::new()
    };
    let mut all: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let extra_headers = custom_headers_config_and_env();
    for id in fetch_provider_models(&client, &provider, &base_url, &api_key, &extra_headers) {
        if seen.insert(id.clone()) {
            all.push(id);
        }
    }
    for (name, url) in &endpoints {
        for id in fetch_provider_models(&client, &provider, url, &api_key, &extra_headers) {
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
    provider: &Provider,
    base_url: &str,
    api_key: &str,
    extra_headers: &BTreeMap<String, String>,
) -> Vec<String> {
    if !provider.has_model_listing() {
        return Vec::new();
    }
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut request = client.get(url).bearer_auth(api_key);
    for (name, value) in extra_headers {
        if name.eq_ignore_ascii_case("authorization") {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            request = request.header(name, value);
        }
    }
    let parsed: Result<ModelsResponse, _> = request
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

/// Parse one custom-header value into `name -> value` pairs.
///
/// Accepts a JSON object (`{"X-Foo":"bar"}` — pi's `headers` map / codex
/// `http_headers` shape) or `Name: Value` / `Name=Value` pairs separated by
/// commas or newlines (Claude Code's `ANTHROPIC_CUSTOM_HEADERS` shape).
/// Entries without a name, without a separator, or with an empty value are
/// skipped; later entries win on duplicate names (case-insensitive, last
/// casing wins). `authorization` is dropped (the api key owns it) and a
/// `{...}` value that isn't a JSON object falls back to pair parsing instead
/// of silently yielding nothing.
pub(crate) fn parse_headers_str(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return out;
    }
    if trimmed.starts_with('{') {
        if let Ok(serde_json::Value::Object(map)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
        {
            for (key, value) in map {
                let name = key.trim().to_string();
                if name.is_empty() {
                    continue;
                }
                let val = match &value {
                    serde_json::Value::String(s) => s.trim().to_string(),
                    serde_json::Value::Number(_) | serde_json::Value::Bool(_) => value.to_string(),
                    _ => continue,
                };
                if val.is_empty() {
                    continue;
                }
                insert_extra_header(&mut out, &name, &val);
            }
            return out;
        }
        // Not a JSON object (`{bad`, a JSON array, …) — fall through to
        // pair parsing instead of silently dropping everything. Outer braces
        // are stripped so `{X-Foo: bar}` still yields `X-Foo`.
    }
    let mut body = trimmed;
    if body.starts_with('{') {
        body = body
            .strip_prefix('{')
            .unwrap_or(body)
            .strip_suffix('}')
            .unwrap_or(body)
            .trim();
        if body.is_empty() {
            return out;
        }
    }
    // Newline-separated values may themselves contain commas, so only split
    // on commas when the value is a single line.
    let pieces: Vec<&str> = if body.contains('\n') {
        body.split('\n').collect()
    } else {
        body.split(',').collect()
    };
    for piece in pieces {
        let piece = piece.trim().trim_end_matches(',').trim();
        if piece.is_empty() {
            continue;
        }
        let split = piece.split_once(':').or_else(|| piece.split_once('='));
        let Some((name, value)) = split else {
            continue;
        };
        let name = name.trim().to_string();
        let value = value.trim().to_string();
        insert_extra_header(&mut out, &name, &value);
    }
    out
}

/// Scalar YAML value as a header value string (`"abc"`, `42`, `true`).
/// Empty strings yield `None` so blank entries are skipped.
fn yaml_scalar_str(v: &serde_yaml::Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| v.as_u64().map(|n| n.to_string()))
        .or_else(|| v.as_i64().map(|n| n.to_string()))
        .or_else(|| v.as_f64().map(|n| n.to_string()))
        .or_else(|| v.as_bool().map(|b| b.to_string()))
        .filter(|s| !s.is_empty())
}

/// Insert one header with case-insensitive "later wins" semantics: a later
/// `x-foo` replaces an earlier `X-Foo` (last casing wins). Empty names/values
/// and `authorization` (the api key owns that) are skipped so a bad entry
/// can never poison the map — send-time filtering remains as defense in depth.
pub(crate) fn insert_extra_header(out: &mut BTreeMap<String, String>, name: &str, value: &str) {
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() || value.is_empty() {
        return;
    }
    if name.eq_ignore_ascii_case("authorization") {
        return;
    }
    if let Some(existing) = out.keys().find(|k| k.eq_ignore_ascii_case(name)).cloned() {
        if existing != name {
            out.remove(&existing);
        }
    }
    out.insert(name.to_string(), value.to_string());
}

fn insert_config_header(out: &mut BTreeMap<String, String>, name: &str, value: &str) {
    insert_extra_header(out, name, value);
}

fn merge_config_headers_map(out: &mut BTreeMap<String, String>, map: &serde_yaml::Mapping) {
    for (k, v) in map {
        let name = k.as_str().unwrap_or_default();
        if let Some(val) = yaml_scalar_str(v) {
            insert_config_header(out, name, &val);
        }
    }
}

/// Custom headers from one config file key. Accepts a mapping (pi/codex
/// style), a text-header block (`"X-Foo: bar\nX-Baz: qux"`, same syntax as
/// the env vars / `--header`), or a list mixing both. Later entries win.
fn config_headers_map(file: &Option<serde_yaml::Value>, key: &str) -> BTreeMap<String, String> {
    let Some(value) = file.as_ref().and_then(|f| f.get(key)) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    if let Some(map) = value.as_mapping() {
        merge_config_headers_map(&mut out, map);
    } else if let Some(text) = value.as_str() {
        for (k, v) in parse_headers_str(text) {
            insert_extra_header(&mut out, &k, &v);
        }
    } else if let Some(items) = value.as_sequence() {
        for item in items {
            if let Some(map) = item.as_mapping() {
                merge_config_headers_map(&mut out, map);
            } else if let Some(text) = item.as_str() {
                for (k, v) in parse_headers_str(text) {
                    insert_extra_header(&mut out, &k, &v);
                }
            }
        }
    }
    out
}

/// Custom headers from the config file. `headers:` (pi) wins per-key over
/// `http_headers:` (codex) when both set the same name.
fn load_config_headers(file: &Option<serde_yaml::Value>) -> BTreeMap<String, String> {
    let mut out = config_headers_map(file, "http_headers");
    for (k, v) in config_headers_map(file, "headers") {
        insert_extra_header(&mut out, &k, &v);
    }
    out
}

/// Custom headers from the environment. Later sources win per-key:
/// `ANTHROPIC_CUSTOM_HEADERS` (claude) < `OPENAI_HEADERS` < `DEX_HEADERS`.
pub(crate) fn custom_headers_from_env() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for key in ["ANTHROPIC_CUSTOM_HEADERS", "OPENAI_HEADERS", "DEX_HEADERS"] {
        if let Ok(raw) = env::var(key) {
            for (k, v) in parse_headers_str(&raw) {
                insert_extra_header(&mut out, &k, &v);
            }
        }
    }
    out
}

/// Config-file + env headers (file < env), for paths without a full
/// `LlmConfig` (e.g. the `/models` refresh). Case-insensitive later-wins.
pub(crate) fn custom_headers_config_and_env() -> BTreeMap<String, String> {
    let mut out = load_config_headers(&load_config_file());
    for (k, v) in custom_headers_from_env() {
        insert_extra_header(&mut out, &k, &v);
    }
    out
}

#[derive(Clone)]
pub(crate) struct LlmConfig {
    pub(crate) provider: Provider,
    pub(crate) api_key: String,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) available_models: Vec<String>,
    pub(crate) endpoints: BTreeMap<String, String>,
    /// Configured generic providers (`providers:` map): the deposit place for
    /// per-provider keys plus optional base_url/`api:` overrides.
    pub(crate) provider_entries: BTreeMap<String, ProviderEntry>,
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
    /// Extra HTTP headers sent on every provider request (gateway auth,
    /// routing, attribution). Config `headers:`/`http_headers:` < env
    /// (`ANTHROPIC_CUSTOM_HEADERS`/`OPENAI_HEADERS`/`DEX_HEADERS`) <
    /// `--header` / per-request overrides. Never carries `authorization`
    /// (the api key owns that) — it is dropped at send time.
    pub(crate) extra_headers: BTreeMap<String, String>,
    pub(crate) client: reqwest::blocking::Client,
}

impl LlmConfig {
    pub(crate) fn from_env(
        base_url_override: Option<String>,
        model_override: Option<String>,
        permission_override: Option<PermissionMode>,
        header_overrides: &[String],
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
        // Custom provider headers: config file < env < CLI flags.
        let mut extra_headers = load_config_headers(&file);
        for (k, v) in custom_headers_from_env() {
            insert_extra_header(&mut extra_headers, &k, &v);
        }
        for raw in header_overrides {
            for (k, v) in parse_headers_str(raw) {
                insert_extra_header(&mut extra_headers, &k, &v);
            }
        }
        let provider_name = env::var("DEX_PROVIDER")
            .ok()
            .or_else(|| load_config_str(&file, "provider"))
            .unwrap_or_else(|| "opencode".to_string());
        let provider_entries = load_provider_entries(&file);
        let known: BTreeSet<String> = provider_entries.keys().cloned().collect();
        let provider = Provider::parse_known(&provider_name, &known).ok_or_else(|| {
            format!(
                "unsupported provider '{provider_name}'; use opencode, openai-codex or a providers: entry"
            )
        })?;
        let model = model_override
            .or_else(|| env::var("OPENAI_MODEL").ok())
            .or_else(|| load_config_str(&file, "model"))
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
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
        let file_base_url = load_config_str(&file, "base_url");
        // Any explicit base_url (CLI flag, OPENAI_BASE_URL, file `base_url:`)
        // pins the endpoint — routing may not silently rewire what the user
        // set. Only the fallback default (nothing set anywhere) routes.
        let explicit_base_url =
            base_url_override.is_some() || env_base_url.is_some() || file_base_url.is_some();
        // Landing endpoint for a bare first pick: builtin default, or for a
        // generic provider its config entry / catalog `api` URL. An empty
        // landing means the generic provider has no known endpoint — fail
        // loudly instead of pointing requests at "".
        let landing = landing_base_url_for(&provider, &provider_entries);
        let base_url = base_url_override
            .or(env_base_url)
            .or(file_base_url)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| landing.clone().unwrap_or_default());
        if base_url.is_empty() {
            return Err(format!(
                "provider '{}' has no base_url: set base_url under providers: or run `dex update --models`",
                provider.name()
            )
            .into());
        }
        // Wire protocol default: OPENAI_API pins everything, else the
        // active provider's `api:` entry pin, else the global file `api:`,
        // else responses.
        let api_name = env::var("OPENAI_API")
            .ok()
            .or_else(|| {
                provider_entries
                    .get(provider.name())
                    .and_then(|e| e.api)
                    .map(|a| a.name().to_string())
            })
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
        // only the default. `OPENAI_API` pins everything, otherwise the
        // `apply_model` call below resolves the selection-aware protocol
        // (full `endpoint/id` key, then bare id, then learned fallback).
        let (api_key, account_id) = resolve_credentials(
            &provider,
            &provider_entries,
            load_config_str(&file, "api_key").as_deref(),
        )?;
        let context_window = env::var("DEX_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
            // Slim cross-process index first (KBs); the 4MB catalog parse
            // is the cold-path fallback, which then refreshes the index.
            .or_else(|| ctx_from_index(&model))
            .or_else(|| {
                load_dex_catalog().and_then(|c| {
                    let ctx = catalog_context_window(&model, &c);
                    ensure_ctx_index(&c);
                    ctx
                })
            })
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        // Pi: reserve 16384, keep 20000 tokens recent (not 12 messages)
        let reserve_tokens = env::var("DEX_RESERVE_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(16_384);
        let keep_recent_tokens = env::var("DEX_KEEP_RECENT_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20_000);
        let connect_secs: u64 = env::var("DEX_HTTP_CONNECT_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        let request_secs: u64 = env::var("DEX_HTTP_REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        // The default-timeouts path (every daemon turn + TUI launch) reuses
        // the process-wide client instead of re-initializing TLS + pool.
        // Custom timeouts still build a dedicated client.
        let client = if connect_secs == 10 && request_secs == 300 {
            crate::client::http::shared_blocking_client()
        } else {
            reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(connect_secs))
                // Note: for the blocking client this deadline applies to the
                // connect and to each individual body read (not to the whole
                // streamed response), so long-lived SSE streams are safe.
                .timeout(Duration::from_secs(request_secs))
                .build()?
        };
        // Dex standalone: no network at startup — models come from config/DEX_MODELS
        // or `dex update --models` cache (XDG_DATA_HOME/dex/models.json). Removed
        // live /models fetch (was 5s+ blocking per endpoint).
        // Always expose zen/go for OpenCode so /model can set base_url without env
        let endpoints: BTreeMap<String, String> = endpoints_for(&provider, &provider_entries);
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
            extra_headers,
            client,
            endpoints,
            provider_entries,
        };
        // A prefixed model override (--model go/foo or the daemon's
        // per-request model) routes to its endpoint; bare ids keep the
        // resolved base_url. An explicit base_url (--base-url,
        // OPENAI_BASE_URL, file `base_url:`) wins over routing.
        if !explicit_base_url {
            this.apply_model(&model, false);
        }
        Ok(this)
    }

    /// Wire protocol for a fresh model selection: an explicit per-model
    /// table entry wins, otherwise a previously learned fallback for this
    /// `(base_url, model)`, otherwise the configured default is kept
    /// (`None`). `OPENAI_API` pins everything and skips this entirely — the
    /// caller keeps `self.api` untouched then.
    fn resolve_model_api(&self, selection: &str) -> Option<ApiProtocol> {
        if env::var("OPENAI_API").is_ok() {
            return None;
        }
        if let Some(api) = model_api_from_env(selection, &self.model) {
            return Some(api);
        }
        learned_api(&self.base_url, &self.model)
    }

    /// Apply a `/model` selection. `provider/model` switches provider (and its
    /// base_url) when `provider` parses as a known provider; `endpoint/model`
    /// routes to the named endpoint's base_url; a bare id keeps the current
    /// endpoint when it serves the model (per the models.dev catalog) and
    /// moves to the serving endpoint otherwise — so the pick, not the user,
    /// owns the base_url. The wire protocol follows the same way
    /// (`DEX_MODEL_APIS`, then learned, then the global default) with the
    /// empirical responses→completions fallback as the last resort.
    /// Provider prefix is stripped first, then endpoint routing runs on the
    /// remainder. Returns the endpoint name when an endpoint route was
    /// taken. With `persist`, the selection is written back to the config
    /// file (it becomes the new default).
    pub(crate) fn apply_model(&mut self, selection: &str, persist: bool) -> Option<String> {
        let prev_model = self.model.clone();
        let prev_provider = self.provider.clone();
        // Provider-qualified: "opencode/gpt-..." or "openai-codex/gpt-..." sets base_url without env
        let mut sel = selection;
        if let Some((prefix, rest)) = selection.split_once('/') {
            let known: BTreeSet<String> = self.provider_entries.keys().cloned().collect();
            if let Some(new_provider) = Provider::parse_known(prefix, &known) {
                if new_provider != self.provider {
                    // Best-effort credential switch; failure surfaces at next LLM call
                    if let Ok((k, acct)) =
                        resolve_credentials(&new_provider, &self.provider_entries, None)
                    {
                        self.api_key = k;
                        self.account_id = acct;
                    }
                    // Switch even without creds so /model shows intent;
                    // auth failure surfaces at the next LLM call. Generic
                    // providers land on their config entry / catalog URL.
                    let landing = landing_base_url_for(&new_provider, &self.provider_entries)
                        .unwrap_or_else(|| self.base_url.clone());
                    let new_endpoints = endpoints_for(&new_provider, &self.provider_entries);
                    self.provider = new_provider;
                    self.base_url = landing;
                    self.endpoints = new_endpoints;
                }
                sel = rest;
            }
        }
        let mut result = None;
        let mut routed = false;
        if let Some((name, rest)) = sel.split_once('/') {
            if let Some(url) = self.endpoints.get(name).cloned() {
                self.base_url = url;
                self.model = rest.to_string();
                result = Some(name.to_string());
                routed = true;
            }
        }
        if !routed {
            // Bare id (possibly with provider-native slashes like
            // `moonshotai/kimi-k2.6`): stay unless the catalog shows the
            // model lives on another known endpoint.
            self.model = sel.to_string();
            if let Some(catalog) = load_dex_catalog() {
                if let Some(url) = catalog_endpoint_for_model(
                    &catalog,
                    &self.model,
                    &self.endpoints,
                    &self.base_url,
                ) {
                    result = self
                        .endpoints
                        .iter()
                        .find_map(|(name, known)| (*known == url).then(|| name.clone()));
                    self.base_url = url;
                }
            }
        }
        // The model carries its wire protocol; the global `api` is the
        // fallback and `OPENAI_API` pins it.
        if let Some(api) = self.resolve_model_api(selection) {
            self.api = api;
        }
        if persist && (self.model != prev_model || self.provider != prev_provider) {
            persist_selection(selection, &self.provider, &self.base_url);
        }
        // Dex standalone: contextWindow from models.dev catalog > provider default; refresh unless env pinned it.
        if env::var("DEX_CONTEXT_WINDOW").is_err() {
            if let Some(catalog) = load_dex_catalog() {
                if let Some(ctx) = catalog_context_window(&self.model, &catalog) {
                    self.context_window = ctx;
                } else {
                    self.context_window = DEFAULT_CONTEXT_WINDOW;
                }
            } else {
                self.context_window = DEFAULT_CONTEXT_WINDOW;
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
        provider: &Provider,
        persist: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (api_key, account_id) = resolve_credentials(provider, &self.provider_entries, None)?;
        let env_base_url = env::var("OPENAI_BASE_URL").ok().filter(|v| !v.is_empty());
        let landing = match provider {
            Provider::Generic(name) => self
                .provider_entries
                .get(name)
                .and_then(|e| e.base_url.clone())
                .or_else(|| load_dex_catalog().and_then(|c| catalog_api(name, &c)))
                .ok_or_else(|| {
                    format!(
                        "provider '{name}' has no base_url: set it under providers: or run `dex update --models`"
                    )
                })?,
            _ => provider.default_base_url().unwrap_or_default().to_string(),
        };
        self.provider = provider.clone();
        self.api_key = api_key;
        self.account_id = account_id;
        self.base_url = env_base_url.unwrap_or(landing);
        self.endpoints = endpoints_for(&self.provider, &self.provider_entries);
        // Wire protocol: OPENAI_API pins everything, else the provider's
        // `api:` entry pin, else the global file `api:`, else responses —
        // then the current model's own resolution on top.
        let api_name = env::var("OPENAI_API")
            .ok()
            .or_else(|| {
                self.provider_entries
                    .get(provider.name())
                    .and_then(|e| e.api)
                    .map(|a| a.name().to_string())
            })
            .or_else(|| load_config_str(&load_config_file(), "api"));
        self.api = api_name
            .as_deref()
            .and_then(ApiProtocol::parse)
            .unwrap_or(ApiProtocol::Responses);
        // Keep the current model's own protocol when the global `api` is not
        // explicitly pinned via `OPENAI_API`.
        let current = self.model.clone();
        if let Some(api) = self.resolve_model_api(&current) {
            self.api = api;
        }
        if persist {
            persist_selection(&self.model, provider, &self.base_url);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        build_ctx_map, detect_verify_command, load_config_file, load_dex_models_cache,
        load_provider_entries, model_api_from_env, reasoning_options_for, remember_learned_api,
        usage_cost, ApiProtocol, LlmConfig, ModelsResponse, PermissionMode, Provider,
    };
    use crate::core::types::Usage;
    use std::env;

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
            extra_headers: Default::default(),
            client: reqwest::blocking::Client::new(),
            provider_entries: Default::default(),
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

    /// Synthetic models.dev catalog: the same id priced differently per
    /// provider, with the endpoint-exact entry NOT first alphabetically.
    fn write_cost_catalog(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("dex/models.dev.json"),
            serde_json::json!({
                "aaa-reseller": {
                    "api": "https://aaa.example/v1",
                    "models": {
                        "m-1": { "cost": { "input": 2.0 } },
                        "m-2": { "cost": { "input": 4.0 } }
                    }
                },
                "opencode": {
                    "api": "https://zen.example/v1",
                    "models": { "m-1": { "cost": { "input": 0.5 } } }
                },
                "opencode-go": {
                    "api": "https://go.example/v1",
                    "models": {
                        "m-1": { "cost": { "input": 0.25, "cache_read": 0.0625, "output": 2.0 } }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn usage_cost_prefers_endpoint_then_provider_pricing() {
        // Serializes process-env redirection against other tests.
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-cost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_cost_catalog(&dir);
        let prev_cache = env::var_os("XDG_CACHE_HOME");
        env::set_var("XDG_CACHE_HOME", &dir);
        let usage = |prompt: u64, completion: u64, cached: Option<u64>| Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: cached,
        };

        // Endpoint-exact match wins even though "aaa-reseller" sorts first.
        assert_eq!(
            usage_cost(
                "m-1",
                &Provider::OpenCode,
                "https://go.example/v1",
                &usage(1_000_000, 0, None)
            ),
            Some(0.25)
        );
        // Fresh + cached + output are each billed at their own rate:
        // 0.6*0.25 + 0.4*0.0625 + 100k*2.0/1M.
        assert_eq!(
            usage_cost(
                "m-1",
                &Provider::OpenCode,
                "https://go.example/v1",
                &usage(1_000_000, 100_000, Some(400_000))
            ),
            Some(0.375)
        );
        // No endpoint match: the configured provider's catalog keys win over
        // the global scan (0.5, not aaa-reseller's 2.0).
        assert_eq!(
            usage_cost(
                "m-1",
                &Provider::OpenCode,
                "https://unrelated.example/v1",
                &usage(1_000_000, 0, None)
            ),
            Some(0.5)
        );
        // A catalog entry without an output rate bills output at the input
        // rate: 1M*0.5 + 1M*0.5.
        assert_eq!(
            usage_cost(
                "m-1",
                &Provider::OpenCode,
                "https://unrelated.example/v1",
                &usage(1_000_000, 1_000_000, None)
            ),
            Some(1.0)
        );
        // Model only listed by another provider: global fallback still prices it.
        assert_eq!(
            usage_cost(
                "m-2",
                &Provider::OpenAiCodex,
                "https://x.example/v1",
                &usage(1_000_000, 0, None)
            ),
            Some(4.0)
        );

        match prev_cache {
            Some(v) => env::set_var("XDG_CACHE_HOME", v),
            None => env::remove_var("XDG_CACHE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
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
            extra_headers: Default::default(),
            client: reqwest::blocking::Client::new(),
            provider_entries: Default::default(),
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
            Some("https://opencode.ai/zen/v1")
        );
        assert_eq!(
            Provider::OpenAiCodex.default_base_url(),
            Some("https://chatgpt.com/backend-api/codex")
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
        assert_eq!(
            cfg.base_url,
            Provider::OpenAiCodex.default_base_url().unwrap()
        );
        assert_eq!(cfg.model, "gpt-5.6-luna");
        // Switch back via alias "openai"
        cfg.apply_model("openai/gpt-4o", false);
        assert_eq!(cfg.provider, Provider::OpenCode);
        assert_eq!(cfg.base_url, Provider::OpenCode.default_base_url().unwrap());
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
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.base_url, Provider::OpenCode.default_base_url().unwrap());
        assert!(cfg.endpoints.contains_key("go"));
        assert!(cfg.endpoints.contains_key("zen"));
    }

    #[test]
    fn custom_headers_parse_json_and_pairs() {
        use super::parse_headers_str;
        let json = parse_headers_str(r#"{"X-Gateway-Key":"abc","X-Empty":"","num":42}"#);
        assert_eq!(json.get("X-Gateway-Key").map(String::as_str), Some("abc"));
        assert_eq!(json.get("num").map(String::as_str), Some("42"));
        assert!(!json.contains_key("X-Empty"), "empty values are skipped");
        assert!(parse_headers_str("  ").is_empty());

        let pairs = parse_headers_str("X-Foo: bar, X-Baz=qux");
        assert_eq!(pairs.get("X-Foo").map(String::as_str), Some("bar"));
        assert_eq!(pairs.get("X-Baz").map(String::as_str), Some("qux"));

        // Newline-separated pairs may contain commas in values.
        let multi = parse_headers_str("X-A: one, two\nX-B: three");
        assert_eq!(multi.get("X-A").map(String::as_str), Some("one, two"));
        assert_eq!(multi.get("X-B").map(String::as_str), Some("three"));

        // Malformed entries are skipped, later duplicates win.
        let messy = parse_headers_str("no-separator, : novalue, X-K: 1, X-K: 2");
        assert_eq!(messy.len(), 1);
        assert_eq!(messy.get("X-K").map(String::as_str), Some("2"));

        // Case-insensitive duplicates collapse (last casing/value wins).
        let ci = parse_headers_str("X-Foo: 1, x-foo: 2");
        assert_eq!(ci.len(), 1);
        assert_eq!(ci.get("x-foo").map(String::as_str), Some("2"));

        // `authorization` never lands in the map (api key owns it).
        assert!(parse_headers_str("Authorization: hacked").is_empty());
        assert!(parse_headers_str(r#"{"authorization":"hacked"}"#).is_empty());

        // A `{...}` value that isn't a JSON object falls back to pairs.
        let fb = parse_headers_str("{bad json");
        assert!(fb.is_empty(), "no separator means no pairs either");
        let fb = parse_headers_str("{X-Foo: bar}");
        assert_eq!(fb.get("X-Foo").map(String::as_str), Some("bar"));
    }

    #[test]
    fn custom_headers_layer_file_env_cli() {
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
            "DEX_HEADERS",
            "OPENAI_HEADERS",
            "ANTHROPIC_CUSTOM_HEADERS",
        ]);
        // Config file: `headers:` (pi) wins per-key over `http_headers:` (codex).
        let dir = std::env::temp_dir().join(format!("dex-headers-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cfg_path = dir.join("config.yaml");
        std::fs::write(
            &cfg_path,
            "provider: opencode\nmodel: m-h\nhttp_headers:\n  X-File: file\n  X-Shared: codex\nheaders:\n  X-Shared: pi\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", &cfg_path);
        std::env::set_var("OPENAI_API_KEY", "test-key");
        std::env::remove_var("OPENAI_BASE_URL");
        std::env::remove_var("DEX_MODELS");
        std::env::set_var("DEX_HEADERS", "X-Env: env");
        std::env::set_var("OPENAI_HEADERS", "X-Env2: openai");
        std::env::set_var("ANTHROPIC_CUSTOM_HEADERS", "X-Env3: claude");
        let cfg = LlmConfig::from_env(
            None,
            None,
            None,
            &["X-Cli: cli".to_string(), "X-Shared: cli".to_string()],
        )
        .unwrap();
        assert_eq!(cfg.model, "m-h");
        assert_eq!(
            cfg.extra_headers.get("X-File").map(String::as_str),
            Some("file")
        );
        assert_eq!(
            cfg.extra_headers.get("X-Env").map(String::as_str),
            Some("env")
        );
        assert_eq!(
            cfg.extra_headers.get("X-Env2").map(String::as_str),
            Some("openai")
        );
        assert_eq!(
            cfg.extra_headers.get("X-Env3").map(String::as_str),
            Some("claude")
        );
        assert_eq!(
            cfg.extra_headers.get("X-Cli").map(String::as_str),
            Some("cli")
        );
        // CLI wins over both config-file spellings.
        assert_eq!(
            cfg.extra_headers.get("X-Shared").map(String::as_str),
            Some("cli")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_headers_config_text_and_list_shapes() {
        use super::load_config_headers;
        // Text-header block (same syntax as env vars / `--header`).
        let file: Option<serde_yaml::Value> =
            Some(serde_yaml::from_str("headers: \"X-A: one\\nX-B: two, three\"\n").unwrap());
        let out = load_config_headers(&file);
        assert_eq!(out.get("X-A").map(String::as_str), Some("one"));
        assert_eq!(out.get("X-B").map(String::as_str), Some("two, three"));
        // List mixing text entries and one-key maps; later entries win.
        let file: Option<serde_yaml::Value> = Some(
            serde_yaml::from_str("http_headers:\n  - \"X-A: 1\"\n  - X-A: 2\n  - X-C: 3\n")
                .unwrap(),
        );
        let out = load_config_headers(&file);
        assert_eq!(out.get("X-A").map(String::as_str), Some("2"));
        assert_eq!(out.get("X-C").map(String::as_str), Some("3"));
        // Non-string scalars and blank entries are skipped.
        let file: Option<serde_yaml::Value> =
            Some(serde_yaml::from_str("headers:\n  X-N: 42\n  X-E: \"\"\n").unwrap());
        let out = load_config_headers(&file);
        assert_eq!(out.get("X-N").map(String::as_str), Some("42"));
        assert!(!out.contains_key("X-E"));
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
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.model, "file-model");
        assert_eq!(cfg.base_url, "https://file.example/v1");
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        // Env model beats the file.
        std::env::set_var("OPENAI_MODEL", "env-model");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.model, "env-model");
        // Write-back: selection lands in the file, unknown keys survive.
        let mut cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
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
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Synthetic catalog where each model lives on exactly one endpoint,
    /// so bare picks must move `base_url` without any prefix knowledge.
    fn write_routing_catalog(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("dex/models.dev.json"),
            serde_json::json!({
                "opencode": {
                    "api": "https://opencode.ai/zen/v1",
                    "models": { "m-zen-only": { "limit": { "context": 1 } } }
                },
                "opencode-go": {
                    "api": "https://go.example/v1",
                    "models": { "m-go-only": { "limit": { "context": 1 } } }
                },
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn bare_model_auto_routes_to_serving_endpoint() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["XDG_CACHE_HOME", "DEX_MODEL_APIS", "OPENAI_API"]);
        let dir = std::env::temp_dir().join(format!("dex-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_routing_catalog(&dir);
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::remove_var("DEX_MODEL_APIS");
        std::env::remove_var("OPENAI_API");
        let mut cfg = test_cfg();
        cfg.endpoints
            .insert("go".to_string(), "https://go.example/v1".to_string());
        // Bare pick living on another endpoint moves base_url by itself.
        assert_eq!(cfg.apply_model("m-go-only", false).as_deref(), Some("go"));
        assert_eq!(cfg.base_url, "https://go.example/v1");
        assert_eq!(cfg.model, "m-go-only");
        // Back to a zen-only model.
        assert_eq!(cfg.apply_model("m-zen-only", false).as_deref(), Some("zen"));
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        // A model the current endpoint serves never moves — dual-served ids
        // must not ping-pong between endpoints.
        assert_eq!(cfg.apply_model("m-zen-only", false), None);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        // Unknown models and custom URLs stay put.
        assert_eq!(cfg.apply_model("m-unknown", false), None);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        cfg.base_url = "https://custom.example/v1".to_string();
        assert_eq!(cfg.apply_model("m-go-only", false), None);
        assert_eq!(cfg.base_url, "https://custom.example/v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_base_url_wins_over_catalog_routing() {
        // OPENAI_BASE_URL is a user pin even when it names a known endpoint:
        // a go-only model stays on the pinned URL instead of being silently
        // rerouted to the serving endpoint.
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
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_routing_catalog(&dir);
        std::fs::write(dir.join("config.yaml"), "provider: opencode\n").unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("OPENAI_API_KEY", "test-key");
        std::env::set_var("OPENAI_BASE_URL", "https://opencode.ai/zen/v1");
        std::env::set_var("OPENAI_MODEL", "m-go-only");
        for key in [
            "DEX_PROVIDER",
            "OPENAI_API",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
        ] {
            std::env::remove_var(key);
        }
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.model, "m-go-only");
        // The pin holds even though the catalog says m-go-only lives on go.
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Synthetic catalog with a third-party OpenAI-compatible provider
    /// ("zai") so the generic provider layer can be exercised end to end.
    fn write_generic_catalog(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("dex/models.dev.json"),
            serde_json::json!({
                "zai": {
                    "api": "https://api.zai.example/v4",
                    "env": { "ZAI_TEST_KEY": "z.ai api key" },
                    "models": {
                        "glm-x": {
                            "limit": { "context": 1 },
                            "reasoning_options": [
                                { "type": "effort", "values": ["low", "high"] }
                            ]
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    fn generic_config(dir: &std::path::Path, providers_yaml: &str) -> std::path::PathBuf {
        let path = dir.join("config.yaml");
        std::fs::write(&path, format!("provider: zai\n{providers_yaml}")).unwrap();
        path
    }

    #[test]
    fn generic_provider_resolves_endpoint_key_and_routing() {
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
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
            "ZAI_TEST_KEY",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-generic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_generic_catalog(&dir);
        generic_config(&dir, "providers:\n  zai:\n    api_key: zsk-deposit\n");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("OPENAI_MODEL", "glm-x");
        for key in [
            "DEX_PROVIDER",
            "OPENAI_API",
            "OPENAI_BASE_URL",
            "OPENAI_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "ZAI_TEST_KEY",
        ] {
            std::env::remove_var(key);
        }
        // Endpoint + key from the deposit place; catalog supplies the URL.
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.provider.name(), "zai");
        assert_eq!(cfg.base_url, "https://api.zai.example/v4");
        assert_eq!(cfg.api_key, "zsk-deposit");
        assert_eq!(
            cfg.endpoints.get("zai").map(String::as_str),
            Some("https://api.zai.example/v4")
        );
        // `/model zai/glm-x` is a no-op (already there); unknown prefixes
        // must still stay plain model ids.
        let mut cfg = cfg;
        assert!(cfg.apply_model("unknown/m", false).is_none());
        assert_eq!(cfg.model, "unknown/m");
        // Key falls back to the provider's own conventional env var.
        std::fs::write(
            dir.join("config.yaml"),
            "provider: zai\nproviders:\n  zai: {}\n",
        )
        .unwrap();
        std::env::set_var("ZAI_TEST_KEY", "zsk-from-env");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.api_key, "zsk-from-env");
        // No key anywhere: the error names both deposit places.
        std::env::remove_var("ZAI_TEST_KEY");
        let err = match LlmConfig::from_env(None, None, None, &[]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected missing-key error"),
        };
        assert!(err.contains("providers.zai.api_key"), "{err}");
        assert!(err.contains("ZAI_TEST_KEY"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generic_provider_switch_and_completion_ids() {
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
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-gswitch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_generic_catalog(&dir);
        generic_config(&dir, "providers:\n  zai:\n    api_key: zsk-deposit\n");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("OPENAI_API_KEY", "test-key");
        for key in [
            "DEX_PROVIDER",
            "OPENAI_API",
            "OPENAI_BASE_URL",
            "OPENAI_MODEL",
        ] {
            std::env::remove_var(key);
        }
        // Completion list offers the configured provider's qualified ids.
        let ids = load_dex_models_cache().unwrap();
        assert!(ids.contains(&"glm-x".to_string()));
        assert!(ids.contains(&"zai/glm-x".to_string()));
        // Provider-qualified pick switches to the generic provider.
        let mut cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert!(cfg.apply_model("zai/glm-x", false).is_none()); // already on zai
        assert_eq!(cfg.model, "glm-x");
        // From opencode, the same pick switches provider AND endpoint.
        let mut cfg = test_cfg();
        cfg.provider_entries = load_provider_entries(&load_config_file());
        assert_eq!(cfg.apply_model("zai/glm-x", false), None);
        assert_eq!(cfg.provider.name(), "zai");
        assert_eq!(cfg.base_url, "https://api.zai.example/v4");
        // Thinking options come from the catalog for the selected model.
        assert_eq!(
            reasoning_options_for("glm-x"),
            Some(vec!["low".to_string(), "high".to_string()])
        );
        assert_eq!(reasoning_options_for("m-unknown"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn models_cache_offers_endpoint_qualified_ids() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
        let dir = std::env::temp_dir().join(format!("dex-mlist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_routing_catalog(&dir);
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let ids = load_dex_models_cache().unwrap();
        // Bare ids for every catalog model, plus endpoint-qualified variants
        // so a pick can name its endpoint explicitly (`zen/…` vs `go/…`).
        assert!(ids.contains(&"m-zen-only".to_string()));
        assert!(ids.contains(&"zen/m-zen-only".to_string()));
        assert!(ids.contains(&"m-go-only".to_string()));
        assert!(ids.contains(&"go/m-go-only".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefixed_selection_restores_protocol_on_restart() {
        // `model: go/<id>` in the config file must honor a bare-id
        // `DEX_MODEL_APIS` entry: the full selection key is tried first,
        // then the stripped id (previously both lookups used the raw
        // `go/<id>` string, so bare entries never matched after restart).
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
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "provider: opencode\nmodel: go/m-z9\n",
        )
        .unwrap();
        // Empty catalog dir: no routing interference, unknown model stays.
        std::fs::create_dir_all(dir.join("cache/dex")).unwrap();
        std::fs::write(dir.join("cache/dex/models.dev.json"), "{}").unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        std::env::set_var("OPENAI_API_KEY", "test-key");
        std::env::set_var("DEX_MODEL_APIS", "m-z9=openai-completions");
        std::env::remove_var("OPENAI_API");
        std::env::remove_var("OPENAI_BASE_URL");
        std::env::remove_var("OPENAI_MODEL");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.model, "m-z9");
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctx_map_covers_both_catalog_shapes_lowercased() {
        let catalog = serde_json::json!({
            "opencode": { "models": {
                "GPT-5": { "limit": { "context": 128000 } },
                "zero": { "limit": { "context": 0 } },
                "nodoc": {},
            } },
            "models": {
                "Claude-X": { "limit": { "context": 200000 } },
            },
        });
        let map = build_ctx_map(&catalog);
        assert_eq!(map.get("gpt-5"), Some(&128000));
        assert_eq!(map.get("claude-x"), Some(&200000));
        assert!(!map.contains_key("zero"));
        assert!(!map.contains_key("nodoc"));
    }
}
