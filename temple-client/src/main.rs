mod app;
mod client;
mod input;
mod render;
mod state;
mod tools;
mod ui;

use clap::{CommandFactory, Parser};
use clap_complete::Shell;

#[derive(Parser)]
#[command(name = "temple", about = "renco TUI client + headless daemon")]
struct Cli {
    #[arg(short, long, default_value = "127.0.0.1:42123")]
    server: String,
    #[arg(short, long, default_value = ".")]
    cwd: String,
    #[arg(short = 'C', long)]
    client_id: Option<String>,
    /// Auth token (or set TEMPLE_TOKEN env var)
    #[arg(short = 't', long, env = "TEMPLE_TOKEN")]
    token: Option<String>,
    /// Resume the most recent persisted session on connect
    #[arg(long)]
    r#continue: bool,
    /// Use TLS (wss://) — set automatically if server starts with https://
    #[arg(long)]
    tls: bool,
    #[arg(long, value_enum)]
    generate_completions: Option<Shell>,
    /// Run as a headless daemon — hosts the full agent locally (loop,
    /// tools, Signal, cron) and serves the TUI over a local WebSocket.
    /// Requires --config with the per-user agent configuration.
    #[arg(long)]
    daemon: bool,
    /// Agent config file (temple-server style TOML). Required with --daemon.
    #[arg(long, required_if_eq("daemon", "true"))]
    config: Option<std::path::PathBuf>,
    /// Log the full session conversation to .temple-session.log in CWD
    #[arg(long)]
    log_session: bool,
    /// Path to SSH private key for daemon authentication (e.g. ~/.ssh/id_ed25519).
    #[arg(long)]
    identity: Option<String>,
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

fn main() {
    let cli = Cli::parse();
    cli.print_completions();

    let cwd =
        std::fs::canonicalize(&cli.cwd).unwrap_or_else(|_| std::path::PathBuf::from(&cli.cwd));
    let client_id = cli
        .client_id
        .unwrap_or_else(|| whoami::fallible::hostname().unwrap_or_else(|_| "unknown".into()));

    if cli.daemon {
        // Headless daemon: the full agent runs in-process (loop, tools,
        // Signal, cron) and the TUI connects to its local WebSocket.
        let cfg = temple_agent::config::Config::load(cli.config.as_deref());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            if let Err(e) =
                temple_agent::agent_server::run_agent_server(std::sync::Arc::new(cfg), true).await
            {
                eprintln!("temple-daemon: fatal: {e}");
                std::process::exit(1);
            }
        });
        return;
    }

    let cwd_str = cwd.to_string_lossy().to_string();
    let mut app = app::App::new(
        cwd_str,
        client_id,
        cli.token,
        cli.r#continue,
        cli.tls,
        cli.server,
        cli.log_session,
    );

    if let Err(e) = app.run() {
        eprintln!("temple: {e}");
    }
}
