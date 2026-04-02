# ADR-0010: Bash safeguard を builtin に残存

**日付**: 2026-04-02
**ステータス**: Accepted

## コンテキスト

Serena の autonomous-agent モードには `bash_command` / `execute_command` ツールが含まれる。しかし、`check_dangerous_command()` の safeguard ロジックは agent 側で制御する必要がある（MCP サーバー側では不十分）。

## 決定

Bash ツールは builtin（tool_impls.rs）に残存させ、MCP 経由の bash 系ツールにも同じ safeguard を `agent_loop.rs` の `check_mcp_safeguard()` で適用する。

## 根拠

セキュリティ制御はエージェント側（信頼境界の内側）で行うべき。MCP サーバーは外部プロセスであり、safeguard を移譲すると制御が分散する。

## 結果

- 二層防御: agent_loop（プロセス内）+ MCP サーバー（プロセス外サンドボックス）
- builtin Bash と MCP bash_command に同一の safeguard を適用
- `check_dangerous_command()` を `pub(crate)` に変更して agent_loop から参照可能に
