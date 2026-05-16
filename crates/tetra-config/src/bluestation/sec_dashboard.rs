use serde::Deserialize;
use std::collections::HashMap;
use toml::Value;

use super::SecretField;

/// Dashboard HTTP server configuration
#[derive(Debug, Clone)]
pub struct CfgDashboard {
    /// Port to listen on (default: 8080)
    pub port: u16,
    /// Bind address (default: 127.0.0.1)
    pub bind: String,
    /// Enable GET/POST /api/config. Disabled by default because configs may contain secrets.
    pub allow_config_api: bool,
    /// Enable WebSocket control commands such as kick, restart, shutdown and SDS.
    pub allow_control_commands: bool,
    /// Optional bearer token required for config API and control WebSocket access.
    pub auth_token: Option<SecretField>,
}

impl Default for CfgDashboard {
    fn default() -> Self {
        Self {
            port: 8080,
            bind: "127.0.0.1".to_string(),
            allow_config_api: false,
            allow_control_commands: false,
            auth_token: None,
        }
    }
}

#[derive(Deserialize)]
pub struct CfgDashboardDto {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default)]
    pub allow_config_api: bool,
    #[serde(default)]
    pub allow_control_commands: bool,
    pub auth_token: Option<String>,

    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

fn default_port() -> u16 {
    8080
}
fn default_bind() -> String {
    "127.0.0.1".to_string()
}

pub fn apply_dashboard_patch(src: CfgDashboardDto) -> Result<CfgDashboard, String> {
    if src.port == 0 {
        return Err("dashboard: port cannot be 0".to_string());
    }
    let auth_token = src.auth_token.and_then(empty_to_none);
    if let Some(ref token) = auth_token {
        if token.len() < 16 || !token.chars().all(is_url_safe_token_char) {
            return Err("dashboard: auth_token must be at least 16 URL-safe characters (A-Z a-z 0-9 - . _ ~)".to_string());
        }
    }
    let auth_token = auth_token.map(SecretField::from);
    if (src.allow_config_api || src.allow_control_commands) && auth_token.is_none() {
        return Err("dashboard: allow_config_api/allow_control_commands requires auth_token".to_string());
    }
    Ok(CfgDashboard {
        port: src.port,
        bind: src.bind,
        allow_config_api: src.allow_config_api,
        allow_control_commands: src.allow_control_commands,
        auth_token,
    })
}

fn empty_to_none(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

fn is_url_safe_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_' | '~')
}
