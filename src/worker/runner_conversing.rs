//! Worker の conversing タスク管理メソッド群（impl 分散）
//!
//! 分類ディスパッチ・会話開始・会話継続を担当。

use std::sync::Arc;

use anyhow::Result;

use crate::db::CodingTask;

use super::classify::TaskClassification;
use super::{context, runner::{Worker, build_conversing_blocks}};

impl Worker {
    /// conversing タスクの継続処理。ユーザー返信があれば次の Claude ターンを spawn する。
    pub(crate) fn process_conversing_tasks(self: &Arc<Self>) -> bool {
        match self.db.get_conversing_tasks_needing_response() {
            Ok(tasks) => {
                for task in tasks {
                    let task_id = task.id;
                    self.spawn_task(task_id, |w| async move {
                        w.continue_conversing_task(task).await
                    });
                }
                false
            }
            Err(e) => {
                tracing::error!("Failed to fetch conversing tasks: {}", e);
                true
            }
        }
    }

    /// LLM で分類し、結果に基づいて executing or conversing にディスパッチ
    ///
    /// heuristics で暫定ステータスが設定済み。LLM 結果で変更が必要なら更新する。
    pub(crate) async fn classify_and_dispatch(&self, task: CodingTask) -> Result<()> {
        let log_dir = self.log_dir();
        let classification = super::classify::classify_new_task_llm(
            &task, &self.repos_config, &self.db, &self.runner_ctx, &log_dir,
        ).await;

        // 分類結果を記録
        let class_str = match classification {
            TaskClassification::Execute => "execute",
            TaskClassification::Converse => "converse",
        };
        self.db.set_initial_classification(task.id, class_str).ok();

        match classification {
            TaskClassification::Execute => {
                // heuristics で conversing にしていた場合は executing に修正
                if task.status != "executing" {
                    self.db.update_status(task.id, "executing")?;
                }
                self.execute_task(task).await
            }
            TaskClassification::Converse => {
                // heuristics で executing にしていた場合は conversing に修正
                if task.status != "conversing" {
                    self.db.update_status(task.id, "conversing")?;
                }
                self.start_conversing_task(task).await
            }
        }
    }

