use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::core::keyring;

// ---------------------------------------------------------------------------
// AccountId — stable UUIDv4 per account
// ---------------------------------------------------------------------------

pub type AccountId = String;

pub fn new_account_id() -> AccountId {
    uuid::Uuid::new_v4().to_string()
}

/// Synthetic ID for env-var-based accounts (stable across restarts).
pub const ENV_ACCOUNT_ID: &str = "env-account";

// ---------------------------------------------------------------------------
// SMTP overrides — per-field optional, merged onto IMAP defaults
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SmtpOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<PasswordBackend>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_starttls: Option<bool>,
}

// ---------------------------------------------------------------------------
// On-disk per-account config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAccountConfig {
    pub id: AccountId,
    pub label: String,
    pub server: String,
    pub port: u16,
    pub username: String,
    pub starttls: bool,
    pub password: PasswordBackend,
    #[serde(default)]
    pub email_addresses: Vec<String>,
    #[serde(default)]
    pub smtp: SmtpOverrides,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "backend")]
pub enum PasswordBackend {
    #[serde(rename = "keyring")]
    Keyring,
    #[serde(rename = "plaintext")]
    Plaintext { value: String },
}

// ---------------------------------------------------------------------------
// Multi-account file config (new on-disk format)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiAccountFileConfig {
    pub accounts: Vec<FileAccountConfig>,
}

// ---------------------------------------------------------------------------
// Legacy single-account file config (for migration)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Runtime SMTP config (fully resolved)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub server: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub use_starttls: bool,
}

impl SmtpConfig {
    /// Resolve SMTP config: start from IMAP defaults, overlay SmtpOverrides.
    pub fn resolve(
        imap_server: &str,
        imap_username: &str,
        imap_password: &str,
        overrides: &SmtpOverrides,
        account_id: &str,
    ) -> Self {
        let server = overrides
            .server
            .clone()
            .unwrap_or_else(|| imap_server.to_string());
        let port = overrides.port.unwrap_or(587);
        let username = overrides
            .username
            .clone()
            .unwrap_or_else(|| imap_username.to_string());
        let password = match &overrides.password {
            Some(PasswordBackend::Plaintext { value }) => value.clone(),
            Some(PasswordBackend::Keyring) => {
                keyring::get_smtp_password(account_id).unwrap_or_else(|_| imap_password.to_string())
            }
            None => imap_password.to_string(),
        };
        let use_starttls = overrides.use_starttls.unwrap_or(true);

        SmtpConfig {
            server,
            port,
            username,
            password,
            use_starttls,
        }
    }

    /// Legacy: build from IMAP config with env var overrides (for env-var accounts).
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

// ---------------------------------------------------------------------------
// Runtime account config (resolved passwords, ready to use)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub imap_server: String,
    pub imap_port: u16,
    pub username: String,
    pub password: String,
    pub use_starttls: bool,
    pub email_addresses: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub id: AccountId,
    pub label: String,
    pub imap_server: String,
    pub imap_port: u16,
    pub username: String,
    pub password: String,
    pub use_starttls: bool,
    pub email_addresses: Vec<String>,
    pub smtp: SmtpConfig,
    pub smtp_overrides: SmtpOverrides,
}

impl AccountConfig {
    /// Build an AccountConfig from a FileAccountConfig + resolved password.
    pub fn from_file_account(fac: &FileAccountConfig, password: String) -> Self {
        let smtp = SmtpConfig::resolve(
            &fac.server,
            &fac.username,
            &password,
            &fac.smtp,
            &fac.id,
        );
        AccountConfig {
            id: fac.id.clone(),
            label: fac.label.clone(),
            imap_server: fac.server.clone(),
            imap_port: fac.port,
            username: fac.username.clone(),
            password,
            use_starttls: fac.starttls,
            email_addresses: fac.email_addresses.clone(),
            smtp,
            smtp_overrides: fac.smtp.clone(),
        }
    }

    /// Convert to a legacy Config for ImapSession::connect (temporary bridge).
    pub fn to_imap_config(&self) -> Config {
        Config {
            imap_server: self.imap_server.clone(),
            imap_port: self.imap_port,
            username: self.username.clone(),
            password: self.password.clone(),
            use_starttls: self.use_starttls,
            email_addresses: self.email_addresses.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// What the dialog needs to show when credentials can't be resolved
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// File paths
// ---------------------------------------------------------------------------

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nevermail")
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

// ---------------------------------------------------------------------------
// Layout config (unchanged)
// ---------------------------------------------------------------------------

/// Persisted pane layout ratios.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutConfig {
    /// Ratio of the outer split (sidebar vs rest). Default ~0.15.
    pub sidebar_ratio: f32,
    /// Ratio of the inner split (message list vs message view). Default ~0.40.
    pub list_ratio: f32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            sidebar_ratio: 0.15,
            list_ratio: 0.40,
        }
    }
}

