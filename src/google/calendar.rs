use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, Utc};
use serde::{Deserialize, Serialize};

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
struct GwsEventsResponse {
    items: Option<Vec<CalendarEvent>>,
}

pub struct GoogleCalendarClient {
    gws_path: String,
    calendar_id: String,
}

impl GoogleCalendarClient {
    pub fn new(gws_path: &str, calendar_id: &str) -> Result<Self> {
        // gws バイナリの存在確認
        let path = std::path::Path::new(gws_path);
        anyhow::ensure!(
            path.exists(),
            "gws binary not found at: {}",
            gws_path
        );

        Ok(Self {
            gws_path: gws_path.to_string(),
            calendar_id: calendar_id.to_string(),
        })
    }

    pub async fn fetch_today_events(&mut self) -> Result<Vec<CalendarEvent>> {
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

        let params = serde_json::json!({
            "calendarId": self.calendar_id,
            "timeMin": time_min,
            "timeMax": time_max,
            "singleEvents": true,
            "orderBy": "startTime",
            "maxResults": 50,
        });

        let output = self
            .run_gws(&["calendar", "events", "list", "--params", &params.to_string()])
            .await?;

        let data: GwsEventsResponse =
            serde_json::from_str(&output).context("Failed to parse gws events response")?;

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
        let mut body = serde_json::json!({
            "summary": summary,
            "start": { "dateTime": start.to_rfc3339() },
            "end": { "dateTime": end.to_rfc3339() },
            "transparency": "transparent",
        });
        if let Some(desc) = description {
            body["description"] = serde_json::Value::String(desc.to_string());
        }

        let params = serde_json::json!({
            "calendarId": self.calendar_id,
        });

        let output = self
            .run_gws(&[
                "calendar",
                "events",
                "insert",
                "--params",
                &params.to_string(),
                "--json",
                &body.to_string(),
            ])
            .await?;

        let event: CalendarEvent =
            serde_json::from_str(&output).context("Failed to parse created event")?;
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
        let params = serde_json::json!({
            "calendarId": self.calendar_id,
            "eventId": event_id,
        });

        self.run_gws(&[
            "calendar",
            "events",
            "delete",
            "--params",
            &params.to_string(),
        ])
        .await?;

        Ok(())
    }

    /// gws CLI をサブプロセスとして実行
    async fn run_gws(&self, args: &[&str]) -> Result<String> {
        let output = tokio::process::Command::new(&self.gws_path)
            .args(args)
            .output()
            .await
            .with_context(|| format!("Failed to execute gws: {}", self.gws_path))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            anyhow::bail!(
                "gws command failed (exit {}): {}{}",
                output.status.code().unwrap_or(-1),
                stderr,
                stdout
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
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
