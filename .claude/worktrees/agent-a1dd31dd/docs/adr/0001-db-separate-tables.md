# ADR-0001: coding_tasks と ops_queue を DB 統合しない

**日付**: 2026-03-14
**ステータス**: Accepted

## コンテキスト

ops/task 統合設計にあたり、`coding_tasks`（28フィールド）と `ops_queue`（12フィールド）を1テーブルに統合するか検討した。

## 決定

**DB 統合しない。** テーブルは別々に維持し、Rust の `WorkItem` enum で論理抽象化する。

## 理由

- カラム差異が大きい（28 vs 12フィールド）
- `coding_tasks.asana_task_gid` は NOT NULL 制約があり、ops 起点タスクには不自然
- 移行コストが高く、既存データの後方互換性リスクがある
- 共通処理は enum でディスパッチすれば十分

## 結果

- ops_queue パイプラインは独立して動作し続ける
- conversing フェーズの会話履歴は既存の `ops_contexts` テーブルを流用
- Slack 入口タスクは `asana_task_gid = "slack_{ts}"` のダミー GID で coding_tasks に挿入
