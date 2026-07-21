use std::path::Path;
use crate::memory::Memory;

/// A user authenticated via token.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub username: String,
    pub phone: String,
}

/// Check a token against the auth_token_file.
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

/// A loaded user from the token file — used for sending welcome messages.
#[derive(Debug, Clone)]
pub struct TokenUser {
    pub username: String,
    pub phone: String,
    pub token: String,
}

/// Load all users from the auth_token_file into the signal_users DB table.
/// Returns the list of users that were loaded (for sending welcome messages).
pub async fn load_signal_users(
    memory: &Memory,
    config: &crate::config::Config,
) -> Result<Vec<TokenUser>, String> {
    let token_file = config.auth_token_file.as_ref()
        .ok_or("auth_token_file not set")?;
    let content = std::fs::read_to_string(token_file)
        .map_err(|e| format!("read token file: {e}"))?;
    let mut users = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        if parts.len() != 3 {
            continue;
        }
        let token = parts[0];
        let username = parts[1];
        let phone = parts[2];
        memory.upsert_signal_user_phone(username, phone).await
            .map_err(|e| format!("upsert signal user: {e}"))?;
        users.push(TokenUser {
            username: username.to_string(),
            phone: phone.to_string(),
            token: token.to_string(),
        });
    }
    tracing::info!("Loaded {} signal users from token file", users.len());
    Ok(users)
}
