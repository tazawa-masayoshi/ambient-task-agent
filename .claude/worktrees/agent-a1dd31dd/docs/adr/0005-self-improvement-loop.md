# ADR-0005: 自己改善ループ + git-ratchet

**日付**: 2026-03-16
**ステータス**: Accepted

## コンテキスト

エージェントが自分自身のコードを改善する仕組みが欲しい。ただし人間が変更を確認できる必要がある（PR モデル）。

参考: autoresearch の git-ratchet パターン（改善なら keep、悪化なら reset）。

## 決定

1. **self_improvement scheduler ジョブ**（毎週月曜 10:00）
   - 分類精度・エラータスク・学習メモを分析
   - Claude が `--json-schema` で改善提案を構造化出力
   - 最大3件のタスクとして `repo_key = "self"` で self-register
   - conversing フローで人間が確認 → worktree で実装 → PR

2. **git-ratchet**（`quality_ratchet_check`）
   - PR 作成前に worktree で `cargo test` + `cargo clippy` を実行
   - テスト数が減少 or clippy warnings が増加 → PR 作成を拒否
   - 成功したら `.agent/quality-baseline.json` にベースラインを更新

## 理由

- PR モデルなら差分が見える、レビューできる、revert できる
- git-ratchet でメトリクスのモノトニックな改善を保証
- 既存の worktree + PR フローをそのまま活用（新しい仕組み不要）

## 結果

- `repos.toml` に `key = "self"` リポジトリを追加
- 分類履歴が蓄積されるほど LLM 分類の few-shot 精度が向上
- 品質ベースラインがコミットごとに更新され、regression を自動防止
