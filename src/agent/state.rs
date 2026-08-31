use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnLimits {
    pub elapsed_seconds: u64,
}

pub(crate) trait CancellationSource {
    fn is_cancelled(&self) -> bool;
    fn take_cancelled(&self) -> bool;
}

/// Process-global cancellation (Ctrl+C) used by the non-TUI paths
/// (`oye "prompt"` and `oye --tool`). The TUI/daemon paths use a per-session
/// `CancellationToken` instead, so a cancel never leaks across sessions.
#[derive(Clone)]
pub(crate) struct GlobalCancellation;

impl CancellationSource for GlobalCancellation {
    fn is_cancelled(&self) -> bool {
        crate::core::console::is_interrupted()
    }
    fn take_cancelled(&self) -> bool {
        crate::core::console::take_interrupt()
    }
}

pub(crate) const CACHE_FILE_NAME: &str = "oye-tool-cache.json";

pub(crate) fn cache_file_path() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(dir).join(CACHE_FILE_NAME));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache").join(CACHE_FILE_NAME))
}

pub(crate) fn cache_fingerprint(name: &str, input: &str) -> String {
    let mut fingerprint = String::new();
    if matches!(name, "read" | "grep" | "find") {
        if let Ok(args) = serde_json::from_str::<Value>(input) {
            if let Some(path) = args.get("path").and_then(Value::as_str) {
                if let Ok(meta) = fs::metadata(path) {
                    fingerprint = format!(
                        ":{}:{}",
                        meta.len(),
                        meta.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos())
                            .unwrap_or_default()
                    );
                }
            }
        }
    }
    fingerprint
}

#[derive(Default)]
pub(crate) struct ToolState {
    pub(crate) cache: HashMap<String, String>,
    pub(crate) dirty: bool,
    /// Last API-reported prompt token count for the main conversation.
    pub(crate) last_usage: Option<u64>,
}

impl ToolState {
    pub(crate) fn load() -> Self {
        let mut state = Self::default();
        // Cached tool output can contain source code or secrets. Keep caching
        // opt-in until a caller explicitly requests it.
        if env::var("OYE_TOOL_CACHE").as_deref() != Ok("1") {
            return state;
        }
        if let Some(path) = cache_file_path() {
            if let Ok(contents) = fs::read_to_string(&path) {
                if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&contents) {
                    state.cache = map;
                }
            }
        }
        state
    }

    pub(crate) fn insert(&mut self, key: String, value: String) {
        self.cache.insert(key, value);
        self.dirty = true;
    }

    pub(crate) fn clear(&mut self) {
        if !self.cache.is_empty() {
            self.cache.clear();
            self.dirty = true;
        }
    }

    /// Persist the cache to disk (best-effort; failures are ignored).
    pub(crate) fn save(&self) {
        if !self.dirty || env::var("OYE_TOOL_CACHE").as_deref() != Ok("1") {
            return;
        }
        if let Some(path) = cache_file_path() {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string(&self.cache) {
                let _ = fs::write(&path, json);
            }
        }
    }
}
