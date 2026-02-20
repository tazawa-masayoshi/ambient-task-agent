use std::time::Duration;

use anyhow::Result;

use crate::asana::client::AsanaClient;
use crate::config::AsanaConfig;
use crate::db::{CodingTask, Db};
use crate::repo_config::ReposConfig;
use crate::slack::client::SlackClient;

use super::planner;

pub struct Worker {
    db: Db,
    repos_config: ReposConfig,
    slack: SlackClient,
    asana_pat: String,
    default_slack_channel: String,
}

impl Worker {
    pub fn new(
        db: Db,
        repos_config: ReposConfig,
        slack: SlackClient,
        asana_pat: String,
        default_slack_channel: String,
    ) -> Self {
        Self {
            db,
            repos_config,
            slack,
            asana_pat,
            default_slack_channel,
        }
    }

    /// メインワーカーループ: pending タスクをポーリングして処理
    pub async fn run(&self) {
        tracing::info!("Worker started");
        loop {
            match self.db.get_pending_task() {
                Ok(Some(task)) => {
                    tracing::info!("Processing task: {} ({})", task.asana_task_name, task.asana_task_gid);
                    if let Err(e) = self.process_task(task).await {
                        tracing::error!("Task processing failed: {}", e);
                    }
                }
                Ok(None) => {
                    // キューが空。少し待つ
                }
                Err(e) => {
                    tracing::error!("Failed to fetch pending task: {}", e);
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn process_task(&self, task: CodingTask) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);

        // Step 1: Slack 親メッセージ送信
        let parent_msg = format!(
            ":inbox_tray: タスクを受信しました\n*{}*\nhttps://app.asana.com/0/0/{}",
            task.asana_task_name, task.asana_task_gid
        );
        let thread_ts = match self.slack.post_message(channel, &parent_msg).await {
            Ok(ts) => {
                self.db
                    .update_slack_thread(task.id, channel, &ts)?;
                ts
            }
            Err(e) => {
                tracing::error!("Failed to post Slack message: {}", e);
                self.db.set_error(task.id, &format!("Slack post failed: {}", e))?;
                return Err(e);
            }
        };

        // Step 2: ステータスを planning に
        self.db.update_status(task.id, "planning")?;

        // Step 3: リポジトリパスを解決
        let repo_path = match task.repo_key.as_deref() {
            Some(key) => {
                let repo = self
                    .repos_config
                    .repo
                    .iter()
                    .find(|r| r.key == key);
                match repo {
                    Some(r) => self.repos_config.repo_local_path(r),
                    None => {
                        let err = format!("Unknown repo key: {}", key);
                        self.db.set_error(task.id, &err)?;
                        self.slack
                            .reply_thread(channel, &thread_ts, &format!(":x: エラー: {}", err))
                            .await
                            .ok();
                        anyhow::bail!(err);
                    }
                }
            }
            None => {
                let err = "No repo_key assigned to task";
                self.db.set_error(task.id, err)?;
                self.slack
                    .reply_thread(channel, &thread_ts, &format!(":x: エラー: リポジトリが特定できません"))
                    .await
                    .ok();
                anyhow::bail!(err);
            }
        };

        // Step 4: Asana からタスク詳細（notes）を取得
        let asana_config = AsanaConfig {
            pat: self.asana_pat.clone(),
            project_id: String::new(),
            user_name: String::new(),
        };
        let asana_client = AsanaClient::new(asana_config);
        let asana_task = asana_client.get_task(&task.asana_task_gid).await?;
        let notes = asana_task.notes.as_deref().unwrap_or("");

        // Step 5: claude -p でプラン生成
        self.slack
            .reply_thread(channel, &thread_ts, ":brain: プラン作成中...")
            .await
            .ok();

        let max_turns = self.repos_config.defaults.claude_max_plan_turns;
        match planner::generate_plan(&task.asana_task_name, notes, &repo_path, max_turns).await {
            Ok(plan) => {
                self.db.update_plan(task.id, &plan)?;
                self.db.update_status(task.id, "plan_posted")?;

                // Slack にプラン投稿
                let plan_msg = format!(
                    ":white_check_mark: プランが完成しました\n\n```\n{}\n```",
                    truncate_for_slack(&plan, 3800)
                );
                self.slack
                    .reply_thread(channel, &thread_ts, &plan_msg)
                    .await?;

                tracing::info!("Plan posted for task {}", task.asana_task_gid);
            }
            Err(e) => {
                let err_msg = format!("Plan generation failed: {}", e);
                self.db.set_error(task.id, &err_msg)?;
                self.slack
                    .reply_thread(
                        channel,
                        &thread_ts,
                        &format!(":x: プラン作成に失敗しました\n```\n{}\n```", e),
                    )
                    .await
                    .ok();
                tracing::error!("{}", err_msg);
            }
        }

        Ok(())
    }
}

/// Slack メッセージの文字数制限に合わせてトランケート
fn truncate_for_slack(text: &str, max_len: usize) -> &str {
    if text.len() <= max_len {
        text
    } else {
        // UTF-8 境界で安全に切る
        let mut end = max_len;
        while !text.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &text[..end]
    }
}
