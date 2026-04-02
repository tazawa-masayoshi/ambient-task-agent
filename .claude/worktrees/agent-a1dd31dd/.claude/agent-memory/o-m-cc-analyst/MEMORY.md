# Analyst Memory — ambient-task-agent

## プロジェクト概要
- Rust + Tokio + rusqlite (WAL) + Axum のサーバーサイドエージェント
- Slack Events API (Webhook + Socket Mode 両対応) でイベント受信
- Worker は Arc<Notify> + 15s heartbeat のループ構造
- DB ファイルは単一 SQLite、マイグレーションは `migrate()` 内 `CREATE TABLE IF NOT EXISTS` + `add_missing_columns` パターン

## 主要ファイルパス
- `/home/ec2-user/masayoshi/ambient-task-agent/src/db.rs` — 全テーブル + CRUD
- `/home/ec2-user/masayoshi/ambient-task-agent/src/worker/runner.rs` — Worker::run, process_tasks()
- `/home/ec2-user/masayoshi/ambient-task-agent/src/server/slack_events.rs` — dispatch_event, handle_message, handle_reaction_added, dispatch_ops_request
- `/home/ec2-user/masayoshi/ambient-task-agent/src/worker/ops.rs` — execute_ops (claude -p)
- `/home/ec2-user/masayoshi/ambient-task-agent/src/repo_config.rs` — RepoEntry (ops_monitor, ops_skills etc.)
- `/home/ec2-user/masayoshi/ambient-task-agent/src/server/http.rs` — AppState, wake_worker()

## 既存パターン（要件定義で継承すべき）
- DB claim パターン: 取得 → status を即更新 → tokio::spawn (二重処理防止)
- エラー時: `set_error` or `increment_retry_count` → ステータス更新
- Slack 通知: `reply_thread` で常にスレッドへ
- Worker 起床: `state.wake_worker()` / `worker_notify.notify_one()`

## テーブル一覧 (2026-03-11 時点)
- coding_tasks, webhook_events, meeting_reminders, sessions, scheduled_jobs, ops_contexts

## 注意事項
- facets/ ディレクトリは存在しない（テンプレート参照不可）
- plan/ ディレクトリは自分で作成が必要