impl LayoutConfig {
    pub fn load() -> Self {
        let path = config_dir().join("layout.json");
        if let Ok(data) = fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str::<LayoutConfig>(&data) {
                // Clamp to sane range
                return LayoutConfig {
                    sidebar_ratio: cfg.sidebar_ratio.clamp(0.05, 0.50),
                    list_ratio: cfg.list_ratio.clamp(0.15, 0.85),
                };
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        let path = config_dir().join("layout.json");
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = fs::write(&path, data);
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-account config: load / save / migrate
// ---------------------------------------------------------------------------

impl MultiAccountFileConfig {
    /// Load config, auto-migrating from legacy single-account format if needed.
    pub fn load() -> Result<Option<Self>, String> {
        let path = config_path();
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(&path).map_err(|e| format!("read config: {e}"))?;

        // Try new multi-account format first
        if let Ok(multi) = serde_json::from_str::<MultiAccountFileConfig>(&data) {
            return Ok(Some(multi));
        }

        // Try legacy single-account format (JSON object with "server" key)
        if let Ok(legacy) = serde_json::from_str::<FileConfig>(&data) {
            log::info!("Migrating legacy single-account config to multi-account format");
            let id = new_account_id();
            let label = legacy.username.clone();
            let migrated = MultiAccountFileConfig {
                accounts: vec![FileAccountConfig {
                    id: id.clone(),
                    label,
                    server: legacy.server,
                    port: legacy.port,
                    username: legacy.username,
                    starttls: legacy.starttls,
                    password: legacy.password,
                    email_addresses: legacy.email_addresses,
                    smtp: SmtpOverrides::default(),
                }],
            };
            // Write back migrated format
            if let Err(e) = migrated.save() {
                log::warn!("Failed to write migrated config: {}", e);
            }
            return Ok(Some(migrated));
        }

        Err("Failed to parse config file (neither multi-account nor legacy format)".into())
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create config dir: {e}"))?;
        }
        let data =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize config: {e}"))?;
        fs::write(&path, data).map_err(|e| format!("write config: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Config resolution
// ---------------------------------------------------------------------------

impl Config {
    /// Try env vars. Returns None if any required var is missing.
    fn from_env() -> Option<Self> {
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

    /// Resolve all accounts from config.
    pub fn resolve_all_accounts() -> Result<Vec<AccountConfig>, ConfigNeedsInput> {
        // 1. Env vars → single env account
        if let Some(config) = Self::from_env() {
            log::info!("Config loaded from environment variables");
            let smtp = SmtpConfig::from_imap_config(&config);
            return Ok(vec![AccountConfig {
                id: ENV_ACCOUNT_ID.to_string(),
                label: config.username.clone(),
                imap_server: config.imap_server.clone(),
                imap_port: config.imap_port,
                username: config.username.clone(),
                password: config.password.clone(),
                use_starttls: config.use_starttls,
                email_addresses: config.email_addresses.clone(),
                smtp,
                smtp_overrides: SmtpOverrides::default(),
            }]);
        }

        // 2. Config file
        match MultiAccountFileConfig::load() {
            Ok(Some(multi)) => {
                let mut accounts = Vec::new();
                for fac in &multi.accounts {
                    match resolve_password(&fac.password, &fac.username, &fac.server) {
                        Ok(password) => {
                            accounts.push(AccountConfig::from_file_account(fac, password));
                        }
                        Err(e) => {
                            log::warn!(
                                "Failed to resolve password for account '{}': {}",
                                fac.label,
                                e
                            );
                            // Skip accounts with unresolvable passwords for now;
                            // they can be re-entered via setup dialog
                        }
                    }
                }
                if accounts.is_empty() && !multi.accounts.is_empty() {
                    // All accounts failed password resolution — show password dialog
                    let fac = &multi.accounts[0];
                    return Err(ConfigNeedsInput::PasswordOnly {
                        server: fac.server.clone(),
                        port: fac.port,
                        username: fac.username.clone(),
                        starttls: fac.starttls,
                        error: Some("Keyring unavailable for all accounts".into()),
                    });
                }
                if accounts.is_empty() {
                    return Err(ConfigNeedsInput::FullSetup);
                }
                Ok(accounts)
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_password(
    backend: &PasswordBackend,
    username: &str,
    server: &str,
) -> Result<String, String> {
    match backend {
        PasswordBackend::Plaintext { value } => Ok(value.clone()),
        PasswordBackend::Keyring => keyring::get_password(username, server),
    }
}

