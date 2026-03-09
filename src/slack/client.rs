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

    /// Block Kit ブロック付きでスレッドに投稿
    pub async fn post_blocks(
        &self,
        channel: &str,
        thread_ts: &str,
        blocks: &serde_json::Value,
        text: &str,
    ) -> Result<String> {
        let body = serde_json::json!({
            "channel": channel,
            "thread_ts": thread_ts,
            "blocks": blocks,
            "text": text,
        });

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

    /// Block Kit メッセージを更新
    pub async fn update_blocks(
        &self,
        channel: &str,
        ts: &str,
        blocks: &serde_json::Value,
        text: &str,
    ) -> Result<()> {
        let body = serde_json::json!({
            "channel": channel,
            "ts": ts,
            "blocks": blocks,
            "text": text,
        });

        let resp = self
            .client
            .post("https://slack.com/api/chat.update")
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

        Ok(())
    }

    /// Slack ファイルをダウンロードしてローカルに保存
    pub async fn download_file(&self, url: &str, dest: &std::path::Path) -> Result<()> {
        let resp = self
            .client
            .get(url)
            .header("Authorization", format!("Bearer {}", self.config.bot_token))
            .send()
            .await
            .context("Failed to download Slack file")?;

        if !resp.status().is_success() {
            anyhow::bail!("Slack file download failed: HTTP {}", resp.status());
        }

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create dir: {}", parent.display()))?;
        }

        let bytes = resp.bytes().await.context("Failed to read file bytes")?;
        tokio::fs::write(dest, &bytes)
            .await
            .with_context(|| format!("Failed to write file: {}", dest.display()))?;

        tracing::info!("Downloaded Slack file to {}", dest.display());
        Ok(())
    }

    /// conversations.history でメッセージ1件を取得（リアクション→ops 用）
    pub async fn fetch_message(&self, channel: &str, ts: &str) -> Result<serde_json::Value> {
        let resp = self
            .client
            .get("https://slack.com/api/conversations.history")
            .header("Authorization", format!("Bearer {}", self.config.bot_token))
            .query(&[
                ("channel", channel),
                ("latest", ts),
                ("inclusive", "true"),
                ("limit", "1"),
            ])
            .send()
            .await
            .context("Slack conversations.history request failed")?;

        let data: serde_json::Value = resp.json().await.context("Failed to parse response")?;

        if data.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let err = data.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
            anyhow::bail!("conversations.history error: {}", err);
        }

        data.get("messages")
            .and_then(|m| m.as_array())
            .and_then(|a| a.first())
            .cloned()
            .context("No message found")
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
