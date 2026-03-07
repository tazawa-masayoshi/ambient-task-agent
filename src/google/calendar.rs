use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, Utc};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
    token_uri: String,
}

#[derive(Debug, Serialize)]
struct JwtClaims {
    iss: String,
    scope: String,
    aud: String,
    iat: i64,
    exp: i64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    pub summary: Option<String>,
    pub start: EventDateTime,
    pub end: EventDateTime,
    #[serde(rename = "htmlLink")]
    pub html_link: Option<String>,
    #[serde(rename = "conferenceData")]
    pub conference_data: Option<ConferenceData>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDateTime {
    #[serde(rename = "dateTime")]
    pub date_time: Option<String>,
    pub date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConferenceData {
    #[serde(rename = "entryPoints")]
    pub entry_points: Option<Vec<EntryPoint>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryPoint {
    #[serde(rename = "entryPointType")]
    pub entry_point_type: Option<String>,
    pub uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventsListResponse {
    items: Option<Vec<CalendarEvent>>,
}

pub struct GoogleCalendarClient {
    client: reqwest::Client,
    service_account_key: ServiceAccountKey,
    calendar_id: String,
    cached_token: Option<(String, DateTime<Utc>)>,
}

impl GoogleCalendarClient {
    pub fn new(key_path: &str, calendar_id: &str) -> Result<Self> {
        let key_json = std::fs::read_to_string(key_path)
            .with_context(|| format!("Failed to read service account key: {}", key_path))?;
        let key: ServiceAccountKey = serde_json::from_str(&key_json)
            .context("Failed to parse service account key JSON")?;

        // token_uri を oauth2.googleapis.com に制限（SSRF 防止）
        anyhow::ensure!(
            key.token_uri.starts_with("https://oauth2.googleapis.com/"),
            "token_uri must be https://oauth2.googleapis.com/*, got: {}",
            key.token_uri
        );

        Ok(Self {
            client: reqwest::Client::new(),
            service_account_key: key,
            calendar_id: calendar_id.to_string(),
            cached_token: None,
        })
    }

    pub async fn fetch_today_events(&mut self) -> Result<Vec<CalendarEvent>> {
        let token = self.get_access_token().await?;

        let now = Local::now();
        let today_start = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(now.timezone())
            .unwrap();
        let today_end = today_start + Duration::days(1);

        let time_min = today_start.to_rfc3339();
        let time_max = today_end.to_rfc3339();

        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/{}/events",
            urlencoded(&self.calendar_id)
        );

        let resp = self
            .client
            .get(&url)
            .bearer_auth(&token)
            .query(&[
                ("timeMin", time_min.as_str()),
                ("timeMax", time_max.as_str()),
                ("singleEvents", "true"),
                ("orderBy", "startTime"),
                ("maxResults", "50"),
            ])
            .send()
            .await
            .context("Failed to call Google Calendar API")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Google Calendar API error ({}): {}", status, body);
        }

        let data: EventsListResponse = resp.json().await.context("Failed to parse events response")?;

        let events = data
            .items
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.status.as_deref() != Some("cancelled"))
            .collect();

        Ok(events)
    }

    /// カレンダーにイベントを作成
    pub async fn create_event(
        &mut self,
        summary: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        description: Option<&str>,
    ) -> Result<String> {
        let token = self.get_access_token().await?;

        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/{}/events",
            urlencoded(&self.calendar_id)
        );

        let mut body = serde_json::json!({
            "summary": summary,
            "start": { "dateTime": start.to_rfc3339() },
            "end": { "dateTime": end.to_rfc3339() },
            "transparency": "transparent",
        });
        if let Some(desc) = description {
            body["description"] = serde_json::Value::String(desc.to_string());
        }

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .context("Failed to create calendar event")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Google Calendar create event error ({}): {}", status, body);
        }

        let event: CalendarEvent = resp.json().await.context("Failed to parse created event")?;
        Ok(event.id)
    }

    /// 指定プレフィックスで始まるイベントを今日分だけ削除
    pub async fn delete_events_by_summary_prefix(&mut self, prefix: &str) -> Result<usize> {
        let events = self.fetch_today_events().await?;
        let mut deleted = 0;

        for event in &events {
            let summary = event.summary.as_deref().unwrap_or("");
            if summary.starts_with(prefix) {
                if let Err(e) = self.delete_event(&event.id).await {
                    tracing::warn!("Failed to delete calendar event {}: {}", event.id, e);
                } else {
                    deleted += 1;
                }
            }
        }

        Ok(deleted)
    }

    async fn delete_event(&mut self, event_id: &str) -> Result<()> {
        let token = self.get_access_token().await?;

        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/{}/events/{}",
            urlencoded(&self.calendar_id),
            urlencoded(event_id),
        );

        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("Failed to delete calendar event")?;

        if !resp.status().is_success() && resp.status().as_u16() != 410 {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Google Calendar delete event error ({}): {}", status, body);
        }

        Ok(())
    }

    async fn get_access_token(&mut self) -> Result<String> {
        if let Some((ref token, ref expires_at)) = self.cached_token {
            if Utc::now() < *expires_at - Duration::minutes(5) {
                return Ok(token.clone());
            }
        }

        let jwt = self.create_jwt()?;

        let resp = self
            .client
            .post(&self.service_account_key.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &jwt),
            ])
            .send()
            .await
            .context("Failed to request access token")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Token request failed ({}): {}", status, body);
        }

        let token_resp: TokenResponse = resp.json().await.context("Failed to parse token response")?;

        let expires_at = Utc::now() + Duration::seconds(token_resp.expires_in as i64);
        self.cached_token = Some((token_resp.access_token.clone(), expires_at));

        Ok(token_resp.access_token)
    }

    fn create_jwt(&self) -> Result<String> {
        let now = Utc::now().timestamp();
        let claims = JwtClaims {
            iss: self.service_account_key.client_email.clone(),
            scope: "https://www.googleapis.com/auth/calendar.events".to_string(),
            aud: self.service_account_key.token_uri.clone(),
            iat: now,
            exp: now + 3600,
        };

        let header = Header::new(Algorithm::RS256);
        let key = EncodingKey::from_rsa_pem(self.service_account_key.private_key.as_bytes())
            .context("Failed to parse RSA private key")?;

        encode(&header, &claims, &key).context("Failed to create JWT")
    }
}

impl CalendarEvent {
    pub fn start_time(&self) -> Option<DateTime<Utc>> {
        self.start
            .date_time
            .as_ref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
    }

    pub fn end_time(&self) -> Option<DateTime<Utc>> {
        self.end
            .date_time
            .as_ref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
    }

    pub fn is_all_day(&self) -> bool {
        self.start.date.is_some() && self.start.date_time.is_none()
    }

    pub fn meet_link(&self) -> Option<&str> {
        self.conference_data
            .as_ref()?
            .entry_points
            .as_ref()?
            .iter()
            .find(|ep| ep.entry_point_type.as_deref() == Some("video"))
            .and_then(|ep| ep.uri.as_deref())
    }
}

fn urlencoded(s: &str) -> String {
    s.replace('@', "%40").replace('#', "%23")
}
