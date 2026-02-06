---
description: "Asanaタスクを完了にしてSlack通知"
---

# /asana-done - タスク完了報告

指定したAsanaタスクを完了にし、Slackに通知します。

## 引数

$ARGUMENTS にタスク名またはGIDを指定してください。
例: `/asana-done 総務チャットbot`

## 実行手順

### 1. タスク特定

$ARGUMENTS が数字のみならGIDとして扱い `asana_get_task` で取得。
それ以外なら `asana_typeahead_search` でタスクを検索し、候補を表示して確認。

### 2. 完了に更新

`asana_update_task` でタスクを完了に:

```
task_id: <特定したGID>
completed: true
```

### 3. Slack通知

SlackのテストチャンネルにRust CLIで通知:

```bash
cd /Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent
cargo run -- slack-test -m "✅ タスク完了: <タスク名>"
```

### 4. キャッシュ更新

`/asana-sync` を実行してローカルキャッシュを更新。
