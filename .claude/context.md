# Context

> セッション間の引き継ぎ情報。学びは MEMORY.md、タスクは TaskList、設定は CLAUDE.md。

### Snapshot (04/06 17:20, end)

**Intent:** claw-code/Claude Code パターンを参考にエージェントハーネスを強化 + ops の実運用テスト

**Outcomes:**
- ツール並行安全性: Read系=並列、Write系+SubAgent=直列
- 3段階コンテキスト圧縮: Micro-compact + Hard Truncate (drain ベース)
- Deferred Tool Loading: ToolSearch + build_deferred_tool_definitions
- OAuth refresh 自動回復: 失敗時にファイルから最新トークンを再読み込み
- ops トリガー簡素化: 6分岐→3分岐（@bot/@admin メンションのみ）
- 検証ターン強化: 依頼完了確認 + gws 自動化チェック + ターンカウント除外
- CTA パターン: hikken/send_survey_mail/favorite_pop の .claude/commands/ops/bin/ スクリプト
- repos.toml 読み込み: cwd/config/ 優先に変更（~/.config/ フォールバック）
- max_turns=100、max_tokens_per_turn=32000

**Next:**
- Serena MCP 導入（rust-analyzer + pylsp インストール）
- チャンネルフリールーティング（チャンネルマッピング廃止検討）
- ops スキルの継続改善（依頼ごとにフィードバック）
