//! Worker の CI チェック・リトライメソッド群（impl 分散）

use anyhow::Result;

use crate::db::CodingTask;

use super::runner::{Worker, truncate_for_slack, ERROR_LOG_HINT};
use super::{context, executor, workspace};

impl Worker {
    /// ci_pending タスクの CI 結果を確認し、完了 or リトライする
    pub(crate) async fn check_ci_and_handle(&self, task: CodingTask) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);
        let thread_ts = task.slack_thread_ts.as_deref().unwrap_or("");
        let base_dir = &self.repos_config.defaults.repos_base_dir;

        let repo_entry = match task
            .repo_key
            .as_deref()
            .and_then(|key| self.repos_config.find_repo_by_key(key))
        {
            Some(r) => r,
            None => {
                tracing::warn!("No repo_entry for ci_pending task {}", task.id);
                return Ok(());
            }
        };

        let branch_name = match task.branch_name.as_deref() {
            Some(b) => b,
            None => {
                tracing::warn!("No branch_name for ci_pending task {}", task.id);
                self.db.update_status(task.id, "done")?;
                return Ok(());
            }
        };

        // CI ステータスを確認
        let ci_status = workspace::check_ci(
            base_dir,
            &repo_entry.key,
            &repo_entry.github,
            branch_name,
        )
        .await?;

        match ci_status {
            workspace::CiStatus::Pending => {
                // まだ実行中 — 次のループで再チェック
                tracing::trace!("CI still pending for task {}", task.id);
            }
            workspace::CiStatus::NotFound => {
                // CI ワークフローがない — そのまま done に
                tracing::info!("No CI workflow found for task {}, marking done", task.id);
                self.db.update_status(task.id, "done")?;

                let repo_path = self.repos_config.repo_local_path(repo_entry);
                context::append_completed_task(base_dir, &task, Some(&repo_path), None);

                self.slack
                    .reply_thread(
                        channel,
                        thread_ts,
                        ":white_check_mark: 完了（CI ワークフローなし）",
                    )
                    .await
                    .ok();
            }
            workspace::CiStatus::Passed => {
                // CI 通過 — done
                self.db.update_status(task.id, "done")?;

                let repo_path = self.repos_config.repo_local_path(repo_entry);
                context::append_completed_task(base_dir, &task, Some(&repo_path), None);

                let pr_url = task.pr_url.as_deref().unwrap_or("(no URL)");
                let msg = format!(
                    ":white_check_mark: CI 通過 — 完了\n{}",
                    pr_url
                );
                self.slack
                    .reply_thread(channel, thread_ts, &msg)
                    .await
                    .ok();
            }
            workspace::CiStatus::Failed { summary } => {
                // CI 失敗 — リトライ可能か判定
                let new_count = self.db.increment_retry_count(task.id)?;
                let max_retry = repo_entry.ci_max_retry;

                if (new_count as u32) > max_retry {
                    // リトライ上限到達
                    self.db
                        .set_error(task.id, &format!("CI failed after {} retries: {}", new_count, summary))?;

                    let msg = format!(
                        ":x: CI 失敗（リトライ上限 {} 回に到達）\n```\n{}\n```{}",
                        max_retry, summary, ERROR_LOG_HINT
                    );
                    self.slack
                        .reply_thread(channel, thread_ts, &msg)
                        .await
                        .ok();
                } else {
                    // リトライ実行
                    tracing::info!(
                        "CI failed for task {} (retry {}/{}), attempting fix",
                        task.id, new_count, max_retry
                    );
                    if let Err(e) = self
                        .retry_ci_failed(&task, repo_entry, &summary)
                        .await
                    {
                        tracing::error!("CI retry failed for task {}: {}", task.id, e);
                        self.db.set_error(task.id, &format!("CI retry error: {}", e))?;
                    }
                }
            }
        }

        Ok(())
    }

    /// CI 失敗時のリトライ: worktree を再作成 → CI エラーをフィードバック → 再実行 → push
    async fn retry_ci_failed(
        &self,
        task: &CodingTask,
        repo_entry: &crate::repo_config::RepoEntry,
        ci_summary: &str,
    ) -> Result<()> {
        let channel = task
            .slack_channel
            .as_deref()
            .unwrap_or(&self.default_slack_channel);
        let thread_ts = task.slack_thread_ts.as_deref().unwrap_or("");
        let base_dir = &self.repos_config.defaults.repos_base_dir;
        let branch_name = task.branch_name.as_deref().unwrap_or("");
        let retry_count = task.retry_count;

        self.slack
            .reply_thread(
                channel,
                thread_ts,
                &format!(
                    ":recycle: CI 失敗を検出 — 自動修正中 (リトライ {})...\n```\n{}\n```",
                    retry_count + 1,
                    truncate_for_slack(ci_summary, 500)
                ),
            )
            .await
            .ok();

        // CI の失敗ログを取得（エージェントへのフィードバック用）
        let ci_log = workspace::get_ci_failure_log(
            base_dir,
            &repo_entry.key,
            &repo_entry.github,
            branch_name,
        )
        .await
        .unwrap_or_else(|_| ci_summary.to_string());

        // 既存ブランチから worktree を再作成
        let ws = workspace::create_for_retry(
            base_dir,
            &repo_entry.key,
            task.id,
            branch_name,
        )
        .await?;

        // CI エラーをフィードバックとしてプロンプトに注入
        let ci_fix_prompt = format!(
            "CI が失敗しました。以下のエラーログを読んで修正してください。\n\
             コードを修正し、テストが通ることを確認してから完了してください。\n\
             リンター設定やテスト設定を変更してはいけません。コードを直してください。\n\n\
             ## CI エラーログ\n```\n{}\n```",
            truncate_for_slack(&ci_log, 2500)
        );

        // executor 実行（CI エラーをプロンプトに含める）
        let max_turns = self.resolve_execute_turns(&ws.worktree_path, task.complexity.as_deref());
        let wc = self.build_worktree_context(&ws, max_turns, false);

        let log_dir = self.log_dir();
        let result = executor::execute_task(
            &format!("[CI FIX] {}", task.asana_task_name),
            &ci_fix_prompt,
            Some(repo_entry),
            Some(ws.worktree_path.as_path()),
            &wc,
            Some(&log_dir),
            &self.runner_ctx,
        )
        .await;

        match result {
            Ok(exec_result) if exec_result.success => {
                // 修正を push
                match workspace::push_retry(&ws).await {
                    Ok(()) => {
                        self.db.update_status(task.id, "ci_pending")?;
                        self.slack
                            .reply_thread(
                                channel,
                                thread_ts,
                                ":gear: CI 修正を push しました — CI 結果を再監視中...",
                            )
                            .await
                            .ok();
                    }
                    Err(e) => {
                        self.db.update_status(task.id, "ci_pending")?;
                        tracing::warn!("Push retry failed for task {}: {}", task.id, e);
                        self.slack
                            .reply_thread(
                                channel,
                                thread_ts,
                                &format!(":warning: CI 修正の push に失敗: {}", e),
                            )
                            .await
                            .ok();
                    }
                }
            }
            Ok(exec_result) => {
                self.db.update_status(task.id, "ci_pending")?;
                let output_summary = truncate_for_slack(&exec_result.output, 500);
                self.slack
                    .reply_thread(
                        channel,
                        thread_ts,
                        &format!(":warning: CI 修正の実行結果が不明 — 再監視中\n```\n{}\n```", output_summary),
                    )
                    .await
                    .ok();
            }
            Err(e) => {
                self.db.update_status(task.id, "ci_pending")?;
                tracing::error!("CI fix executor error for task {}: {}", task.id, e);
                self.slack
                    .reply_thread(
                        channel,
                        thread_ts,
                        &format!(":x: CI 修正の実行中にエラー: {}{}", e, ERROR_LOG_HINT),
                    )
                    .await
                    .ok();
            }
        }

        // worktree cleanup
        workspace::remove(&ws).await.ok();

        Ok(())
    }
}
