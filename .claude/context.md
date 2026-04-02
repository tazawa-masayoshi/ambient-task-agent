# Context

> セッション間の引き継ぎ情報。学びは MEMORY.md、タスクは TaskList、設定は CLAUDE.md。

### Snapshot (04/02 18:00, end)

**Intent:** MCP ベース自律エージェントアーキテクチャへの移行（Phase 0-1）+ 共有 crate 統合

**Outcomes:**
- Phase 0 完了: ClaudeCliBackend 削除（459行削減）、OAuth → API Key → Bedrock フォールバック
- Phase 1 途中: resolve_tools()、McpConfigBuilder、McpServerConfig::resolve_env() 実装済み
- 共有 crate 統合: claude-auth / slack-socket / slack-mrkdwn → 595行削減
- OAuth テスト成功（system block 注入 + stream:false が必要と判明）
- 残りタスク: #21-#28（check_mcp_safeguard, ops.rs MCP 連携, Serena 設定, ContentRouter, 統合テスト）

**Next:**
- #21: check_mcp_safeguard() 実装（agent_loop.rs に MCP bash safeguard）
- #22: ops.rs から ClaudeRunner に mcp_servers を渡す（McpConfigBuilder 使用）
- #23: repos.toml に Serena 設定追加（self リポジトリで検証）
- Phase 2 以降: ContentRouter, GitHub/kintone MCP, レートリミット対応

**Changed Files:**
- `src/claude.rs` — ClaudeCliBackend + CLI パース型を削除（998→487行）
- `src/main.rs` — build_agent_backend() を OAuth → API Key → Bedrock フォールバックに
- `src/anthropic/backend.rs` — resolve_tools() 追加（Serena 有無で builtin/MCP 切り替え）
- `src/anthropic/mcp.rs` — resolve_env() 追加（${VAR} テンプレート展開）
- `src/anthropic/mcp_config.rs` — 新規（McpConfigBuilder, serena_config）
- `src/anthropic/client.rs` — claude-auth crate アダプター化
- `src/anthropic/types.rs` — SSE 型を crate に移動
- `src/anthropic/oauth.rs` — 削除（crate に統一）
- `src/slack/mrkdwn.rs` — 削除（crate に統一）
- `plan/design.md` — MCP アーキテクチャ設計書
- `plan/requirements.md` — 要件定義
