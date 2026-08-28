use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Deserialize)]
pub(crate) struct CodexAuthFile {
    pub(crate) tokens: Option<CodexTokens>,
}

#[derive(Deserialize)]
pub(crate) struct CodexTokens {
    pub(crate) access_token: String,
    pub(crate) account_id: Option<String>,
}

pub(crate) fn load_codex_credentials(
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    if let Some(access_token) = env::var_os("CODEX_ACCESS_TOKEN") {
        let access_token = access_token.to_string_lossy().trim().to_string();
        if !access_token.is_empty() {
            return Ok((
                access_token,
                env::var("CODEX_ACCOUNT_ID")
                    .ok()
                    .filter(|id| !id.is_empty()),
            ));
        }
    }
    let path = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .ok_or("HOME is not set; cannot locate Codex credentials")?
        .join("auth.json");
    let contents = fs::read_to_string(&path)
        .map_err(|e| format!("could not read Codex credentials {}: {}", path.display(), e))?;
    let auth: CodexAuthFile = serde_json::from_str(&contents)
        .map_err(|e| format!("invalid Codex credentials {}: {}", path.display(), e))?;
    let tokens = auth
        .tokens
        .ok_or("Codex auth.json has no OAuth tokens; run `codex --login`")?;
    if tokens.access_token.trim().is_empty() {
        return Err("Codex auth.json contains an empty access token".into());
    }
    Ok((tokens.access_token, tokens.account_id))
}
