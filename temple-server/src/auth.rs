use crate::memory::Memory;
use std::path::Path;

/// A user authenticated via token.
#[allow(dead_code)] // admin/priority consumed by queue routing on other paths.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub username: String,
    pub phone: String,
    pub admin: bool,
    /// Request-queue priority — lower runs first (0 ethan, 1 valarie,
    /// -1 default). Field 5 in the token file; -1 when absent.
    pub priority: i32,
}

/// Parse the optional trailing fields of a token-file line:
/// `token:username:phone[:admin[:priority]]`
fn parse_extras(parts: &[&str]) -> (bool, i32) {
    let admin = parts
        .get(3)
        .map(|a| a.trim().eq_ignore_ascii_case("yes"))
        .unwrap_or(false);
    let priority = parts
        .get(4)
        .and_then(|p| p.trim().parse::<i32>().ok())
        .unwrap_or(-1);
    (admin, priority)
}

/// Constant-time token comparison — a short-circuiting byte compare leaks
/// the match position over the WebSocket handshake timing.
fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Check a token against the auth_token_file.
pub fn check_token(token_file: &Path, token: &str) -> Result<AuthUser, String> {
    let content =
        std::fs::read_to_string(token_file).map_err(|e| format!("read token file: {e}"))?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(5, ':').collect();
        if parts.len() < 3 {
            continue;
        }
        if token_eq(parts[0], token) {
            let (admin, priority) = parse_extras(&parts);
            return Ok(AuthUser {
                username: parts[1].to_string(),
                phone: parts[2].to_string(),
                admin,
                priority,
            });
        }
    }
    Err("invalid token".into())
}

/// A loaded user from the token file — used for sending welcome messages.
#[allow(dead_code)] // token/admin/priority used when generating + verifying.
#[derive(Debug, Clone)]
pub struct TokenUser {
    pub username: String,
    pub phone: String,
    pub token: String,
    pub admin: bool,
    pub priority: i32,
}

/// Load all users from the auth_token_file into the signal_users DB table.
pub async fn load_signal_users(
    memory: &Memory,
    config: &crate::config::Config,
) -> Result<Vec<TokenUser>, String> {
    let token_file = config
        .auth_token_file
        .as_ref()
        .ok_or("auth_token_file not set")?;
    let content =
        std::fs::read_to_string(token_file).map_err(|e| format!("read token file: {e}"))?;
    let mut users = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(5, ':').collect();
        if parts.len() < 3 {
            continue;
        }
        let token = parts[0];
        let username = parts[1];
        let phone = parts[2];
        // Field-order mistakes (admin/priority in the phone slot, phone
        // tacked on the end) silently corrupt the DB sync — warn loudly.
        if !phone.starts_with('+') {
            tracing::warn!(
                "token file: user {username} has phone field '{phone}' — \
                 expected token:username:phone[:admin[:priority]]"
            );
        }
        if let Some(a) = parts.get(3) {
            let a = a.trim();
            if !a.eq_ignore_ascii_case("yes") && !a.eq_ignore_ascii_case("no") {
                tracing::warn!(
                    "token file: user {username} field 4 is '{a}' — expected 'yes' or 'no'"
                );
            }
        }
        if let Some(p) = parts.get(4) {
            if p.trim().parse::<i32>().is_err() {
                tracing::warn!(
                    "token file: user {username} field 5 is '{}' — expected an integer priority",
                    p.trim()
                );
            }
        }
        let (admin, priority) = parse_extras(&parts);
        memory
            .upsert_signal_user_phone(username, phone, admin, priority)
            .await
            .map_err(|e| format!("upsert signal user: {e}"))?;
        users.push(TokenUser {
            username: username.to_string(),
            phone: phone.to_string(),
            token: token.to_string(),
            admin,
            priority,
        });
    }
    tracing::info!("Loaded {} signal users from token file", users.len());
    Ok(users)
}
