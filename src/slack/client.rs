use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::SlackConfig;
use super::mrkdwn::markdown_to_mrkdwn;

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
        let converted_text = markdown_to_mrkdwn(text);
        let converted_blocks = convert_blocks_text(blocks);
        let body = serde_json::json!({
            "channel": channel,
            "thread_ts": thread_ts,
            "blocks": converted_blocks,
            "text": converted_text,
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
        let converted_text = markdown_to_mrkdwn(text);
        let converted_blocks = convert_blocks_text(blocks);
        let body = serde_json::json!({
            "channel": channel,
            "ts": ts,
            "blocks": converted_blocks,
            "text": converted_text,
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

    fn check_ok(data: &serde_json::Value, api_name: &str) -> Result<()> {
        if data.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let err = data.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
            anyhow::bail!("{} error: {}", api_name, err);
        }
        Ok(())
    }

    /// auth.test でボットの user ID を取得
    pub async fn fetch_bot_user_id(&self) -> Result<String> {
        let resp = self
            .client
            .post("https://slack.com/api/auth.test")
            .header("Authorization", format!("Bearer {}", self.config.bot_token))
            .send()
            .await
            .context("Slack auth.test request failed")?;

        let data: serde_json::Value = resp.json().await?;
        Self::check_ok(&data, "auth.test")?;

        data.get("user_id")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .context("No user_id in auth.test response")
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
        Self::check_ok(&data, "conversations.history")?;

        data.get("messages")
            .and_then(|m| m.as_array())
            .and_then(|a| a.first())
            .cloned()
            .context("No message found")
    }

    /// conversations.replies でスレッドの全メッセージを取得
    pub async fn fetch_thread_replies(&self, channel: &str, thread_ts: &str) -> Result<Vec<serde_json::Value>> {
        let resp = self
            .client
            .get("https://slack.com/api/conversations.replies")
            .header("Authorization", format!("Bearer {}", self.config.bot_token))
            .query(&[
                ("channel", channel),
                ("ts", thread_ts),
                ("limit", "50"),
            ])
            .send()
            .await
            .context("Slack conversations.replies request failed")?;

        let data: serde_json::Value = resp.json().await.context("Failed to parse response")?;
        Self::check_ok(&data, "conversations.replies")?;

        Ok(data.get("messages")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// ボットが参加しているチャンネル一覧を取得 (name → id)
    pub async fn fetch_bot_channels(&self) -> Result<std::collections::HashMap<String, String>> {
        let mut channels = std::collections::HashMap::new();
        let mut cursor = String::new();

        loop {
            let mut query = vec![
                ("types", "public_channel,private_channel"),
                ("exclude_archived", "true"),
                ("limit", "200"),
            ];
            if !cursor.is_empty() {
                query.push(("cursor", &cursor));
            }

            let resp = self
                .client
                .get("https://slack.com/api/users.conversations")
                .header("Authorization", format!("Bearer {}", self.config.bot_token))
                .query(&query)
                .send()
                .await
                .context("Slack users.conversations request failed")?;

            let data: serde_json::Value = resp.json().await?;
            Self::check_ok(&data, "users.conversations")?;

            if let Some(arr) = data.get("channels").and_then(|c| c.as_array()) {
                for ch in arr {
                    if let (Some(id), Some(name)) = (
                        ch.get("id").and_then(|v| v.as_str()),
                        ch.get("name").and_then(|v| v.as_str()),
                    ) {
                        channels.insert(name.to_string(), id.to_string());
                    }
                }
            }

            // ページネーション
            let next = data
                .get("response_metadata")
                .and_then(|m| m.get("next_cursor"))
                .and_then(|c| c.as_str())
                .unwrap_or("");
            if next.is_empty() {
                break;
            }
            cursor = next.to_string();
        }

        Ok(channels)
    }

    async fn send_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String> {
        let converted = markdown_to_mrkdwn(text);
        let body = PostMessageRequest {
            channel,
            text: &converted,
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

/// Block Kit JSON 内の section/header ブロックの text フィールドを mrkdwn 変換
fn convert_blocks_text(blocks: &serde_json::Value) -> serde_json::Value {
    match blocks {
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(convert_block_element).collect())
        }
        _ => blocks.clone(),
    }
}

fn convert_block_element(block: &serde_json::Value) -> serde_json::Value {
    let mut b = block.clone();
    if let Some(obj) = b.as_object_mut() {
        // section / header の text.text を変換 (type=mrkdwn のもの)
        if let Some(text_obj) = obj.get_mut("text") {
            if let Some(inner) = text_obj.as_object_mut() {
                let is_mrkdwn = inner
                    .get("type")
                    .and_then(|t| t.as_str())
                    .is_none_or(|t| t == "mrkdwn");
                if is_mrkdwn {
                    if let Some(serde_json::Value::String(s)) = inner.get("text") {
                        inner.insert(
                            "text".to_string(),
                            serde_json::Value::String(markdown_to_mrkdwn(s)),
                        );
                    }
                }
            }
        }
        // fields 配列内のテキストも変換
        if let Some(serde_json::Value::Array(fields)) = obj.get_mut("fields") {
            for field in fields.iter_mut() {
                if let Some(inner) = field.as_object_mut() {
                    let is_mrkdwn = inner
                        .get("type")
                        .and_then(|t| t.as_str())
                        .is_none_or(|t| t == "mrkdwn");
                    if is_mrkdwn {
                        if let Some(serde_json::Value::String(s)) = inner.get("text") {
                            inner.insert(
                                "text".to_string(),
                                serde_json::Value::String(markdown_to_mrkdwn(s)),
                            );
                        }
                    }
                }
            }
        }
    }
    b
}
