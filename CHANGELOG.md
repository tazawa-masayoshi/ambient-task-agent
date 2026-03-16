# Changelog

破壊的変更を含むアップデートの記録。設計判断の詳細は [`docs/adr/`](docs/adr/) を参照。

## [Unreleased]

### Breaking Changes
- **旧 plan/approve フロー完全削除** — `planning`, `proposed`, `approved`, `auto_approved` ステータスを廃止。v12 マイグレーションで自動変換。
- **analyzer.rs 削除** — 旧 `plan_task()`, `PlanResult`, `extract_complexity()` を削除。
- **approve_task/reject_task/regenerate_task ボタン削除** — 新ボタン `task_execute/task_manual/task_skip/task_add_instruction/task_resume/task_done` に置き換え。
- **🤖 リアクション即実行を削除** — `go`/`実行` テキストコマンドまたは `[実行開始]` ボタンに統一。
- **`--system-prompt` → `--append-system-prompt`** — Claude Code のデフォルトシステムプロンプトを保持。
- **`set_error()` が `'error'` を設定**（旧: `'failed'`）。
- **`execute_approved_task()` → `execute_task()` にリネーム**。

### Added
- **conversing/manual ステータス** — 曖昧なタスクは Slack ラリーで要件確定、手動修正モードで人間が terminal 対応。
- **`classify_new_task_llm()`** — 過去の分類履歴を few-shot で渡して LLM 分類。heuristics フォールバック付き。
- **`classification_outcome` 自動記録** — ブロッカー検知時に `needed_converse`、PR 成功時に `correct`。
- **ブロッカー検知** — executor 出力の `BLOCKED:`/`REQUIRES_CLARIFICATION:` で `executing → conversing` 遷移。
- **conversing タイムアウト** — 営業日5日返信なし → `sleeping`。
- **self_improvement ジョブ** — 毎週月曜に分類精度・エラー・memory を分析して改善 PR を提案。
- **git-ratchet** — PR 作成前にテスト数・clippy warnings の悪化を防止。
- **Bottom-up Skill Discovery** — `SKILL_CANDIDATE:` 出力で繰り返しパターンを検出、成熟したら通知。
- **分析テンプレート外部ファイル化** — `config/self-improvement-template.md` で分析観点を管理。
- **`--json-schema` 構造化出力** — `route_ops()` で構造化分類。
- **`CLAUDE_NON_INTERACTIVE=1`** — sisyphus headless モード対応。
- **ops フォローアップ土日除外** — 営業日ベースのカウント。
- **ADR** — `docs/adr/` に主要設計判断7件を記録。
- **Inception 即実行** — `ops_inception_approve` → `executing` で直接登録（二重分析廃止）。
- **`source` カラム** — タスク入口識別（`asana`/`slack`/`manual`）。
- **`skill_candidates` テーブル** — スキル候補の蓄積・成熟度追跡。

## v0.2.0 (2026-03-11)

### Breaking Changes
- **decomposer.rs 削除** — サブタスク分解機能を廃止。Plan/Act mode に統合。
- **`--subtask-start`/`--subtask-done` CLI 引数削除**。
- **heartbeat 60s → 15s** — Bedrock 廃止に伴い短縮。

### Added
- **Plan/Act mode 全自動化** — `plan_task()` → `execute_in_worktree()` の2ステップ。`--resume session_id` 継続。
- **worktree 隔離実行** — 全 repo_entry で worktree 実行に統一。
- **tokio::spawn 並列化** — `process_tasks()` で各タスクを並列実行。`Semaphore` で同時実行数制限。
- **CI 監視 + 自動リトライ** — `ci_pending` → CI 結果確認 → 失敗時に worktree 再作成して修正。
- **WORKFLOW.md** — per-repo の実行設定（max_execute_turns, allowed_tools）。

## v0.1.0 (2026-03-05)

### Added
- **初期実装** — Asana webhook → coding_tasks → claude -p → Slack 通知。
- **ops チャンネル監視** — `@bot` メンション or ⚡リアクションで ops 実行。
- **Inception モード** — AI-DLC 軽量版。2ターンで要件定義 → タスク分解。
- **スケジューラ** — 朝会ブリーフィング、夕会サマリー、停滞チェック、タイムボクシング。
- **Google Calendar 連携** — 空き時間にタスクを自動配置。
- **Socket Mode** — Slack WebSocket 接続。
