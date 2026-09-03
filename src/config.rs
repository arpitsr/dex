use std::env;
use std::path::PathBuf;

/// Resolve configuration using the documented XDG/legacy precedence.
///
/// YAML is the preferred format (`dex/config.yaml`); `.yml` and the old JSON
/// file still parse (JSON is a subset of YAML). The pre-rename `ak/config.json`
/// location is kept as a final fallback so existing setups keep working.
pub(crate) fn path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("DEX_CONFIG") {
        return Some(PathBuf::from(path));
    }
    let dir = match env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env::var_os("HOME")?).join(".config"),
    };
    for name in [
        "dex/config.yaml",
        "dex/config.yml",
        "dex/config.json",
        "ak/config.json",
    ] {
        let path = dir.join(name);
        if path.exists() {
            return Some(path);
        }
    }
    Some(dir.join("dex/config.yaml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn path_resolves_dex_config_xdg_then_home() {
        let prev_dex = env::var_os("DEX_CONFIG");
        let prev_xdg = env::var_os("XDG_CONFIG_HOME");
        let prev_home = env::var_os("HOME");

        env::set_var("DEX_CONFIG", "/tmp/custom-dex-config.yaml");
        assert_eq!(path().unwrap(), PathBuf::from("/tmp/custom-dex-config.yaml"));

        env::remove_var("DEX_CONFIG");
        env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-test");
        // nothing exists -> new default location
        assert_eq!(path().unwrap(), PathBuf::from("/tmp/xdg-test/dex/config.yaml"));

        // legacy JSON and ak/ locations are still found, in precedence order
        env::set_var("HOME", "/tmp/fakehome");
        std::fs::create_dir_all("/tmp/xdg-test/ak").unwrap();
        std::fs::write("/tmp/xdg-test/ak/config.json", "{}").unwrap();
        assert_eq!(path().unwrap(), PathBuf::from("/tmp/xdg-test/ak/config.json"));
        let _ = std::fs::remove_dir_all("/tmp/xdg-test");

        env::remove_var("XDG_CONFIG_HOME");
        assert_eq!(
            path().unwrap(),
            PathBuf::from("/tmp/fakehome/.config/dex/config.yaml")
        );

        match prev_dex {
            Some(v) => env::set_var("DEX_CONFIG", v),
            None => env::remove_var("DEX_CONFIG"),
        }
        match prev_xdg {
            Some(v) => env::set_var("XDG_CONFIG_HOME", v),
            None => env::remove_var("XDG_CONFIG_HOME"),
        }
        match prev_home {
            Some(v) => env::set_var("HOME", v),
            None => env::remove_var("HOME"),
        }
    }
}
