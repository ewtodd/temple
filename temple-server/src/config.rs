use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub listen: String,
    pub backends: std::collections::HashMap<String, String>,
    pub db_path: PathBuf,
    pub signal: SignalConfig,
    pub nextcloud: NextcloudConfig,
    pub models: ModelConfig,
    pub cron: CronConfig,
    pub default_permission: String,
    pub allowed_dirs: Vec<String>,
    /// Path to the auth tokens file. Each line: `token:username:phone`.
    pub auth_token_file: Option<PathBuf>,
    /// TLS configuration for secure WebSocket connections.
    pub tls: TlsConfig,
    /// Health check listen address (host:port). When set, responds with 200 OK on HTTP GET.
    #[serde(default = "default_health_listen")]
    pub listen_health: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SignalConfig {
    pub enabled: bool,
    /// signal-cli daemon TCP socket, e.g. "127.0.0.1:7583"
    pub socket_addr: String,
    /// Phone numbers allowed to send inbound commands (E.164 format, + prefix).
    /// Optional ADDITIONAL restriction — when empty, senders are gated by
    /// the token-auth flow (signal_users table + /verify) instead.
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

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct ModelConfig {
    /// Default model for fallback / Medium complexity
    pub default_model: String,
    /// Fast model for Simple queries
    pub simple_model: String,
    /// Planner model for Complex pipeline
    pub planner_model: String,
    /// Executor model for Complex pipeline
    pub executor_model: String,
    /// Reviewer model for Complex pipeline
    pub reviewer_model: String,
    /// Model for research/lookups
    pub researcher_model: String,
    /// Model for complexity classification. Falls back to
    /// researcher_model when unset.
    #[serde(default)]
    pub router_model: Option<String>,
    /// Model for session titles and conversation summaries. Falls back to
    /// researcher_model when unset.
    #[serde(default)]
    pub title_model: Option<String>,
    /// Model for compaction summaries (long-context). Falls back to
    /// default_model when unset. Must have at least 16K context.
    #[serde(default)]
    pub compact_model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CronConfig {
    pub skills_extract: String,
    pub flake_update: String,
    pub self_maintenance: String,
    pub transient_sweep: String,
    /// Per-user cron jobs. Map of username → list of cron job definitions.
    #[serde(default)]
    pub user_cron: std::collections::HashMap<String, Vec<UserCronJob>>,
}

/// A user-specific cron job that runs an agent prompt on a schedule.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserCronJob {
    pub name: String,
    /// Cron schedule expression (5-field: "min hour dom month dow")
    pub schedule: String,
    /// System prompt given to the agent for this job
    pub system_prompt: String,
    /// Optional state file path (relative to data dir) that the agent can
    /// read from and write to across invocations
    #[serde(default)]
    pub state_file: Option<String>,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            skills_extract: "0 3 * * *".into(),
            flake_update: "0 4 * * *".into(),
            self_maintenance: "0 5 * * 0".into(),
            transient_sweep: "30 4 * * *".into(),
            user_cron: std::collections::HashMap::new(),
        }
    }
}

fn default_health_listen() -> String {
    "127.0.0.1:42124".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:42123".into(),
            backends: std::collections::HashMap::new(),
            db_path: PathBuf::from("./temple-memory.db"),
            signal: SignalConfig::default(),
            nextcloud: NextcloudConfig::default(),
            models: ModelConfig::default(),
            cron: CronConfig::default(),
            default_permission: "default".into(),
            allowed_dirs: vec!["/etc/nixos".into(), "/home".into()],
            auth_token_file: None,
            tls: TlsConfig::default(),
            listen_health: default_health_listen(),
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
                let data = std::fs::read_to_string(path).expect("Failed to read config file");
                toml::from_str(&data).expect("Failed to parse config file")
            } else {
                eprintln!("Config file not found at {path:?}, using defaults");
                Self::default()
            }
        } else {
            for p in &[
                "/etc/temple/config.toml",
                "/var/lib/temple/config.toml",
                "temple.toml",
            ] {
                let p = std::path::Path::new(p);
                if p.exists() {
                    let data = std::fs::read_to_string(p).expect("Failed to read config file");
                    return toml::from_str(&data).expect("Failed to parse config file");
                }
            }
            Self::default()
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct TlsConfig {
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

impl TlsConfig {
    pub fn acceptor(
        &self,
    ) -> Result<Option<tokio_native_tls::TlsAcceptor>, Box<dyn std::error::Error>> {
        match (&self.cert, &self.key) {
            (Some(cert), Some(key)) => {
                let cert_bytes = std::fs::read(cert)?;
                let key_bytes = std::fs::read(key)?;
                let identity = native_tls::Identity::from_pkcs8(&cert_bytes, &key_bytes)?;
                let acceptor =
                    tokio_native_tls::TlsAcceptor::from(native_tls::TlsAcceptor::new(identity)?);
                Ok(Some(acceptor))
            }
            _ => Ok(None),
        }
    }
}
