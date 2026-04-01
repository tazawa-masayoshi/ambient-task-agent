# Context

> セッション間の引き継ぎ情報。学びは MEMORY.md、タスクは TaskList、設定は CLAUDE.md。

### Snapshot (04/01 14:50, end)

**Intent:** Context Rot 検知 + 失敗パターンフィードバック実装、pi-mono 風ハーネス移行の調査・設計

**Outcomes:**
- Context Rot 検知（skill mtime + ops 成功率 → Slack 通知）実装完了
- 失敗パターンフィードバック（DB 蓄積 → システムプロンプト注入）実装完了
- OPS_RESULT: failed の outcome バグ修正
- Quality Gate 通過（Review Council + 112 テスト全パス）、push 済み
- pi-mono 風ハーネス移行調査: AGENT_BACKEND=max で OAuth 経由 API 直叩きが既に可能と判明
- ClaudeCliBackend → AnthropicApiBackend 移行のギャップ分析完了

**Changed Files:**
- `src/db.rs` — ops_failure_patterns + context_rot_notifications テーブル、DB メソッド 5 件、テスト 5 件
- `src/worker/context_rot.rs` — 新規: 陳腐化判定 scan_all_repos + format_rot_alert
- `src/worker/ops.rs` — execute_ops に failure_context 引数追加
- `src/worker/runner_ops.rs` — outcome バグ修正 + 失敗サマリ保存 + 失敗パターン注入（サニタイズ付き）
- `src/worker/scheduler.rs` — context_rot_scan ジョブ + SchedulerContext.repos_config 追加
- `src/worker/runner.rs` — SchedulerContext 構築に repos_config 追加
- `src/worker/mod.rs` — pub mod context_rot 追加

**Next:**
- `AGENT_BACKEND=max` で EC2 デプロイ・動作確認（ClaudeRunner 廃止の実証）
- ギャップ対応: fallback_model の AnthropicApiBackend 実装、空出力リカバリの代替実装
- 動作確認後、ClaudeCliBackend を非推奨化
