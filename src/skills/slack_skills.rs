use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

use crate::slack::client::SlackClient;
use super::{Skill, SkillResult};

/// Slackにメッセージを投稿するスキル
pub struct PostSlackMessage {
    client: Arc<SlackClient>,
}

impl PostSlackMessage {
    pub fn new(client: Arc<SlackClient>) -> Self {
        Self { client }
    }
}

#[derive(Debug, Deserialize)]
struct PostSlackMessageParams {
    channel: String,
    text: String,
}

#[async_trait]
impl Skill for PostSlackMessage {
    fn name(&self) -> &str {
        "post_slack_message"
    }

    fn description(&self) -> &str {
        "Slackチャンネルにメッセージを投稿する。進捗報告、通知、確認依頼などに使用。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "投稿先のチャンネルID（例: C01234567）"
                },
                "text": {
                    "type": "string",
                    "description": "投稿するメッセージ本文"
                }
            },
            "required": ["channel", "text"]
        })
    }

    async fn execute(&self, params: Value) -> Result<SkillResult> {
        let p: PostSlackMessageParams = serde_json::from_value(params)?;

        match self.client.post_message(&p.channel, &p.text).await {
            Ok(ts) => Ok(SkillResult::ok_with_data(
                format!("メッセージを投稿しました"),
                serde_json::json!({ "ts": ts, "channel": p.channel }),
            )),
            Err(e) => Ok(SkillResult::error(format!("投稿失敗: {}", e))),
        }
    }
}

/// Slackスレッドに返信するスキル
pub struct ReplySlackThread {
    client: Arc<SlackClient>,
}

impl ReplySlackThread {
    pub fn new(client: Arc<SlackClient>) -> Self {
        Self { client }
    }
}

#[derive(Debug, Deserialize)]
struct ReplySlackThreadParams {
    channel: String,
    thread_ts: String,
    text: String,
}

#[async_trait]
impl Skill for ReplySlackThread {
    fn name(&self) -> &str {
        "reply_slack_thread"
    }

    fn description(&self) -> &str {
        "Slackのスレッドに返信する。タスク進捗の更新、質問への回答などに使用。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "チャンネルID"
                },
                "thread_ts": {
                    "type": "string",
                    "description": "スレッドの親メッセージのtimestamp"
                },
                "text": {
                    "type": "string",
                    "description": "返信するメッセージ本文"
                }
            },
            "required": ["channel", "thread_ts", "text"]
        })
    }

    async fn execute(&self, params: Value) -> Result<SkillResult> {
        let p: ReplySlackThreadParams = serde_json::from_value(params)?;

        match self.client.reply_thread(&p.channel, &p.thread_ts, &p.text).await {
            Ok(ts) => Ok(SkillResult::ok_with_data(
                format!("スレッドに返信しました"),
                serde_json::json!({ "ts": ts }),
            )),
            Err(e) => Ok(SkillResult::error(format!("返信失敗: {}", e))),
        }
    }
}
