use std::env;
use std::path::PathBuf;

/// Resolve configuration using the documented XDG/legacy precedence.
pub(crate) fn path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("RUSTY_PI_CONFIG") {
        return Some(PathBuf::from(path));
    }
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("ak/config.json"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/ak/config.json"))
}
