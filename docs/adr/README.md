# Architecture Decision Records

| ADR | 決定 | 日付 |
|-----|------|------|
| [0001](0001-db-separate-tables.md) | coding_tasks と ops_queue を DB 統合しない | 2026-03-14 |
| [0002](0002-conversing-manual-status.md) | plan/approve 廃止 → conversing/manual に統合 | 2026-03-14 |
| [0003](0003-hybrid-session-management.md) | セッション管理のハイブリッド方式 | 2026-03-14 |
| [0004](0004-append-system-prompt.md) | --system-prompt → --append-system-prompt | 2026-03-14 |
| [0005](0005-self-improvement-loop.md) | 自己改善ループ + git-ratchet | 2026-03-16 |
| [0006](0006-tokio-spawn-parallelization.md) | process_tasks() の tokio::spawn 並列化 | 2026-03-14 |
| [0007](0007-llm-classification-learning.md) | LLM 分類学習（few-shot classify） | 2026-03-16 |
