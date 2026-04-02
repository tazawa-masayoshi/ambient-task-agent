---
name: MCP ファーストアーキテクチャ移行のタスク分解
description: ambient-task-agent の MCP 移行で使ったタスク粒度・依存関係パターン（Phase 0-3、9タスク）
type: project
---

## 移行の全体像

Phase 0 → Phase 1 → Phase 2 → Phase 3 の順に段階的移行。各 Phase でロールバック可能。

| Phase | タスク数 | 内容 |
|-------|---------|------|
| Phase 0 | 2 | ClaudeCliBackend 削除 + build_agent_backend() デフォルト変更 |
| Phase 1 | 6 | MCP 基盤実装（resolve_env, McpConfigBuilder, resolve_tools, safeguard, ops統合, repos.toml） |
| Phase 2 | 3 | ContentRouter 実装 + レートリミット対応 |
| Phase 3 | 2 | ADR 記録 + 統合テスト |

## タスク粒度の基準（このプロジェクト実績）

- **S（小）**: 1 関数・1 メソッドの追加（`resolve_env()`、`check_mcp_safeguard()` 等）— 1-2時間
- **M（中）**: 新規ファイル作成・既存ロジックの移行（`McpConfigBuilder`、`ContentRouter`）— 半日
- **L（大）**: クロスカット変更（複数ファイルの連鎖変更）— 1日以上。なるべく M 以下に分割

## 依存関係パターン

- 「削除タスク」は後続全体のブロッカー（0-1: ClaudeCliBackend 削除が Phase 1 全体を解放）
- 「低レベル基盤」（resolve_env）→「高レベル組み立て」（McpConfigBuilder）→「呼び出し側統合」（ops.rs）の順
- 独立した横断タスク（ADR 記録）は依存なしで並列実行可能
- 統合テストは複数タスクの完了を待つ最後のゲートとして設定

## エージェント選択の実績

- Rust 実装タスク → `general-purpose`（backend.rs、agent_loop.rs 等）
- 設計記録・文書化 → `general-purpose`（ADR ファイル作成）
- 統合テスト・動作確認 → `code-reviewer` が適切だが `general-purpose` でも可

## 「既知の不足」の扱い方

design.md §8 の未解決項目への対応:
- 実装する場合: 明示的なタスクを作成（例: 2-3 レートリミット対応）
- 当面見送る場合: 最も近い関連タスクの description に「見送り理由」を記録
- Google Workspace MCP 化・Slack MCP 統合は見送り（Phase 2 以降に再検討）

**Why:** design.md の「既知の不足」は Planner の入力指示に明示されているため、全項目を明示的に扱う必要がある。
**How to apply:** 次回の MCP 関連タスク分解でも同じパターンを適用する。
