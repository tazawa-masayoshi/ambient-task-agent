# Context

> compaction で失われる文脈を保存。compaction summary と合わせて復元に使用。
> Learnings に長期的価値があれば MEMORY.md に反映すること。

### Snapshot (03/09 10:51, auto)

**Intent:** https://github.com/sipeed/picoclaw

**Outcomes:** 5 files changed
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/bedrock.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/claude.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/repo_config.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/server/slack_events.rs`
- `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent/src/worker/ops.rs`

**Context:**

".claude/scripts/add-subcategory.sh"
```

方向性は正しい。今日の変更をまとめると：

1. **ClaudeCliBackend 強化** — `--output-format json` / `--dangerously-skip-permissions` / `--no-chrome` / stdin入力 / JSON usage パース（PicoClaw + OpenFang 参考）
2. **ops の Tool 化** — `ExtraToolDispatcher` trait + `OpsToolDef` in TOML + `PARAM_xxx` 環境変数（ZeroClaw + OpenFang 参考）
3. **後方互換** — `ops_tools` 優先、`ops_skills` フォールバック
