# ADR-0004: --system-prompt を --append-system-prompt に変更

**日付**: 2026-03-14
**ステータス**: Accepted

## コンテキスト

`claude -p` で `--system-prompt` を使うと Claude Code のデフォルトシステムプロンプトが完全に置き換えられる。o-m-cc プラグインのフック・メモリ機能がデフォルトプロンプトに依存している可能性がある。

## 決定

`--system-prompt` → `--append-system-prompt` に変更。Claude Code のデフォルト動作を保持しつつ、soul + rules を追記する。

## 理由

- o-m-cc の SessionStart hook、compaction hook、agent-memory がデフォルトプロンプト内の仕組みで動く
- `--append-system-prompt` なら Claude Code のベストプラクティス（ツール使用ガイド等）も保持される
- `CLAUDE_NON_INTERACTIVE=1` と組み合わせて sisyphus の headless モードも正常動作

## 結果

- `claude.rs` の `ClaudeCliBackend::run()` で `--append-system-prompt` を使用
- `CLAUDE_NON_INTERACTIVE=1` を `resolved_env` に固定注入
