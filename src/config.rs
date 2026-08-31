use std::env;
use std::path::PathBuf;

/// Resolve configuration using the documented XDG/legacy precedence.
///
/// The preferred location is `oye/config.json`; if it does not exist we fall
/// back to the pre-rename `ak/config.json` location so existing setups keep
/// working after the rename.
pub(crate) fn path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("OYE_CONFIG") {
        return Some(PathBuf::from(path));
    }
    let (primary, legacy) = match env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => (
            PathBuf::from(&dir).join("oye/config.json"),
            PathBuf::from(&dir).join("ak/config.json"),
        ),
        None => {
            let home = env::var_os("HOME")?;
            (
                PathBuf::from(&home).join(".config/oye/config.json"),
                PathBuf::from(&home).join(".config/ak/config.json"),
            )
        }
    };
    Some(if primary.exists() { primary } else { legacy })
}
