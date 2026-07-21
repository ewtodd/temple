use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub listen: String,
    pub litellm_url: String,
    pub litellm_api_key: Option<String>,
    pub db_path: PathBuf,
    pub signal: SignalConfig,
    pub nextcloud: NextcloudConfig,
    pub models: ModelConfig,
    pub cron: CronConfig,
    pub default_permission: String,
    pub allowed_dirs: Vec<String>,
    /// Local llama.cpp instance for routing/titles (on oracle).
    pub local_llama_url: String,
    pub local_llama_model: String,
    /// Path to the auth tokens file. Each line: `token:username:phone`.
    pub auth_token_file: Option<PathBuf>,
    /// SSH targets for remote tool execution.
    pub ssh_targets: Vec<SshTarget>,
    /// Bastion host for SSH (e.g., "deploy-mu" or "10.0.0.2:2222").
    pub ssh_bastion: Option<String>,
    /// SSH key for connecting to workstations (path on oracle).
    pub ssh_key_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SshTarget {
    /// Human-readable name, e.g., "e-work@e-desktop"
    pub name: String,
    /// Username on the workstation, e.g., "e-work"
    pub account: String,
    /// Workstation IP, e.g., "10.0.0.4"
    pub host: String,
    /// SSH port (usually 2222)
    pub port: u16,
    /// User who owns this account (matches token file username)
    pub owner: String,
    /// Allowed directories for this target (in addition to $HOME and /tmp)
    #[serde(default)]
    pub allowed_dirs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SignalConfig {
    pub enabled: bool,
    /// signal-cli daemon TCP socket, e.g. "127.0.0.1:7583"
    pub socket_addr: String,
    /// Phone numbers allowed to send inbound commands (E.164 format, + prefix).
    /// If empty, all senders are rejected.
    pub allowed_senders: Vec<String>,
    /// Default recipient for outbound notifications (user's phone number).
    pub default_recipient: String,
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket_addr: "127.0.0.1:7583".into(),
            allowed_senders: Vec::new(),
            default_recipient: String::new(),
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
    /// Default model for fallback / Medium complexity
    pub default_model: String,
    /// Fast model for Simple queries (oracle local classifier)
    pub simple_model: String,
    /// Planner model for Complex pipeline (deepseek on son-of-anton)
    pub planner_model: String,
    /// Executor model for Complex pipeline (qwen coding on anton)
    pub executor_model: String,
    /// Reviewer model for Complex pipeline (deepseek on son-of-anton)
    pub reviewer_model: String,
    /// Model for Critical complexity (deepseek direct)
    pub critical_model: String,
    /// Model for research/lookups (gemma on anton — good at general knowledge)
    pub researcher_model: String,
    /// Model definitions
    pub models: Vec<ModelDef>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            use_local_router: false,
            local_router_model: None,
            default_model: "deepseek-v4-flash-high".into(),
            simple_model: "qwen3-4b-instruct".into(),
            planner_model: "deepseek-v4-flash-high".into(),
            executor_model: "qwen3.6-27b-coding".into(),
            reviewer_model: "deepseek-v4-flash-high".into(),
            critical_model: "deepseek-v4-flash-high".into(),
            researcher_model: "gemma-4-31b".into(),
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
            signal: SignalConfig::default(),
            nextcloud: NextcloudConfig::default(),
            models: ModelConfig::default(),
            cron: CronConfig::default(),
            default_permission: "default".into(),
            allowed_dirs: vec!["/etc/nixos".into(), "/home".into()],
            local_llama_url: "http://127.0.0.1:8080/v1".into(),
            local_llama_model: "qwen3-4b-instruct".into(),
            auth_token_file: None,
            ssh_targets: Vec::new(),
            ssh_bastion: None,
            ssh_key_path: None,
        }
    }
}

impl Config {
    /// Workspace directory for system-initiated sessions (cron).
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
