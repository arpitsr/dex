//! Shared streaming boundary. The concrete SSE readers remain compatible with
//! both provider protocols and are called through these typed entry points.

use crate::core::types::{ApiProtocol, ChatMessage, Provider, SinkLine, Usage};
use crate::llm::config::LlmConfig;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Models empirically switched to chat-completions after the responses API
/// rejected them (e.g. glm-5.3-flash on zen/go 500s on `/responses`, 200s on
/// `/chat/completions`). Keyed by (base_url, model); per-process, so the
/// one-time cost of learning is a single failed call per model per run.
/// ponytail: in-memory only — re-learned on restart; persist to the cache dir
/// if cold-start latency for completions-only models ever matters.
fn probed_apis() -> &'static Mutex<HashMap<(String, String), ApiProtocol>> {
    static MAP: OnceLock<Mutex<HashMap<(String, String), ApiProtocol>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A failure raised *after* the provider began streaming the `/responses`
/// reply (dropped SSE connection, malformed chunk, cancellation). Purely a
/// type-level marker: `Display` passes the inner message through untouched
/// so callers matching on `"cancelled"` etc. keep working.
#[derive(Debug)]
pub(crate) struct MidStreamError(pub String);

impl std::fmt::Display for MidStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MidStreamError {}

/// Only a pre-stream rejection (HTTP status, connect failure) qualifies for
/// protocol fallback; a mid-stream failure may have already put partial text
/// on the transcript, and a retried call would duplicate it.
fn is_mid_stream(err: &(dyn std::error::Error + 'static)) -> bool {
    err.downcast_ref::<MidStreamError>().is_some()
}

/// Wire protocol for this call: an explicit pin (`OPENAI_API` or config-file
/// `api:`) or a `DEX_MODEL_APIS` entry always wins (all baked into
/// `config.api`); otherwise a learned fallback overrides the configured
/// default (`openai-responses`).
fn effective_api(config: &LlmConfig) -> ApiProtocol {
    if crate::llm::config::api_pinned()
        || crate::llm::config::model_api_from_env(&config.model, &config.model).is_some()
    {
        return config.api;
    }
    probed_apis()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(config.base_url.clone(), config.model.clone()))
        .copied()
        .unwrap_or(config.api)
}

/// May we infer the protocol by retrying a failed `/responses` call as
/// chat-completions? Only when nothing explicitly pinned the protocol, the
/// provider exposes both wire shapes, and the failure isn't a cancellation.
fn try_responses_fallback(config: &LlmConfig, err: &str) -> bool {
    if crate::llm::config::api_pinned() {
        return false; // user pinned one protocol for everything
    }
    if crate::llm::config::model_api_from_env(&config.model, &config.model).is_some() {
        return false; // explicit per-model table entry
    }
    if config.provider != Provider::OpenCode {
        return false; // codex backend-api has no /chat/completions
    }
    !err.contains("cancelled")
}

