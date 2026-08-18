use crate::config::NextcloudConfig;
use reqwest::Client;

pub struct Nextcloud {
    #[allow(dead_code)] // Reserved for calendar-aware system prompts.
    client: Client,
    config: NextcloudConfig,
}

impl Nextcloud {
    pub fn new(config: &NextcloudConfig) -> Self {
        Self {
            client: Client::new(),
            config: config.clone(),
        }
    }

    /// Check if Nextcloud integration is enabled
    #[allow(dead_code)] // Calendar integration is wired in a later phase.
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Get calendar events for today
    #[allow(dead_code)]
    pub async fn get_today_events(&self) -> Result<Vec<String>, String> {
        if !self.config.enabled {
            return Ok(Vec::new());
        }
        let base = self.config.server_url.trim_end_matches('/');
        let url = format!(
            "{}/remote.php/dav/calendars/{}/renco_events?export",
            base, self.config.username
        );

        let pass = self.config.password.as_deref().unwrap_or("").to_string();

        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.config.username, Some(&pass))
            .send()
            .await
            .map_err(|e| format!("Nextcloud request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("Nextcloud error: {}", resp.status()));
        }
        let ics = resp.text().await.unwrap_or_default();
        // Simple ICS parsing — extract summaries
        let events: Vec<String> = ics
            .lines()
            .filter(|l| l.starts_with("SUMMARY:"))
            .map(|l| l["SUMMARY:".len()..].to_string())
            .collect();
        Ok(events)
    }

    /// Create a calendar event
    #[allow(dead_code)]
    pub async fn create_event(&self, summary: &str, date: &str) -> Result<(), String> {
        if !self.config.enabled {
            return Ok(());
        }
        let base = self.config.server_url.trim_end_matches('/');
        let url = format!(
            "{}/remote.php/dav/calendars/{}/renco_events/",
            base, self.config.username
        );

        let pass = self.config.password.as_deref().unwrap_or("").to_string();

        let ics = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nDTSTART;VALUE=DATE:{}\r\nSUMMARY:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            date.replace('-', ""),
            summary
        );

        let resp = self
            .client
            .post(&url)
            .basic_auth(&self.config.username, Some(&pass))
            .header("Content-Type", "text/calendar; charset=utf-8")
            .body(ics)
            .send()
            .await
            .map_err(|e| format!("Nextcloud create event failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("Nextcloud event create error: {}", resp.status()));
        }
        Ok(())
    }
}
