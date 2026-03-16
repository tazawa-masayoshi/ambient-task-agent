# ADR-0007: LLM 分類学習（few-shot classify with history）

**日付**: 2026-03-16
**ステータス**: Accepted

## コンテキスト

`classify_new_task()` は静的 heuristics（Slack 入口 + analysis_text 有無）で分類していた。過去のタスクから学習して精度を上げたい。

## 決定

1. **`initial_classification` + `classification_outcome` カラム**を追加し、分類結果を記録
2. **`classify_new_task_llm()`** で直近10件の分類履歴を few-shot として Claude に渡す
3. **`--json-schema`** で `{"classification": "execute" | "converse"}` を構造化出力
4. LLM 失敗時は heuristics にフォールバック

## 理由

- heuristics は静的で、タスクの文脈を理解しない
- few-shot 学習なら過去のパターンを活かせる（「このリポのタスクは converse が必要だった」等）
- classification_outcome の記録は自動（blocker 検知 → needed_converse、PR 成功 → correct）
- データが蓄積されるほど精度が向上する self-improving パターン

## 結果

- `ClassificationRecord` 構造体 + `get_recent_classification_history()`
- ブロッカー検知時に `needed_converse`、PR 作成成功時に `correct` を自動記録
- LLM コスト: 1タスクあたり classify で 1回の軽量呼び出し（max_turns=1, tools=""）
