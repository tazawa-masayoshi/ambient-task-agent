# ambient-task-agent

Rust 製の自律タスクエージェント。Asana/Slack からタスクを受け取り、Claude Code で自動実行する。

## ステータスモデル

```
new → executing（明確）/ conversing（曖昧）
conversing → executing / manual / done
manual → executing / done
executing → done / ci_pending / conversing（ブロッカー検知）
```

旧ステータス（planning/proposed/approved/auto_approved）は廃止済み。

## Development Guidelines

- `cargo clippy -- -D warnings` がクリーンであること
- テスト: `cargo test`（現在36件）
- 設計判断は `docs/adr/` に記録
- 破壊的変更は `CHANGELOG.md` に記録
- 詳細な設計: `plan/design.md`, `plan/requirements.md`
