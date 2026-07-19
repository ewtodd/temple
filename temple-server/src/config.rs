use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Listen address for the WebSocket server
    pub listen: String,
    /// Litellm API endpoint
    pub litellm_url: String,
    /// Litellm API key (if empty, reads from env LITELLM_API_KEY)
    pub litellm_api_key: Option<String>,
    /// Database path for persistent memory
    pub db_path: PathBuf,
    /// ntfy configuration
    pub ntfy: NtfyConfig,
    /// Nextcloud configuration
    pub nextcloud: NextcloudConfig,
    /// Model routing configuration
    pub models: ModelConfig,
    /// Cron schedule overrides
    pub cron: CronConfig,
    /// Default permission mode
    pub default_permission: String,
    /// Allowed directories for default mode (beyond CWD)
    pub allowed_dirs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct NtfyConfig {
    pub enabled: bool,
    pub server_url: String,
    /// Topic for incoming commands to renco
    pub control_topic: String,
    /// Topic for outgoing notifications
    pub out_topic: String,
    /// Token from agenix
    pub token: Option<String>,
}

impl Default for NtfyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: "https://ntfy.ethanwtodd.com".into(),
            control_topic: "temple-ctrl".into(),
            out_topic: "temple-out".into(),
            token: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct NextcloudConfig {
    pub enabled: bool,
    pub server_url: String,
    pub username: String,
    pub password: Option<String>,
}

impl Default for NextcloudConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: "https://cloud.ethanwtodd.com".into(),
            username: "renco".into(),
            password: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ModelConfig {
    /// Whether to use small local model as router
    pub use_local_router: bool,
    /// Path to small GGUF for router (optional)
    pub local_router_model: Option<String>,
    /// Default model for fallback
    pub default_model: String,
    /// Model definitions
    pub models: Vec<ModelDef>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            use_local_router: false,
            local_router_model: None,
            default_model: "deepseek-v4-flash-high".into(),
            models: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelDef {
    pub id: String,
    pub host: String,
    pub litellm_model: String,
    pub capabilities: Vec<String>,
    pub priority: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CronConfig {
    pub skills_extract: String,
    pub flake_update: String,
    pub self_maintenance: String,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            skills_extract: "0 3 * * *".into(),
            flake_update: "0 4 * * *".into(),
            self_maintenance: "0 5 * * 0".into(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:42123".into(),
            litellm_url: "https://llm.ethanwtodd.com".into(),
            litellm_api_key: None,
            db_path: PathBuf::from("./temple-memory.db"),
            ntfy: NtfyConfig::default(),
            nextcloud: NextcloudConfig::default(),
            models: ModelConfig::default(),
            cron: CronConfig::default(),
            default_permission: "default".into(),
            allowed_dirs: vec!["/etc/nixos".into(), "/home".into()],
        }
    }
}

impl Config {
    /// Workspace directory for system-initiated sessions (ntfy, cron).
    pub fn data_dir_workspace(&self) -> String {
        self.db_path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "/var/lib/temple".into())
    }

    pub fn load(path: Option<&std::path::Path>) -> Self {
        if let Some(path) = path {
            if path.exists() {
                let data = std::fs::read_to_string(path)
                    .expect("Failed to read config file");
                toml::from_str(&data).expect("Failed to parse config file")
            } else {
                eprintln!("Config file not found at {path:?}, using defaults");
                Self::default()
            }
        } else {
            // Try standard paths
            for p in &[
                "/etc/temple/config.toml",
                "/var/lib/temple/config.toml",
                "temple.toml",
            ] {
                let p = std::path::Path::new(p);
                if p.exists() {
                    let data = std::fs::read_to_string(p)
                        .expect("Failed to read config file");
                    return toml::from_str(&data).expect("Failed to parse config file");
                }
            }
            Self::default()
        }
    }
}
