---
description: "Asanaからタスクを取得してJSONキャッシュに書き出す"
---

# /asana-sync - Asanaタスク同期

Asana MCPを使ってタスクを取得し、ローカルJSONキャッシュに書き出します。
wez-sidebarのtasks_file形式に準拠。

## 実行手順

### 1. タスク取得

Asana MCP `asana_get_tasks` を使い、レボリューションズプロジェクトのタスクを取得してください。

```
project: 1210220399981871
opt_fields: name,due_on,assignee.name,completed,notes,memberships.section.name
limit: 100
```

### 2. JSON整形・書き出し

取得したタスクを **wez-sidebar の TasksFile 形式** に整形し、`~/.config/wez-sidebar/tasks-cache.json` に書き出してください。
ディレクトリが無ければ作成してください。

```json
{
  "tasks": [
    {
      "id": "1234567890",
      "title": "タスク名",
      "status": "pending",
      "priority": 2,
      "due_on": "2026-02-28"
    }
  ]
}
```

**フィールド変換ルール:**

| Asana フィールド | tasks-cache フィールド | 変換 |
|---|---|---|
| `gid` | `id` | そのまま |
| `name` | `title` | そのまま |
| `completed` | `status` | `true` → `"completed"`, `false` → `"pending"` |
| （なし） | `priority` | セクション名で判定: "高優先" → `1`, "進行中" → `2`(status=`"in_progress"`), その他 → `3` |
| `due_on` | `due_on` | そのまま（nullなら省略） |

### 3. サマリー出力

同期結果をコンソールに表示してください：

```
Asana同期完了: 全X件 (未完了: Y件, 期限超過: W件)
```

### 4. 期限超過タスクがあれば警告

`due_on` が今日より前で `completed: false` のタスクがあれば警告表示してください。

## 注意

- completedタスクは含めるが、最新20件のみ
- assigneeがnullの場合も含める（wez-sidebar側では使わない）
