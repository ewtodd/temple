mod agent;
mod auth;
mod config;
mod cron;
mod litellm;
mod mcp;
mod memory;
mod nextcloud;
mod permissions;
mod router;
mod server;
mod signal;
mod ssh;

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

    /// Generate a new auth token for a user.
    /// Usage: --generate-token USERNAME PHONE
    /// Writes `token:username:phone` to the auth_token_file and prints the token.
    #[arg(long, num_args = 2, value_names = ["USERNAME", "PHONE"])]
    generate_token: Option<Vec<String>>,
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

    // --generate-token: generate a new auth token, write to file, print, exit.
    if let Some(args) = &cli.generate_token {
        let username = &args[0];
        let phone = &args[1];
        let token = generate_and_save_token(&cfg, username, phone)?;
        println!("{token}");
        return Ok(());
    }

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
        cfg.models.clone(),
    );
    agent.set_local_endpoint(&cfg.local_llama_url, &cfg.local_llama_model);
    let agent = Arc::new(agent);

    // Load tools (local + MCP)
    agent.refresh_tools().await;

    // Initialize integrations
    let signal = Arc::new(signal::Signal::new(&cfg.signal));
    let nextcloud = Arc::new(Mutex::new(nextcloud::Nextcloud::new(&cfg.nextcloud)));

    // Signal two-way loop: inbound messages → agent → outbound reply
    if cfg.signal.enabled {
        let agent = agent.clone();
        let signal = signal.clone();
        let memory_for_signal = memory.clone();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            let sys_session = uuid::Uuid::nil();
            agent.open_session(
                sys_session,
                "signal",
                &cfg2.data_dir_workspace(),
                router::SessionKind::Quick,
                None,
            ).await;
            let scope = Arc::new(Mutex::new(
                permissions::PermissionScope::new(
                    std::path::Path::new(&cfg2.data_dir_workspace()),
                    PermissionMode::Default,
                    &cfg2.allowed_dirs,
                ).await,
            ));

            let agent_for_handler = agent.clone();
            let scope_for_handler = scope.clone();
            let signal_for_handler = signal.clone();
            let memory_for_handler = memory_for_signal.clone();
            let token_file_for_handler = cfg2.auth_token_file.clone();
            let handler = Arc::new(move |sender: String, message: String, timestamp: u64| {
                let agent = agent_for_handler.clone();
                let scope = scope_for_handler.clone();
                let signal = signal_for_handler.clone();
                let memory = memory_for_handler.clone();
                let token_file = token_file_for_handler.clone();
                tokio::spawn(async move {
                    tracing::info!("signal inbound from {sender}: {message:.60}");

                    // Send read receipt immediately
                    if timestamp > 0 {
                        signal.send_read_receipt(&sender, timestamp).await.ok();
                    }

                    // Check if sender is a verified signal user
                    let user = memory.find_signal_user(&sender).await
                        .unwrap_or(None);

                    let username = match &user {
                        Some((u, _, Some(_))) => u.clone(),
                        Some((u, _, None)) => u.clone(),
                        None => {
                            if let Some(token) = message.strip_prefix("/verify ") {
                                let token = token.trim();
                                if let Some(ref token_file) = token_file {
                                    if let Ok(auth_user) = auth::check_token(token_file, token) {
                                        let _ = memory.set_signal_user_uuid(
                                            &auth_user.username,
                                            &sender,
                                        ).await;
                                        signal.send(&sender, &format!(
                                            "Verified! You are now registered as {}. You can send messages to renco.",
                                            auth_user.username
                                        )).await.ok();
                                        return;
                                    }
                                }
                            }
                            tracing::warn!(
                                "signal: ignoring message from unknown sender: {sender}"
                            );
                            return;
                        }
                    };

                    // If user's UUID wasn't set yet, set it now
                    if let Some((_, _, None)) = &user {
                        let _ = memory.set_signal_user_uuid(&username, &sender).await;
                    }

                    // Send typing indicator
                    signal.send_typing(&sender).await.ok();

                    // Start "still consulting" timer — sends a status message
                    // every 2 minutes while the agent is still working.
                    let signal_for_timer = signal.clone();
                    let sender_for_timer = sender.clone();
                    let timer_handle = tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                            signal_for_timer.send(&sender_for_timer, "still consulting the oracle...").await.ok();
                            signal_for_timer.send_typing(&sender_for_timer).await.ok();
                        }
                    });

                    let response = Arc::new(std::sync::Mutex::new(String::new()));
                    let first_delta = Arc::new(std::sync::Mutex::new(true));
                    let resp = response.clone();
                    let first = first_delta.clone();
                    let emit = move |ev: agent::AgentEvent| {
                        if let agent::AgentEvent::Delta(d) = ev {
                            // First delta cancels the "consulting" message
                            // (typing indicator continues)
                            response.lock().unwrap().push_str(&d);
                        }
                    };
                    agent
                        .process_chat(sys_session, &message, scope, &emit)
                        .await;

                    // Stop the "still consulting" timer
                    timer_handle.abort();

                    let text = resp.lock().unwrap().clone();
                    if !text.trim().is_empty() {
                        signal.send_multi(&sender, &text).await.ok();
                    }
                });
            });

            signal.receive_loop(handler).await;
        });
    }

    // Cron scheduler
    {
        let agent = agent.clone();
        let memory = memory.clone();
        let signal = signal.clone();
        let nextcloud = nextcloud.clone();
        tokio::spawn(async move {
            let scheduler = cron::CronScheduler::new(agent, memory, signal, nextcloud);
            if let Err(e) = scheduler.run_forever().await {
                tracing::error!("cron error: {e}");
            }
        });
    }

    // Notify all signal users that renco is online
    if cfg.signal.enabled {
        match memory.get_signal_users().await {
            Ok(users) => {
                for (username, phone, uuid) in &users {
                    let recipient = uuid.as_deref().unwrap_or(phone.as_str());
                    signal.notify_recipient(recipient, "renco", "Temple server started, renco is online.")
                        .await.ok();
                }
            }
            Err(e) => tracing::warn!("get signal users for startup notify: {e}"),
        }
    }

    // Run the WebSocket server
    server::run_server(agent, memory, cfg).await
}

/// Generate a random 32-byte auth token, write `token:username:phone` to the
/// auth_token_file, and return the token. The file is created if it doesn't
/// exist; existing lines are preserved.
fn generate_and_save_token(
    cfg: &config::Config,
    username: &str,
    phone: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    use rand::Rng;
    let token: String = (0..32)
        .map(|_| {
            let n = rand::thread_rng().gen_range(0..62);
            if n < 26 {
                (b'a' + n) as char
            } else if n < 52 {
                (b'A' + (n - 26)) as char
            } else {
                (b'0' + (n - 52)) as char
            }
        })
        .collect();

    let token_file = cfg.auth_token_file.as_ref()
        .ok_or("auth_token_file not set in config — cannot generate token")?;

    if let Some(parent) = token_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let line = format!("{token}:{username}:{phone}\n");
    let mut existing = std::fs::read_to_string(token_file).unwrap_or_default();
    // Remove any existing line for this username (re-generating replaces)
    existing.retain(|c| c != '\n' && c != '\r');
    let lines: Vec<&str> = existing.lines()
        .filter(|l| {
            let parts: Vec<&str> = l.splitn(3, ':').collect();
            parts.len() < 2 || parts[1] != username
        })
        .collect();
    let new_content = if lines.is_empty() {
        line
    } else {
        format!("{}\n{}", lines.join("\n"), line)
    };
    std::fs::write(token_file, new_content)?;

    Ok(token)
}
