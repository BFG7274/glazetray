//! Configuration loading with graceful degradation (missing file = defaults,
//! invalid file = keep last-good and surface the error).

use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;

pub const CONFIG_DIR_REL: &str = ".glzr\\glazetray";
pub const CONFIG_FILE: &str = "config.yaml";
#[allow(dead_code)]
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", default)]
pub struct Config {
    pub glazewm: GlazeWmConfig,
    pub tray: TrayConfig,
    pub flyout: FlyoutConfig,
    pub startup: StartupConfig,
    pub logging: LoggingConfig,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", default)]
pub struct GlazeWmConfig {
    pub url: String,
    pub reconnect_initial_ms: u64,
    pub reconnect_max_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", default)]
pub struct TrayConfig {
    pub show_direction: bool,
    pub use_system_accent: bool,
    pub scroll_switch_workspace: bool,
}

impl Default for TrayConfig {
    fn default() -> Self {
        TrayConfig {
            show_direction: true,
            use_system_accent: true,
            scroll_switch_workspace: false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", default)]
pub struct FlyoutConfig {
    pub width: f32,
    pub show_empty_workspaces: bool,
    pub close_on_workspace_switch: bool,
    pub animation: String, // "system" | "on" | "off"
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", default)]
pub struct StartupConfig {
    pub launch_with_windows: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case", default)]
pub struct LoggingConfig {
    pub level: String,
    pub retention_days: u32,
}

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("配置文件不存在，使用内置默认值")]
    Missing,
    #[error("配置文件无效: {0}")]
    Invalid(String),
    #[error("读取配置文件失败: {0}")]
    Io(String),
}

impl Default for Config {
    fn default() -> Self {
        Config {
            glazewm: GlazeWmConfig {
                url: "ws://127.0.0.1:6123".into(),
                reconnect_initial_ms: 250,
                reconnect_max_ms: 10_000,
            },
            tray: TrayConfig {
                show_direction: true,
                use_system_accent: true,
                scroll_switch_workspace: false,
            },
            flyout: FlyoutConfig {
                width: 460.0,
                show_empty_workspaces: true,
                close_on_workspace_switch: false,
                animation: "system".into(),
            },
            startup: StartupConfig {
                launch_with_windows: true,
            },
            logging: LoggingConfig {
                level: "info".into(),
                retention_days: 7,
            },
        }
    }
}

pub fn config_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(CONFIG_DIR_REL)
}

pub fn config_path() -> PathBuf {
    config_dir().join(CONFIG_FILE)
}

pub fn default_config_yaml() -> String {
    r#"# GlazeTray 配置
# 默认值即为此文件内容；删除文件后使用内置默认值。

glazewm:
  url: "ws://127.0.0.1:6123"
  reconnect-initial-ms: 250
  reconnect-max-ms: 10000

tray:
  show-direction: true
  use-system-accent: true
  scroll-switch-workspace: false

flyout:
  width: 460
  show-empty-workspaces: true
  close-on-workspace-switch: false
  animation: "system"     # system | on | off

startup:
  launch-with-windows: true

logging:
  level: "info"
  retention-days: 7
"#
    .to_string()
}

/// Load config. `Ok(None)` means the file is missing (caller keeps defaults).
pub fn load_config(path: &std::path::Path) -> Result<Config, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::Missing);
    }
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
    let cfg: Config =
        serde_yaml_ng::from_str(&text).map_err(|e| ConfigError::Invalid(e.to_string()))?;
    Ok(cfg)
}

/// Write the default config file (creating the directory). No-op if it exists.
pub fn ensure_default_config(path: &std::path::Path) -> std::io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, default_config_yaml())?;
    Ok(true)
}

pub fn animation_enabled(cfg: &Config, system_animations: bool) -> bool {
    match cfg.flyout.animation.as_str() {
        "off" => false,
        "on" => true,
        _ => system_animations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default() {
        let cfg: Config = serde_yaml_ng::from_str(&default_config_yaml()).unwrap();
        assert_eq!(cfg.glazewm.url, "ws://127.0.0.1:6123");
        assert_eq!(cfg.flyout.width, 460.0);
        assert!(cfg.tray.use_system_accent);
        assert_eq!(cfg.logging.retention_days, 7);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        let cfg: Config = serde_yaml_ng::from_str("tray:\n  show-direction: false\n").unwrap();
        assert_eq!(cfg.glazewm.url, "ws://127.0.0.1:6123");
        assert!(!cfg.tray.show_direction);
        assert!(cfg.tray.use_system_accent);
    }

    #[test]
    fn invalid_yaml_is_an_error() {
        let err = serde_yaml_ng::from_str::<Config>("flyout: [not, a, map");
        assert!(err.is_err());
    }
}
