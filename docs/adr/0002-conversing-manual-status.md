# ADR-0002: plan/approve を廃止し conversing/manual ステータスに統合

**日付**: 2026-03-14
**ステータス**: Accepted

## コンテキスト

旧フロー: `new → planning → proposed → approved → executing → done`
- planning/proposed は常に人間承認を経るため、明確なタスクでも無駄な待ち時間が発生
- Devin/Symphony 型のフローを採用したい（明確なら即実行、曖昧なら Slack ラリー）

## 決定

`planning`, `proposed`, `approved`, `auto_approved` を廃止し、`conversing` と `manual` を新設。

```
new → executing（明確）/ conversing（曖昧）
conversing → executing / manual / done
manual → executing / done
executing → done / ci_pending / conversing（ブロッカー）
```

## 理由

- 入口（Slack/Asana）に関わらず同じフロー
- エージェントが自律的に「明確/曖昧」を判断（LLM classify + heuristics fallback）
- 人間は必要な時だけ介入（conversing でのラリー、manual での直接修正）

## 結果

- v12 マイグレーションで既存タスクのステータスを変換
- 旧ボタン（approve/reject/regenerate）はマイグレーション完了まで並存
- Inception 経由のタスクは `executing` で直接登録（二重分析廃止）
