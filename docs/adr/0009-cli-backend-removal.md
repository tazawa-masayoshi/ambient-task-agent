# ADR-0009: ClaudeCliBackend の廃止

**日付**: 2026-04-02
**ステータス**: Accepted

## コンテキスト

`ClaudeCliBackend` は `claude -p` CLI を子プロセスとして実行する。`AnthropicApiBackend`（claude-auth crate 経由の OAuth）が安定稼働しており、CLI 依存の理由がなくなった。CLI 固有のバグ回避策（verbose JSON パース等）がコードを複雑にしている。

## 決定

`ClaudeCliBackend` を完全削除。OAuth 失敗時は `ANTHROPIC_API_KEY` → Bedrock の順でフォールバック。

## 検討した代替案

1. **CLI 残存案**: フォールバックとして維持 → デッドコードの保守負担、MCP 対応が CLI では困難

## 結果

- コードの大幅簡素化（459行削減）
- MCP 対応が統一的に可能
- OAuth トークン切れ時は API Key / Bedrock でカバー
