# ADR-0008: MCP ファーストアーキテクチャへの移行

**日付**: 2026-04-02
**ステータス**: Accepted

## コンテキスト

ambient-task-agent は tool_impls.rs にハードコードされた 6 つの builtin ツール（Read/Write/Edit/Bash/Glob/Grep）に依存している。セマンティックなコード理解がなく、ファイル全体を読み込むためトークン効率が悪い。

## 決定

Pragmatic 案を採用: Serena 有無で builtin/MCP を動的に切り替え、Phase 0-4 の段階的移行を行う。

## 検討した代替案

1. **Minimal 案**: builtin を維持し Serena を横並びに追加 → ツール重複が問題
2. **Clean 案**: builtin 全廃止、全て MCP に移行 → 一括移行のリスクが高い

## 結果

- NFR-1（既存 ops 維持）と NFR-2（段階的移行）を満たす
- 各 Phase 単位でテスト・ロールバック可能
- 移行期間中は builtin + MCP の二重ロジックが存在（許容）
