# Security Reviewer Memory

## Project Threat Model
- Rust + Axum Web サーバー、Slack Webhook/Events API、SQLite (rusqlite)、subprocess `claude -p`、Asana API
- 信頼境界: Slack 署名検証 (/webhook/slack は実装済み、/slack/actions は未実装)
- 攻撃面: Slack Interactivity エンドポイント、subprocess コマンド実行、ファイル書き出し

## Known Patterns
- `slack_signing_secret` は `Option<String>` — None の場合は署名検証をスキップする実装 (fail-open)
- `/slack/actions` エンドポイントには署名検証なし (Critical)
- HMAC 比較が `computed == expected` (文字列比較) — タイミング攻撃リスクがある (Warning)
- `Command::new("claude").args(["-p", &prompt, ...])` — args() 経由なのでシェルインジェクションは発生しない
- `TASK_COLUMNS` は定数 → format!() 内 SQL インジェクションは発生しない
- `add_missing_columns` の table/col 名は全てハードコード定数 → 動的入力なし

## Accepted Risks
- subprocess `claude -p` はユーザー入力をプロンプトとして渡す。シェル展開なし (args()使用) のためコマンドインジェクションは非該当だが、プロンプトインジェクションは設計上の受容リスク

## Calibration
- 初回レビュー。過剰/見逃し傾向はまだ不明。
