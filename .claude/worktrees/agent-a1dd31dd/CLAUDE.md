# ambient-task-agent

Rust 製の自律タスクエージェント。Asana/Slack からタスクを受け取り、Claude Code で自動実行する。

## ステータスモデル

```
new → executing（明確）/ conversing（曖昧）
conversing → executing / manual / done
manual → executing / done
executing → done / ci_pending / manual（stop） / conversing（ブロッカー検知）
```

旧ステータス（planning/proposed/approved/auto_approved）は廃止済み。

## Development Guidelines

- `cargo clippy -- -D warnings` がクリーンであること
- テスト: `cargo test`（現在112件）
- 設計判断は `docs/adr/` に記録
- 破壊的変更は `CHANGELOG.md` に記録
- 詳細な設計: `plan/design.md`, `plan/requirements.md`

## バックエンド切り替え (AGENT_BACKEND)

- `cli` — claude -p (ClaudeCliBackend, Claude Max サブスク定額)
- `max` — Anthropic API + OAuth (AnthropicApiBackend, Claude Max サブスク定額)
- `bedrock` — AWS Bedrock Converse API (BedrockBackend, 従量課金)
- 未指定 — ANTHROPIC_API_KEY があれば API 直叩き、なければ cli フォールバック

## テスト

- DB テストは `Connection::open_in_memory()` で Db を構築（`Db::open` はファイルパス必須）
