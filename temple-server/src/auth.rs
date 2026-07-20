use std::path::Path;
use crate::memory::Memory;

/// A user authenticated via token.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub username: String,
    pub phone: String,
}

/// Check a token against the auth_token_file.
/// Returns the user info if the token matches.
pub fn check_token(token_file: &Path, token: &str) -> Result<AuthUser, String> {
    let content = std::fs::read_to_string(token_file)
        .map_err(|e| format!("read token file: {e}"))?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        if parts.len() != 3 {
            continue;
        }
        if parts[0] == token {
            return Ok(AuthUser {
                username: parts[1].to_string(),
                phone: parts[2].to_string(),
            });
        }
    }
    Err("invalid token".into())
}

/// Load all users from the auth_token_file into the signal_users DB table.
/// Called at startup so the DB knows every user's phone number for Signal
/// UUID verification. UUIDs are added later when the user sends /verify.
pub async fn load_signal_users(
    memory: &Memory,
    config: &crate::config::Config,
) -> Result<(), String> {
    let token_file = config.auth_token_file.as_ref()
        .ok_or("auth_token_file not set")?;
    let content = std::fs::read_to_string(token_file)
        .map_err(|e| format!("read token file: {e}"))?;
    let mut count = 0;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        if parts.len() != 3 {
            continue;
        }
        let username = parts[1];
        let phone = parts[2];
        memory.upsert_signal_user_phone(username, phone).await
            .map_err(|e| format!("upsert signal user: {e}"))?;
        count += 1;
    }
    tracing::info!("Loaded {count} signal users from token file");
    Ok(())
}
