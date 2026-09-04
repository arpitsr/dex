//! Per-provider wiring. Every behavior that differs between the supported
//! providers is a method or constant here, so adding a third provider means
//! editing this file — not chasing match arms across the tree. Identity
//! (`parse`/`name`) stays on the enum in `core::types`.

use std::collections::BTreeMap;

use crate::core::types::Provider;
use crate::llm::auth::load_codex_credentials;

/// Model used when `OPENAI_MODEL`, config file and `--model` are all unset.
/// Both supported providers list it.
pub(crate) const DEFAULT_MODEL: &str = "gpt-5.6-luna";

/// Context-window fallback when `DEX_CONTEXT_WINDOW` is unset and the
/// models.dev catalog has no entry for the model.
pub(crate) const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

impl Provider {
    /// Base URL when `OPENAI_BASE_URL`, `--base-url` and config-file
    /// `base_url` are all unset.
    pub(crate) fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenCode => "https://api.openai.com/v1",
            Self::OpenAiCodex => "https://chatgpt.com/backend-api/codex",
        }
    }

    /// Named endpoints offered to `/model` routing (`zen/<id>`, `go/<id>`).
    pub(crate) fn endpoints(self) -> BTreeMap<String, String> {
        match self {
            // ponytail: static table, add dynamic registry if more than
            // 3 providers/endpoints
            Self::OpenCode => [
                ("zen", "https://opencode.ai/zen/v1"),
                ("go", "https://opencode.ai/zen/go/v1"),
            ]
            .into_iter()
            .map(|(name, url)| (name.to_string(), url.to_string()))
            .collect(),
            Self::OpenAiCodex => BTreeMap::new(),
        }
    }

    /// models.dev catalog keys that can serve this provider (pricing lookup).
    pub(crate) fn catalog_keys(self) -> &'static [&'static str] {
        match self {
            Self::OpenCode => &["opencode", "opencode-go", "openai"],
            Self::OpenAiCodex => &["openai-codex", "codex"],
        }
    }

    /// Does the endpoint expose OpenAI-compatible `GET /models`?
    pub(crate) fn has_model_listing(self) -> bool {
        matches!(self, Self::OpenCode)
    }

    /// Does the endpoint also speak chat-completions, so a rejected
    /// `/responses` call may be retried there? (Empirical protocol fallback.)
    pub(crate) fn has_protocol_fallback(self) -> bool {
        matches!(self, Self::OpenCode)
    }

    /// Credentials for this provider: `(api_key, account_id)`.
    /// `file_api_key` is the config-file `api_key` fallback (OpenCode only;
    /// codex reads its own credential file and ignores it).
    pub(crate) fn load_credentials(
        self,
        file_api_key: Option<&str>,
    ) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
        match self {
            Self::OpenCode => Ok((
                std::env::var("OPENAI_API_KEY")
                    .ok()
                    .or_else(|| file_api_key.map(str::to_string))
                    .ok_or("OPENAI_API_KEY not set (export it or add it to your shell profile)")?,
                None,
            )),
            Self::OpenAiCodex => load_codex_credentials(),
        }
    }

    /// Can a 401 be recovered by re-reading credentials from their source?
    /// Codex tokens are file-based and can be refreshed by another process;
    /// env-provided keys are static within a run.
    pub(crate) fn credentials_refreshable(self) -> bool {
        matches!(self, Self::OpenAiCodex)
    }

    /// Extra headers for every request (codex backend-api wants the
    /// originator marker and the account id).
    pub(crate) fn auth_headers(self, account_id: Option<&str>) -> Vec<(&'static str, String)> {
        match self {
            Self::OpenCode => Vec::new(),
            Self::OpenAiCodex => {
                let mut headers = vec![("originator", "codex_cli_rs".to_string())];
                if let Some(account_id) = account_id {
                    headers.push(("ChatGPT-Account-ID", account_id.to_string()));
                }
                headers
            }
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
        assert!(Provider::OpenCode.has_model_listing());
        assert!(!Provider::OpenAiCodex.has_model_listing());
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
            "https://api.openai.com/v1"
        );
        assert_eq!(
            Provider::OpenAiCodex.default_base_url(),
            "https://chatgpt.com/backend-api/codex"
        );
    }
}
