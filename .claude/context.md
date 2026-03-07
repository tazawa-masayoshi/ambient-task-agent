# Context

> compaction で失われる文脈を保存。compaction summary と合わせて復元に使用。
> Learnings に長期的価値があれば MEMORY.md に反映すること。

### Snapshot (03/10 00:53, auto)

**Intent:** Implement the following plan:

**Outcomes:** 12 files changed
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/.claude/context.md`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/config/repos.toml`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/config/soul.md`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/db.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/google/calendar.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/repo_config.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/server/slack_events.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/worker/mod.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/worker/runner.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/worker/scheduler.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/worker/workspace.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/tests/test_gcal_create.py`

**Context:**

ープが `calendar.readonlyevents`（旧 + 新が結合されたマージアーティファクト）になっています。まず修正してから並列レビューを実行します。
実ファイルは `calendar.events` で正しい（diff の表示が jj の old→new concatenation による誤読）。次に3エージェント並列レビューを実行します。
soul.md とrepos.toml の実ファイルを確認します。diff に古い内容が残っているように見えます。
