# Context

> compaction で失われる文脈を保存。compaction summary と合わせて復元に使用。
> Learnings に長期的価値があれば MEMORY.md に反映すること。

### Snapshot (03/11 20:54, auto)

**Intent:** Implement the following plan:

**Outcomes:** 15 files changed
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/.claude/agent-memory/o-m-cc-code-reviewer/MEMORY.md`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/.claude/chronicle.md`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/.claude/context.md`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/Cargo.toml`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/config/repos.toml`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/config/soul.md`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/spec/vision-v3.md`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/bedrock.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/claude.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/db.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/execution.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/google/calendar.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/main.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/repo_config.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/server/api.rs`

**Context:**

 execute は走らない
- CI チェックも走らない
- 新しい `new` タスクの plan も走らない

**全部止まる。**

並列化すればこうなる:

```
今: heartbeat → process_tasks(5分ブロック) → 次のheartbeat
後: heartbeat → spawn(plan) → 即return → 15秒後に次のheartbeat → spawn(execute) → ...
```

spawn した各タスクは Semaphore で `claude_max_concurrent` まで同時実行。heartbeat ループ自体は軽量なまま回り続ける。