pub(crate) fn complete(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<::std::sync::mpsc::Sender<crate::core::types::SinkLine>>,
    cancel: &dyn crate::agent::state::CancellationSource,
) -> Result<(ChatMessage, Option<Usage>), Box<dyn std::error::Error>> {
    match effective_api(config) {
        ApiProtocol::ChatCompletions => {
            crate::llm::chat_completions::complete(config, messages, with_tools, sink, cancel)
        }
        ApiProtocol::Responses => {
            match crate::llm::responses::complete(
                config,
                messages,
                with_tools,
                sink.clone(),
                cancel,
            ) {
                Ok(ok) => Ok(ok),
                Err(e) if !is_mid_stream(&*e) && try_responses_fallback(config, &e.to_string()) => {
                    // Empirical protocol inference: responses API rejected the
                    // model — try chat-completions once and remember.
                    match crate::llm::chat_completions::complete(
                        config,
                        messages,
                        with_tools,
                        sink.clone(),
                        cancel,
                    ) {
                        Ok(ok) => {
                            probed_apis()
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(
                                    (config.base_url.clone(), config.model.clone()),
                                    ApiProtocol::ChatCompletions,
                                );
                            // Survive restarts / one-shot runs.
                            crate::llm::config::remember_learned_api(
                                &config.base_url,
                                &config.model,
                                ApiProtocol::ChatCompletions,
                            );
                            if let Some(sink) = &sink {
                                sink.send(SinkLine::System(format!(
                                    "auto: {} speaks openai-completions (responses API failed); remembered for future runs",
                                    config.model
                                )))
                                .ok();
                            }
                            Ok(ok)
                        }
                        // The original responses error is the canonical one.
                        Err(_) => Err(e),
                    }
                }
                Err(e) => Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::config::tests::test_cfg;

    /// Set/restore env around gate tests (local copy of config's EnvRestore).
    struct EnvGuard {
        prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn clear(keys: &[&'static str]) -> Self {
            let prev = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
            for k in keys {
                std::env::remove_var(k);
            }
            Self { prev }
        }

        /// Point `key` at `value`, restoring the previous value on drop.
        fn set(mut self, key: &'static str, value: &std::path::Path) -> Self {
            self.prev.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
            self
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in self.prev.drain(..) {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn fallback_gate_and_learned_protocol() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut cfg = test_cfg();
        {
            // Hermetic: no real env pins and no developer config.yaml.
            let absent = std::env::temp_dir().join("dex-gate-test-absent.yaml");
            let _env =
                EnvGuard::clear(&["OPENAI_API", "DEX_MODEL_APIS"]).set("DEX_CONFIG", &absent);
            // Unpinned opencode model: fallback allowed.
            assert!(try_responses_fallback(&cfg, "500 Internal server error"));
            // Never on cancellation.
            assert!(!try_responses_fallback(&cfg, "cancelled"));
            // OPENAI_API pins the protocol — no inference.
            std::env::set_var("OPENAI_API", "openai-responses");
            assert!(!try_responses_fallback(&cfg, "500 boom"));
            std::env::remove_var("OPENAI_API");
            // Explicit DEX_MODEL_APIS entry — user already decided.
            std::env::set_var("DEX_MODEL_APIS", "m-r=openai-responses");
            assert!(!try_responses_fallback(&cfg, "500 boom"));
            std::env::remove_var("DEX_MODEL_APIS");
            // A config-file `api:` pin counts too.
            let pin_dir =
                std::env::temp_dir().join(format!("dex-gate-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&pin_dir);
            std::fs::create_dir_all(&pin_dir).unwrap();
            std::fs::write(pin_dir.join("config.yaml"), "api: openai-responses\n").unwrap();
            std::env::set_var("DEX_CONFIG", pin_dir.join("config.yaml"));
            assert!(!try_responses_fallback(&cfg, "500 boom"));
            std::env::set_var("DEX_CONFIG", &absent);
            let _ = std::fs::remove_dir_all(&pin_dir);
            // Codex backend has no /chat/completions.
            cfg.provider = Provider::OpenAiCodex;
            assert!(!try_responses_fallback(&cfg, "500 boom"));
            cfg.provider = Provider::OpenCode;
            // Learned protocol overrides the configured default.
            assert_eq!(effective_api(&cfg), ApiProtocol::Responses);
            probed_apis()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    (cfg.base_url.clone(), cfg.model.clone()),
                    ApiProtocol::ChatCompletions,
                );
            assert_eq!(effective_api(&cfg), ApiProtocol::ChatCompletions);
            // But an explicit table entry still wins over what we learned.
            std::env::set_var("DEX_MODEL_APIS", "m-r=openai-responses");
            assert_eq!(effective_api(&cfg), ApiProtocol::Responses);
        }
        probed_apis()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    #[test]
    fn mid_stream_marker_blocks_fallback_and_keeps_message() {
        let err: Box<dyn std::error::Error> = Box::new(MidStreamError("cancelled".into()));
        assert!(is_mid_stream(&*err));
        // Display passes the message through untouched so callers matching on
        // `"cancelled"` (exact or substring) keep working.
        assert_eq!(err.to_string(), "cancelled");
        let plain: Box<dyn std::error::Error> = "API error: boom".into();
        assert!(!is_mid_stream(&*plain));
    }
}
