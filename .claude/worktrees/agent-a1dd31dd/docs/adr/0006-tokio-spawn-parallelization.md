# ADR-0006: process_tasks() の tokio::spawn 並列化

**日付**: 2026-03-14
**ステータス**: Accepted

## コンテキスト

旧 `process_tasks()` は直列 await。1タスクの `claude -p` 実行が5分かかると、heartbeat ループ全体がブロックされ、他のタスクが処理できない。

参考: zeroClaw/openClaw の「セッション内直列 + セッション間並列」パターン。

## 決定

- `Worker` を `Arc<Worker>` で共有
- 各タスクを `tokio::spawn` で並列実行
- `spawn_task()` ヘルパーで panic 検知 + DB error 復帰
- `busy` フラグ削除（spawn 方式ではブロックしないため不要）
- `google_calendar` を `tokio::sync::Mutex` で包む
- spawn 前に `update_status` で二重 pickup 防止

## 理由

- heartbeat ループは軽量のまま回り続ける（spawn して即 return）
- 既存の `Semaphore(claude_max_concurrent)` で同時実行数を制限
- plan_task は冒頭で status を更新するため次の heartbeat で再 pickup されない

## 結果

- 複数タスクの同時実行が可能に（Semaphore で上限制御）
- heartbeat 15秒間隔でもタスク処理がブロックしない
