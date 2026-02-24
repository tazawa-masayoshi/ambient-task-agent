use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::SlackConfig;

#[derive(Debug, Clone)]
pub struct SlackClient {
    config: SlackConfig,
    client: Client,
}

#[derive(Debug, Serialize)]
struct PostMessageRequest<'a> {
    channel: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_ts: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct SlackResponse {
    ok: bool,
    error: Option<String>,
    ts: Option<String>,
}

impl SlackClient {
    pub fn new(config: SlackConfig) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to build reqwest client");
        Self { config, client }
    }

    /// Post a message to a channel.
    pub async fn post_message(&self, channel: &str, text: &str) -> Result<String> {
        self.send_message(channel, text, None).await
    }

    /// Reply to a thread.
    pub async fn reply_thread(&self, channel: &str, thread_ts: &str, text: &str) -> Result<String> {
        self.send_message(channel, text, Some(thread_ts)).await
    }

    async fn send_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String> {
        let body = PostMessageRequest {
            channel,
            text,
            thread_ts,
        };

        let resp = self
            .client
            .post("https://slack.com/api/chat.postMessage")
            .header("Authorization", format!("Bearer {}", self.config.bot_token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Slack API request failed")?;

        let status = resp.status();
        let data: SlackResponse = resp.json().await.context("Failed to parse Slack response")?;

        if !data.ok {
            anyhow::bail!(
                "Slack API error ({}): {}",
                status,
                data.error.unwrap_or_else(|| "unknown".to_string())
            );
        }

        Ok(data.ts.unwrap_or_default())
    }
}
