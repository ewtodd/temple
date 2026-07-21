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
    /// Usage: --generate-token USERNAME PHONE [--admin]
    /// Writes `token:username:phone[:yes]` to the auth_token_file and prints the token.
    #[arg(long, num_args = 2, value_names = ["USERNAME", "PHONE"])]
    generate_token: Option<Vec<String>>,

    /// Mark the generated user as an admin
    #[arg(long)]
    admin: bool,
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
        let token = generate_and_save_token(&cfg, username, phone, cli.admin)?;
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
    agent.set_ssh_config(
        cfg.ssh_targets.clone(),
        cfg.ssh_bastion.clone(),
        cfg.ssh_key_path.clone(),
    );
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
            let auth_token_file = cfg2.auth_token_file.clone();
            let data_dir_workspace = cfg2.data_dir_workspace();
            // Per-sender active session (session id chosen via /session, /new)
            let active_sessions: Arc<Mutex<std::collections::HashMap<String, uuid::Uuid>>> =
                Arc::new(Mutex::new(std::collections::HashMap::new()));
            let active_for_handler = active_sessions.clone();
            // Pending permission requests: sender → (request_id, session_id)
            let pending_perms: Arc<Mutex<std::collections::HashMap<String, (uuid::Uuid, uuid::Uuid)>>> =
                Arc::new(Mutex::new(std::collections::HashMap::new()));
            let perms_for_handler = pending_perms.clone();
            let handler = Arc::new(move |sender: String, message: String, timestamp: u64| {
                let agent = agent_for_handler.clone();
                let scope = scope_for_handler.clone();
                let signal = signal_for_handler.clone();
                let memory = memory_for_handler.clone();
                let token_file = auth_token_file.clone();
                let workspace = data_dir_workspace.clone();
                let active = active_for_handler.clone();
                let perms = perms_for_handler.clone();
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
                                            "Verified! You are now registered as {}.",
                                            auth_user.username
                                        )).await.ok();
                                        notify_admins(&signal, &memory, &format!(
                                            "{} (+{}...) just verified on Signal.",
                                            auth_user.username, &auth_user.phone[..auth_user.phone.len().min(6)]
                                        )).await;
                                        return;
                                    }
                                }
                                signal.send(&sender, "token not recognized — check with Ethan and try again.").await.ok();
                                notify_admins(&signal, &memory, &format!(
                                    "Someone from {sender} tried /verify with an invalid token."
                                )).await;
                                return;
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

                    let trimmed = message.trim();

                    // ── Session commands ──

                    // ── Pending permission reply (y/N) ──
                    {
                        let pending = perms.lock().await.get(&sender).copied();
                        if let Some((request_id, _sid)) = pending {
                            let lower = trimmed.to_lowercase();
                            let granted = lower == "y" || lower == "yes" || lower == "allow";
                            if granted || lower == "n" || lower == "no" || lower == "deny" {
                                perms.lock().await.remove(&sender);
                                agent.permissions.resolve(request_id, granted).await;
                                signal.send(&sender,
                                    if granted { "ok, proceeding" } else { "denied" }
                                ).await.ok();
                                return;
                            }
                        }
                    }

                    if let Some(msg) = trimmed.strip_prefix("/broadcast ") {
                        let body = msg.trim();
                        if body.is_empty() {
                            signal.send(&sender, "usage: /broadcast <message>").await.ok();
                            return;
                        }
                        // Admin check: sender must be in the admin list
                        let admins = memory.get_admins().await.unwrap_or_default();
                        let is_admin = admins.iter().any(|(phone, uuid)| {
                            phone == &sender || uuid.as_deref() == Some(&sender)
                        });
                        if !is_admin {
                            signal.send(&sender, "admin only").await.ok();
                            return;
                        }
                        match memory.get_signal_users().await {
                            Ok(users) => {
                                let mut count = 0;
                                for (_, phone, uuid) in &users {
                                    let recipient = uuid.as_deref().unwrap_or(phone);
                                    if signal.send(recipient, &format!("📢 renco: {body}")).await.is_ok() {
                                        count += 1;
                                    }
                                }
                                signal.send(&sender, &format!("sent to {count} users")).await.ok();
                            }
                            Err(e) => { signal.send(&sender, &format!("error: {e}")).await.ok(); }
                        }
                        return;
                    }

                    if trimmed == "/help" {
                        signal.send(&sender,
                            "commands:\n\
                             /sessions — list your recent sessions\n\
                             /session <id-prefix> — resume a session\n\
                             /new <target> [dir] — new coding session\n\
                             /new — new session\n\
                             /quick — back to the default session\n\
                             /clear <account> — delete all sessions for an account (e.g. /clear e-play)\n\
                             /targets — list ssh targets\n\
                             /help — this"
                        ).await.ok();
                        return;
                    }

                    if let Some(account) = trimmed.strip_prefix("/clear ") {
                        let account = account.trim();
                        if account.is_empty() {
                            signal.send(&sender, "usage: /clear <account> (e.g. /clear e-play)").await.ok();
                        } else {
                            match memory.clear_sessions(account).await {
                                Ok(n) => {
                                    signal.send(&sender, &format!("deleted {n} sessions for {account}")).await.ok();
                                }
                                Err(e) => {
                                    signal.send(&sender, &format!("error: {e}")).await.ok();
                                }
                            }
                        }
                        return;
                    }

                    if trimmed == "/targets" {
                        let names = agent.ssh_target_names();
                        let body = if names.is_empty() {
                            "no ssh targets configured".to_string()
                        } else {
                            names.join("\n")
                        };
                        signal.send(&sender, &body).await.ok();
                        return;
                    }

                    if trimmed == "/sessions" {
                        match memory.list_sessions(&username, 10).await {
                            Ok(rows) if !rows.is_empty() => {
                                let mut body = String::from("recent sessions:\n");
                                for r in &rows {
                                    let id8: String = r.id.simple().to_string().chars().take(8).collect();
                                    let target = r.ssh_target.as_deref().unwrap_or("quick");
                                    let title = r.title.as_deref().unwrap_or("(untitled)");
                                    body.push_str(&format!("• {id8} · {target} · {title}\n"));
                                }
                                body.push_str("\nresume with /session <id-prefix>");
                                signal.send(&sender, &body).await.ok();
                            }
                            Ok(_) => {
                                signal.send(&sender, "no sessions yet — /new <target> to start one").await.ok();
                            }
                            Err(e) => {
                                signal.send(&sender, &format!("error listing sessions: {e}")).await.ok();
                            }
                        }
                        return;
                    }

                    if let Some(prefix) = trimmed.strip_prefix("/session ") {
                        let prefix = prefix.trim().to_lowercase();
                        match memory.list_sessions(&username, 50).await {
                            Ok(rows) => {
                                let found = rows.iter().find(|r| {
                                    r.id.simple().to_string().starts_with(&prefix)
                                });
                                match found {
                                    Some(r) => {
                                        let sid = r.id;
                                        // Load into memory if not already there
                                        if !agent.has_session(sid).await {
                                            if let Err(e) = agent.resume_session(sid, &username).await {
                                                signal.send(&sender, &format!("resume failed: {e}")).await.ok();
                                                return;
                                            }
                                        }
                                        active.lock().await.insert(sender.clone(), sid);
                                        // Auto-attach SSH if needed — Signal sessions
                                        // need SSH to execute tools (no TUI client).
                                        if !agent.has_ssh(sid).await {
                                            if let Some(t) = agent.ssh_targets.iter()
                                                .find(|t| t.owner == username)
                                            {
                                                if let Err(e) = agent.attach_ssh(sid, &t.name).await {
                                                    tracing::warn!("attach_ssh for {sid}: {e}");
                                                } else {
                                                    tracing::info!("auto-attached SSH {} to session {sid}", t.name);
                                                }
                                            }
                                        }
                                        let target = r.ssh_target.as_deref().unwrap_or("quick");
                                        let title = r.title.as_deref().unwrap_or("(untitled)");
                                        signal.send(&sender, &format!(
                                            "📋 resumed {target} · {title}"
                                        )).await.ok();
                                    }
                                    None => {
                                        signal.send(&sender, "no session matching that prefix").await.ok();
                                    }
                                }
                            }
                            Err(e) => {
                                signal.send(&sender, &format!("error: {e}")).await.ok();
                            }
                        }
                        return;
                    }

                    if trimmed == "/quick" {
                        active.lock().await.remove(&sender);
                        signal.send(&sender, "📋 back to default session").await.ok();
                        return;
                    }

                    if trimmed == "/new" || trimmed.starts_with("/new ") {
                        let rest = trimmed.strip_prefix("/new").unwrap().trim();
                        let parts: Vec<&str> = rest.splitn(3, ' ').filter(|p| !p.is_empty()).collect();
                        let target = parts.first().copied();
                        let subdir = parts.get(1).copied();
                        match agent.new_persisted_session(&username, target, subdir, None).await {
                            Ok(sid) => {
                                active.lock().await.insert(sender.clone(), sid);
                                let id8: String = sid.simple().to_string().chars().take(8).collect();
                                signal.send(&sender, &format!(
                                    "📋 new session {id8} · {}{}",
                                    target.unwrap_or("quick"),
                                    subdir.map(|d| format!(" · {d}")).unwrap_or_default(),
                                )).await.ok();
                            }
                            Err(e) => {
                                signal.send(&sender, &format!("error: {e}")).await.ok();
                            }
                        }
                        return;
                    }

                    // ── Normal message → route to active or personal session ──
                    let target_session = {
                        let active_id = active.lock().await.get(&sender).copied();
                        match active_id {
                            Some(id) => id,
                            None => {
                                // Create a per-user session with the correct username
                                // (not the shared sys_session with "signal" as username)
                                let new_id = uuid::Uuid::new_v4();
                                agent.open_session(
                                    new_id,
                                    &username,
                                    &workspace,
                                    router::SessionKind::Interactive,
                                    None,
                                ).await;
                                active.lock().await.insert(sender.clone(), new_id);
                                new_id
                            }
                        }
                    };

                    // Send typing indicator
                    if let Err(e) = signal.send_typing(&sender).await {
                        tracing::warn!("send_typing: {e}");
                    }

                    // Keep the typing bubble alive (expires after 15s) and
                    // send a "still consulting" note every 2 minutes.
                    let signal_for_timer = signal.clone();
                    let sender_for_timer = sender.clone();
                    let timer_handle = tokio::spawn(async move {
                        let mut ticks: u32 = 0;
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                            ticks += 1;
                            signal_for_timer.send_typing(&sender_for_timer).await.ok();
                            if ticks % 12 == 0 {
                                signal_for_timer
                                    .send(&sender_for_timer, "still consulting the oracle...")
                                    .await
                                    .ok();
                            }
                        }
                    });

                    let sender_for_emit = sender.clone();
                    let signal_for_emit = signal.clone();
                    let perms_for_emit = perms.clone();
                    let response = Arc::new(std::sync::Mutex::new(String::new()));
                    let resp = response.clone();
                    let emit = move |ev: agent::AgentEvent| {
                        match ev {
                            agent::AgentEvent::PermissionNeeded(req) => {
                                // Intercept — send a Signal prompt and store the
                                // request so the reply resolves it.
                                let path = req.path.chars().take(200).collect::<String>();
                                tokio::spawn({
                                    let signal = signal_for_emit.clone();
                                    let sender = sender_for_emit.clone();
                                    let perms = perms_for_emit.clone();
                                    let request_id = req.request_id;
                                    let session_id = req.session_id;
                                    async move {
                                        perms.lock().await.insert(
                                            sender.clone(),
                                            (request_id, session_id),
                                        );
                                        signal.send(&sender, &format!(
                                            "Allow {path}? Reply y or N (300s timeout)"
                                        )).await.ok();
                                    }
                                });
                            }
                            agent::AgentEvent::Delta(d) => {
                                response.lock().unwrap().push_str(&d);
                            }
                            _ => {}
                        }
                    };
                    agent
                        .process_chat(target_session, &message, scope, &emit)
                        .await;

                    // Stop the "still consulting" timer
                    timer_handle.abort();

                    let text = resp.lock().unwrap().clone();
                    if !text.trim().is_empty() {
                        // Preamble for non-quick sessions
                        let full = {
                            let (target, title) = agent.session_display(target_session)
                                .await
                                .unwrap_or((None, None));
                            let id8: String = target_session.simple().to_string().chars().take(8).collect();
                            format!(
                                "📋 {} · {}{}\n\n{}",
                                target.as_deref().unwrap_or("temple"),
                                id8,
                                title.map(|t| format!(" · {t}")).unwrap_or_default(),
                                text
                            )
                        };
                        signal.send_multi(&sender, &full).await.ok();
                    }
                });
            });

            signal.receive_loop(handler).await;
        });
    }

    // Load signal users from token file into DB and send welcome messages
    // to newly added (unverified) users so they can reply on Signal.
    if cfg.signal.enabled {
        match auth::load_signal_users(&memory, &cfg).await {
            Ok(users) => {
                for u in &users {
                    // Check if user hasn't verified yet (no UUID)
                    if let Ok(Some((_, _, uuid))) = memory.find_signal_user(&u.phone).await {
                        if uuid.is_none() {
                            signal.send(
                                &u.phone,
                                "You've been added to renco! Send /verify to complete setup.",
                            ).await.ok();
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("Failed to load signal users: {e}"),
        }
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
    admin: bool,
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

    let line = if admin {
        format!("{token}:{username}:{phone}:yes\n")
    } else {
        format!("{token}:{username}:{phone}\n")
    };
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

async fn notify_admins(signal: &crate::signal::Signal, memory: &crate::memory::Memory, message: &str) {
    match memory.get_admins().await {
        Ok(admins) if !admins.is_empty() => {
            for (phone, uuid) in &admins {
                let recipient = uuid.as_deref().unwrap_or(phone);
                signal.send(recipient, message).await.ok();
            }
        }
        Ok(_) => {
            tracing::warn!("notify_admins: no admins configured");
        }
        Err(e) => {
            tracing::warn!("notify_admins: {e}");
        }
    }
}
