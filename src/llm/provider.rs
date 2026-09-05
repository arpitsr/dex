//! Per-provider wiring. Behavior that differs between builtin providers is a
//! method here; configured generic providers (`providers:` map in config.yaml)
//! resolve everything wire-shape-specific through `LlmConfig` + the models.dev
//! catalog instead — bearer auth, catalog `api` endpoint, empirical protocol.
//! Identity (`parse`/`name`) stays on the enum in `core::types`.

use std::collections::BTreeMap;

use crate::core::types::Provider;

/// Model used when `OPENAI_MODEL`, config file and `--model` are all unset.
pub(crate) const DEFAULT_MODEL: &str = "gpt-5.6-luna";

/// Context-window fallback when `DEX_CONTEXT_WINDOW` is unset and the
/// models.dev catalog has no entry for the model.
pub(crate) const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

impl Provider {
    /// Base URL when `OPENAI_BASE_URL`, `--base-url` and config-file
    /// `base_url` are all unset. Bare model picks route themselves to the
    /// right endpoint via the models.dev catalog (see `apply_model`), so
    /// this is just the landing endpoint, not a per-model decision. Generic
    /// providers land on their catalog `api` URL instead (`None` here;
    /// `LlmConfig::landing_base_url` resolves it).
    pub(crate) fn default_base_url(&self) -> Option<&'static str> {
        match self {
            Self::OpenCode => Some("https://opencode.ai/zen/v1"),
            Self::OpenAiCodex => Some("https://chatgpt.com/backend-api/codex"),
            Self::Generic(_) => None,
        }
    }

    /// Named endpoints offered to `/model` routing (`zen/<id>`, `go/<id>`).
    /// Generic providers carry a single implicit endpoint (their catalog
    /// `api` URL, named after the provider); `LlmConfig` injects it.
    pub(crate) fn endpoints(&self) -> BTreeMap<String, String> {
        match self {
            // ponytail: static table, add dynamic registry if more than
            // 3 builtin endpoints
            Self::OpenCode => [
                ("zen", "https://opencode.ai/zen/v1"),
                ("go", "https://opencode.ai/zen/go/v1"),
            ]
            .into_iter()
            .map(|(name, url)| (name.to_string(), url.to_string()))
            .collect(),
            // Generic endpoints live in LlmConfig (catalog/config-derived).
            _ => BTreeMap::new(),
        }
    }

    /// models.dev catalog keys that can serve this provider (pricing lookup).
    pub(crate) fn catalog_keys(&self) -> Vec<String> {
        match self {
            Self::OpenCode => ["opencode", "opencode-go", "openai"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            Self::OpenAiCodex => ["openai-codex", "codex"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            // The catalog entry of the same key prices a generic provider.
            Self::Generic(name) => vec![name.clone()],
        }
    }

    /// Does the endpoint also speak chat-completions, so a rejected
    /// `/responses` call may be retried there? (Empirical protocol fallback.)
    pub(crate) fn has_protocol_fallback(&self) -> bool {
        !matches!(self, Self::OpenAiCodex)
    }

    /// Can a 401 be recovered by re-reading credentials from their source?
    /// Codex tokens are file-based and can be refreshed by another process;
    /// env/config keys are static within a run.
    pub(crate) fn credentials_refreshable(&self) -> bool {
        matches!(self, Self::OpenAiCodex)
    }

    /// Extra headers for every request (codex backend-api wants the
    /// originator marker and the account id; generic providers are plain
    /// bearer — no per-provider header table until one is actually needed).
    pub(crate) fn auth_headers(&self, account_id: Option<&str>) -> Vec<(&'static str, String)> {
        match self {
            Self::OpenAiCodex => {
                let mut headers = vec![("originator", "codex_cli_rs".to_string())];
                if let Some(account_id) = account_id {
                    headers.push(("ChatGPT-Account-ID", account_id.to_string()));
                }
                headers
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_spec_tables() {
        assert_eq!(
            Provider::OpenCode.endpoints().get("go").map(String::as_str),
            Some("https://opencode.ai/zen/go/v1")
        );
        assert!(Provider::OpenAiCodex.endpoints().is_empty());
        assert!(Provider::OpenCode.has_protocol_fallback());
        assert!(!Provider::OpenAiCodex.has_protocol_fallback());
        assert!(Provider::OpenAiCodex.credentials_refreshable());
        assert!(!Provider::OpenCode.credentials_refreshable());
        assert_eq!(
            Provider::OpenAiCodex.auth_headers(Some("acct")),
            vec![
                ("originator", "codex_cli_rs".to_string()),
                ("ChatGPT-Account-ID", "acct".to_string())
            ]
        );
        assert!(Provider::OpenCode.auth_headers(Some("acct")).is_empty());
        assert_eq!(
            Provider::OpenCode.default_base_url(),
            Some("https://opencode.ai/zen/v1")
        );
        assert_eq!(
            Provider::OpenAiCodex.default_base_url(),
            Some("https://chatgpt.com/backend-api/codex")
        );
        assert_eq!(Provider::Generic("zai".into()).default_base_url(), None);
        // Pricing: a generic provider prices via its own catalog entry.
        assert_eq!(
            Provider::Generic("zai".into()).catalog_keys(),
            vec!["zai".to_string()]
        );
    }
}
