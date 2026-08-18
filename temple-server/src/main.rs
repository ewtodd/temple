use clap::{CommandFactory, Parser};
use clap_complete::{self, Shell};
use std::sync::Arc;
use temple_agent::{agent_server, config};

#[derive(Parser)]
#[command(name = "temple-server", about = "renco's always-on agent harness")]
struct Cli {
    /// Path to config file
    #[arg(long)]
    config: Option<std::path::PathBuf>,

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
    /// Usage: --generate-token USERNAME PHONE [--admin] [--priority N]
    /// Writes `token:username:phone[:admin[:priority]]` to the auth_token_file
    /// and prints the token.
    #[arg(long, num_args = 2, value_names = ["USERNAME", "PHONE"])]
    generate_token: Option<Vec<String>>,

    /// Mark the generated user as an admin
    #[arg(long)]
    admin: bool,

    /// Queue priority for the generated user — lower runs first
    /// (0 = ethan, 1 = valarie, -1 = default)
    #[arg(long, allow_negative_numbers = true)]
    priority: Option<i32>,
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

    let cli = Cli::parse();
    cli.print_completions();

    // Load config
    let cfg = config::Config::load(cli.config.as_deref());
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
        let token = generate_and_save_token(&cfg, username, phone, cli.admin, cli.priority)?;
        println!("{token}");
        return Ok(());
    }

    agent_server::run_agent_server(Arc::new(cfg), false).await
}

/// Generate a random 32-byte auth token, write `token:username:phone` to the
/// auth_token_file, and return the token. The file is created if it doesn't
/// exist; existing lines are preserved.
fn generate_and_save_token(
    cfg: &config::Config,
    username: &str,
    phone: &str,
    admin: bool,
    priority: Option<i32>,
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

    let token_file = cfg
        .auth_token_file
        .as_ref()
        .ok_or("auth_token_file not set in config — cannot generate token")?;

    // ':' is the field separator and newlines are the record separator —
    // either would corrupt the token file.
    for (field, value) in [("username", username), ("phone", phone)] {
        if value.is_empty() || value.contains(':') || value.chars().any(char::is_whitespace) {
            return Err(
                format!("invalid {field}: must be non-empty with no ':' or whitespace").into(),
            );
        }
    }

    if let Some(parent) = token_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let line = match (admin, priority) {
        (true, Some(p)) => format!("{token}:{username}:{phone}:yes:{p}\n"),
        (false, Some(p)) => format!("{token}:{username}:{phone}:no:{p}\n"),
        (true, None) => format!("{token}:{username}:{phone}:yes\n"),
        (false, None) => format!("{token}:{username}:{phone}\n"),
    };
    let existing = std::fs::read_to_string(token_file).unwrap_or_default();
    // Remove any existing line for this username (re-generating replaces)
    let lines: Vec<&str> = existing
        .lines()
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