    /// conversing 開始: Slack スレッド作成 + 要件ヒアリング質問生成
    async fn start_conversing_task(&self, task: CodingTask) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);

        // 1. Slack スレッドを作成（まだなければ）
        let thread_ts = if let Some(ref existing_ts) = task.slack_thread_ts {
            existing_ts.clone()
        } else {
            let msg = format!(
                ":speech_balloon: *要件ヒアリング開始*\n*{}*",
                task.asana_task_name
            );
            let ts = self.slack.post_message(channel, &msg).await?;
            self.db.update_slack_thread(task.id, channel, &ts)?;
            ts
        };

        // 2. converse_thread_ts を設定
        self.db.update_converse_thread_ts(task.id, &thread_ts)?;

        // 3. ops_contexts にタスク情報を追加
        let repo_key = task.repo_key.as_deref().unwrap_or("default");
        let desc = task.description.as_deref().unwrap_or("(なし)");
        let initial_context = format!("タスク: {}\n説明: {}", task.asana_task_name, desc);
        self.db.append_ops_context(channel, &thread_ts, repo_key, "user", &initial_context)?;

        // 4. 要件ヒアリング質問を生成（入口に関わらず汎用プロンプト）
        let log_dir = self.log_dir();
        let prompt = format!(
            "以下のタスクを実装するために、不明点や確認事項を洗い出してください。\n\n\
             ## タスク\n{}\n\n## 説明\n{}\n\n\
             ## 指示\n\
             - コードベースを調査して、実装に必要な情報を整理してください\n\
             - 不明点があれば具体的な質問を箇条書きで出力してください\n\
             - 要件が十分に明確であれば `REQUIREMENTS_CONFIRMED: 要件の要約` を出力してください\n\
             - 質問は最大5個まで。最も重要なものから順に",
            task.asana_task_name, desc
        );

        let repo_path = self.resolve_repo_path(&task).unwrap_or_else(|_| {
            std::path::PathBuf::from(&self.repos_config.defaults.repos_base_dir)
        });
        let base_dir = &self.repos_config.defaults.repos_base_dir;
        let soul = context::merged_soul(base_dir, Some(&repo_path));

        let result = crate::claude::ClaudeRunner::new("conversing", &prompt)
            .system_prompt(&soul)
            .max_turns(3)
            .cwd(&repo_path)
            .log_dir(&log_dir)
            .with_context(&self.runner_ctx)
            .run()
            .await?;

        let output = result.stdout;

        // 5. assistant 出力を保存
        self.db.append_ops_context(channel, &thread_ts, repo_key, "assistant", &output)?;

        // 6. REQUIREMENTS_CONFIRMED: が出たら即 executing に遷移（明確だった場合）
        if output.lines().any(|l| l.trim().starts_with("REQUIREMENTS_CONFIRMED:")) {
            self.db.update_status(task.id, "executing")?;
            self.db.update_analysis(task.id, &output)?;
            self.slack.reply_thread(channel, &thread_ts,
                ":white_check_mark: 要件が確認できました。実行を開始します...").await.ok();
            tracing::info!("Task {} requirements confirmed at first turn, → executing", task.id);
            return Ok(());
        }

        // 7. 質問を Slack スレッドに投稿（conversing ボタン付き）
        let truncated = crate::claude::truncate_str(&output, 2800);
        let blocks = build_conversing_blocks(task.id, truncated);
        self.slack.post_blocks(channel, &thread_ts, &blocks,
            &format!(":speech_balloon: {}", truncated)).await?;

        tracing::info!("Task {} is now conversing, waiting for user reply", task.id);
        Ok(())
    }

    /// conversing 継続: ユーザー返信を受けて次の Claude ターンを実行
    async fn continue_conversing_task(&self, task: CodingTask) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);
        let thread_ts = task.converse_thread_ts.as_deref()
            .or(task.slack_thread_ts.as_deref())
            .ok_or_else(|| anyhow::anyhow!("No converse_thread_ts for task {}", task.id))?;

        let repo_key = task.repo_key.as_deref().unwrap_or("default");
        let history = self.db.get_ops_context(channel, thread_ts)?;

        // 会話履歴をプロンプトに組み込む
        let history_text: String = history.iter()
            .map(|m| format!("[{}] {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n\n");

        let prompt = format!(
            "以下のタスクについて要件ヒアリングを継続してください。\n\n\
             ## タスク\n{}\n\n## 会話履歴\n{}\n\n\
             ## 指示\n\
             - ユーザーの最新の返信を踏まえて、追加の質問があれば出力してください\n\
             - 要件が十分に明確になったら `REQUIREMENTS_CONFIRMED: 要件の要約` を出力してください\n\
             - 質問は具体的に、最大3個まで",
            task.asana_task_name, history_text
        );

        let repo_path = self.resolve_repo_path(&task).unwrap_or_else(|_| {
            std::path::PathBuf::from(&self.repos_config.defaults.repos_base_dir)
        });
        let base_dir = &self.repos_config.defaults.repos_base_dir;
        let soul = context::merged_soul(base_dir, Some(&repo_path));
        let log_dir = self.log_dir();

        let result = crate::claude::ClaudeRunner::new("conversing", &prompt)
            .system_prompt(&soul)
            .max_turns(3)
            .cwd(&repo_path)
            .log_dir(&log_dir)
            .with_context(&self.runner_ctx)
            .run()
            .await?;

        let output = result.stdout;

        // assistant 出力を保存
        self.db.append_ops_context(channel, thread_ts, repo_key, "assistant", &output)?;

        // REQUIREMENTS_CONFIRMED: 検出 → 自動的に executing に遷移（行頭マッチで誤検知防止）
        if output.lines().any(|l| l.trim().starts_with("REQUIREMENTS_CONFIRMED:")) {
            self.db.update_status(task.id, "executing")?;
            self.db.update_analysis(task.id, &output)?;
            self.slack.reply_thread(channel, thread_ts,
                ":white_check_mark: 要件が確定しました。実行を開始します...").await.ok();
            return Ok(());
        }

        // 通常の会話継続: 応答を Slack に投稿（conversing ボタン付き）
        let truncated = crate::claude::truncate_str(&output, 2800);
        let blocks = build_conversing_blocks(task.id, truncated);
        self.slack.post_blocks(channel, thread_ts, &blocks,
            &format!(":speech_balloon: {}", truncated)).await?;

        Ok(())
    }
}
