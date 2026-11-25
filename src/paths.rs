use std::path::PathBuf;

/// Returns the root Noren configuration directory (`%APPDATA%\Noren` by default,
/// or `NOREN_CONFIG_DIR` when overridden for isolated testing).
pub fn noren_config_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("NOREN_CONFIG_DIR") {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("Noren");
    }
    PathBuf::from("Noren")
}

/// Shared theme and general settings file (`%APPDATA%\Noren\general.toml`).
pub fn general_toml_path() -> PathBuf {
    noren_config_dir().join("general.toml")
}

/// Directory storing installed `noren-module` manifest files (`%APPDATA%\Noren\manifests`).
pub fn manifests_dir() -> PathBuf {
    noren_config_dir().join("manifests")
}

/// Per-module config directory (`%APPDATA%\Noren\<module>`).
pub fn module_config_dir(module: &str) -> PathBuf {
    noren_config_dir().join(module)
}

/// Per-module config file path (`%APPDATA%\Noren\<module>\<config_file>`).
pub fn module_config_path(module: &str, config_file: &str) -> PathBuf {
    module_config_dir(module).join(config_file)
}

/// Named-pipe prefix (`"noren."` by default, or `NOREN_PIPE_PREFIX` for isolated tests).
pub fn pipe_prefix() -> String {
    if let Ok(prefix) = std::env::var("NOREN_PIPE_PREFIX") {
        let trimmed = prefix.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "noren.".to_string()
}

/// Named mutex used to elect the single Noren Tray Host (`Local\noren-tray-owner-<user-sid>`).
/// Isolated automatically when `NOREN_TRAY_MUTEX_NAME` or a non-default `NOREN_PIPE_PREFIX` is set.
pub fn tray_mutex_name(user_sid: &str) -> String {
    if let Ok(override_name) = std::env::var("NOREN_TRAY_MUTEX_NAME") {
        let trimmed = override_name.trim();
        if !trimmed.is_empty() {
            return format!("Local\\{}", trimmed.trim_start_matches("Local\\"));
        }
    }
    let prefix = pipe_prefix();
    if prefix != "noren." {
        let sanitized: String = prefix
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        return format!("Local\\{sanitized}tray-owner-{user_sid}");
    }
    format!("Local\\noren-tray-owner-{user_sid}")
}
