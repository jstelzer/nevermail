use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::core::keyring;

/// Runtime config — password is always a plain String in memory.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub imap_server: String,
    pub imap_port: u16,
    pub username: String,
    pub password: String,
    pub use_starttls: bool,
    pub email_addresses: Vec<String>,
}

/// On-disk representation. Password is either a keyring reference or plaintext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileConfig {
    pub server: String,
    pub port: u16,
    pub username: String,
    pub starttls: bool,
    pub password: PasswordBackend,
    #[serde(default)]
    pub email_addresses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "backend")]
pub enum PasswordBackend {
    #[serde(rename = "keyring")]
    Keyring,
    #[serde(rename = "plaintext")]
    Plaintext { value: String },
}

/// SMTP configuration derived from IMAP config + optional env overrides.
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub server: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub use_starttls: bool,
}

impl SmtpConfig {
    pub fn from_imap_config(config: &Config) -> Self {
        let server = std::env::var("NEVERMAIL_SMTP_SERVER")
            .unwrap_or_else(|_| config.imap_server.clone());
        let port = std::env::var("NEVERMAIL_SMTP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(587);
        SmtpConfig {
            server,
            port,
            username: config.username.clone(),
            password: config.password.clone(),
            use_starttls: true,
        }
    }
}

/// What the dialog needs to show when credentials can't be resolved automatically.
#[derive(Debug, Clone)]
pub enum ConfigNeedsInput {
    /// No config file exists — show full setup form.
    FullSetup,
    /// Config exists but password is missing from keyring.
    PasswordOnly {
        server: String,
        port: u16,
        username: String,
        starttls: bool,
        error: Option<String>,
    },
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nevermail")
        .join("config.json")
}

impl FileConfig {
    pub fn load() -> Result<Option<Self>, String> {
        let path = config_path();
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(&path).map_err(|e| format!("read config: {e}"))?;
        let cfg: FileConfig = serde_json::from_str(&data).map_err(|e| format!("parse config: {e}"))?;
        Ok(Some(cfg))
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create config dir: {e}"))?;
        }
        let data = serde_json::to_string_pretty(self).map_err(|e| format!("serialize config: {e}"))?;
        fs::write(&path, data).map_err(|e| format!("write config: {e}"))
    }
}

impl Config {
    /// Try env vars. Returns None if any required var is missing.
    pub fn from_env() -> Option<Self> {
        let imap_server = std::env::var("NEVERMAIL_SERVER").ok()?;
        let username = std::env::var("NEVERMAIL_USER").ok()?;
        let password = std::env::var("NEVERMAIL_PASSWORD").ok()?;
        let imap_port = std::env::var("NEVERMAIL_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(993);
        let use_starttls = std::env::var("NEVERMAIL_STARTTLS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        let email_addresses = std::env::var("NEVERMAIL_FROM")
            .ok()
            .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();

        Some(Config {
            imap_server,
            imap_port,
            username,
            password,
            use_starttls,
            email_addresses,
        })
    }

    /// Build runtime Config from a FileConfig + resolved password string.
    pub fn from_file_config(fc: &FileConfig, password: String) -> Self {
        Config {
            imap_server: fc.server.clone(),
            imap_port: fc.port,
            username: fc.username.clone(),
            password,
            use_starttls: fc.starttls,
            email_addresses: fc.email_addresses.clone(),
        }
    }

    /// Resolution order: env vars → config file + keyring → Err(ConfigNeedsInput).
    pub fn resolve() -> Result<Self, ConfigNeedsInput> {
        // 1. Env vars override everything
        if let Some(config) = Self::from_env() {
            log::info!("Config loaded from environment variables");
            return Ok(config);
        }

        // 2. Config file + keyring
        match FileConfig::load() {
            Ok(Some(fc)) => {
                match &fc.password {
                    PasswordBackend::Plaintext { value } => {
                        log::info!("Config loaded from file (plaintext password)");
                        Ok(Self::from_file_config(&fc, value.clone()))
                    }
                    PasswordBackend::Keyring => {
                        match keyring::get_password(&fc.username, &fc.server) {
                            Ok(pw) => {
                                log::info!("Config loaded from file + keyring");
                                Ok(Self::from_file_config(&fc, pw))
                            }
                            Err(e) => {
                                log::warn!("Keyring lookup failed: {}", e);
                                Err(ConfigNeedsInput::PasswordOnly {
                                    server: fc.server,
                                    port: fc.port,
                                    username: fc.username,
                                    starttls: fc.starttls,
                                    error: Some(format!("Keyring unavailable: {e}")),
                                })
                            }
                        }
                    }
                }
            }
            Ok(None) => {
                log::info!("No config file found, need full setup");
                Err(ConfigNeedsInput::FullSetup)
            }
            Err(e) => {
                log::warn!("Config file error: {}", e);
                Err(ConfigNeedsInput::FullSetup)
            }
        }
    }
}
