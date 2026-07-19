mod agent;
mod config;
mod cron;
mod litellm;
mod mcp;
mod memory;
mod nextcloud;
mod ntfy;
mod permissions;
mod router;
mod server;

use clap::{CommandFactory, Parser};
use clap_complete::{self, Generator, Shell};
use std::sync::Arc;
use temple_protocol::PermissionMode;
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(name = "temple-server", about = "renco's always-on agent harness")]
struct Cli {
    /// Path to config file
    #[arg(long)]
    config: Option<std::path::PathBuf>,

    /// Litellm API URL (overrides config)
    #[arg(long)]
    litellm_url: Option<String>,

    /// Database path
    #[arg(long)]
    db_path: Option<std::path::PathBuf>,

    /// Listen address
    #[arg(short = 'L', long)]
    listen: Option<String>,

    /// Generate shell completions
    #[arg(long, value_enum)]
    generate_completions: Option<Shell>,
}

impl Cli {
    fn print_completions(&self) {
        if let Some(shell) = &self.generate_completions {
            let mut cmd = Self::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(*shell, &mut cmd, name, &mut std::io::stdout());
            std::process::exit(0);
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "temple_server=info".into()),
        )
        .init();

    let mut cli = Cli::parse();
    cli.print_completions();

    // Load config
    let cfg = config::Config::load(cli.config.as_deref());
    let cfg = if let Some(url) = cli.litellm_url {
        config::Config { litellm_url: url, ..cfg }
    } else {
        cfg
    };
    let cfg = if let Some(db) = cli.db_path {
        config::Config { db_path: db, ..cfg }
    } else {
        cfg
    };
    let cfg = if let Some(listen) = cli.listen {
        config::Config { listen, ..cfg }
    } else {
        cfg
    };

    let cfg = Arc::new(cfg);

    tracing::info!("Starting temple server...");
    tracing::info!("Litellm endpoint: {}", cfg.litellm_url);
    tracing::info!("Database: {}", cfg.db_path.display());
    tracing::info!("Listening on: {}", cfg.listen);

    let api_key = cfg
        .litellm_api_key
        .clone()
        .or_else(|| std::env::var("LITELLM_API_KEY").ok())
        // Reuse the fleet's existing litellm-master-key agenix secret directly
        .or_else(|| std::env::var("LITELLM_MASTER_KEY").ok())
        .unwrap_or_else(|| {
            tracing::warn!("No litellm API key set — using 'empty'");
            "empty".into()
        });

    let memory = Arc::new(
        memory::Memory::open(&cfg.db_path)
            .await
            .expect("Failed to open database"),
    );

    // Initialize litellm + MCP clients
    let litellm = litellm::LiteLLM::new(&cfg.litellm_url, &api_key);
    let mcp = mcp::McpClient::new(&cfg.litellm_url, &api_key);

    // Initialize agent
    let mut agent = agent::Agent::new(
        litellm,
        mcp,
        memory.clone(),
        &cfg.models.default_model,
    );
    agent.set_local_endpoint(&cfg.local_llama_url, &cfg.local_llama_model);
    let agent = Arc::new(agent);

    // Load tools (local + MCP)
    agent.refresh_tools().await;

    // Initialize integrations
    let ntfy = Arc::new(Mutex::new(ntfy::Ntfy::new(&cfg.ntfy)));
    let nextcloud = Arc::new(Mutex::new(nextcloud::Nextcloud::new(&cfg.nextcloud)));

    // ntfy two-way loop: inbound messages → agent → outbound reply
    if cfg.ntfy.enabled {
        let agent = agent.clone();
        let ntfy = ntfy.clone();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            let sys_session = uuid::Uuid::nil();
            agent.open_session(sys_session, "ntfy", &cfg2.data_dir_workspace()).await;
            let scope = Arc::new(Mutex::new(permissions::PermissionScope::new(
                std::path::Path::new(&cfg2.data_dir_workspace()),
                PermissionMode::Default,
                &cfg2.allowed_dirs,
            )));
            loop {
                match ntfy.lock().await.poll_incoming().await {
                    Ok(messages) => {
                        for msg in messages {
                            tracing::info!("ntfy inbound: {msg:.60}");
                            let response = Arc::new(std::sync::Mutex::new(String::new()));
                            let resp = response.clone();
                            let emit = move |ev: agent::AgentEvent| {
                                if let agent::AgentEvent::Delta(d) = ev {
                                    response.lock().unwrap().push_str(&d);
                                }
                            };
                            agent
                                .process_chat(sys_session, &msg, scope.clone(), &emit)
                                .await;
                            let text = resp.lock().unwrap().clone();
                            if !text.trim().is_empty() {
                                let truncated: String = text.chars().take(4000).collect();
                                ntfy.lock().await.notify("renco", &truncated).await.ok();
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("ntfy poll: {e}");
                        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                        continue;
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            }
        });
    }

    // Cron scheduler
    {
        let agent = agent.clone();
        let memory = memory.clone();
        let ntfy = ntfy.clone();
        let nextcloud = nextcloud.clone();
        tokio::spawn(async move {
            let scheduler = cron::CronScheduler::new(agent, memory, ntfy, nextcloud);
            if let Err(e) = scheduler.run_forever().await {
                tracing::error!("cron error: {e}");
            }
        });
    }

    // Notify online
    if cfg.ntfy.enabled {
        ntfy.lock()
            .await
            .notify("renco", "Temple server started, renco is online.")
            .await
            .ok();
    }

    // Run the WebSocket server
    server::run_server(agent, cfg).await
}
