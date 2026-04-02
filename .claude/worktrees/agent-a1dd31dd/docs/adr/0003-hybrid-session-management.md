# ADR-0003: セッション管理のハイブリッド方式

**日付**: 2026-03-14
**ステータス**: Accepted

## コンテキスト

- coding_tasks: `claude_session_id` + `--resume` でセッション継続
- ops: `ops_contexts` テーブルにテキスト履歴を蓄積してプロンプト注入
- 統合後、conversing フェーズと executing フェーズでセッション管理が異なる

## 決定

**ハイブリッド方式**: フェーズごとに最適な管理方式を使い分ける。

| フェーズ | 管理方式 | テーブル |
|---------|---------|---------|
| conversing | テキスト履歴蓄積 | ops_contexts |
| executing | claude_session_id + --resume | coding_tasks |

## 理由

- conversing は対話的（複数ターン、人間の返信を待つ）→ テキスト履歴が適切
- executing は連続実行（1セッションで完了を目指す）→ `--resume` が適切
- `executing → conversing` 遷移時に `claude_session_id` を保持 → ブロッカー解消後に `--resume` で再開可能

## 結果

- `converse_thread_ts` カラムで ops_contexts との紐付け
- conversing → executing 遷移時に最新の assistant 出力を `analysis_text` に保存
