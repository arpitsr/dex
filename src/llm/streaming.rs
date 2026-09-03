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

/// Wire protocol for this call: an `OPENAI_API` pin or an explicit
/// `DEX_MODEL_APIS` entry always wins (both are already baked into
/// `config.api`); otherwise a learned fallback overrides the configured
/// default (config file `api:` / `openai-responses`).
fn effective_api(config: &LlmConfig) -> ApiProtocol {
    if std::env::var("OPENAI_API").is_ok()
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
    if std::env::var("OPENAI_API").is_ok() {
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
                Err(e) if try_responses_fallback(config, &e.to_string()) => {
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
            let _env = EnvGuard::clear(&["OPENAI_API", "DEX_MODEL_APIS"]);
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
}
