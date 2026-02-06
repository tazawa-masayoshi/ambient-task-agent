---
description: "Asanaからタスクを取得してJSONキャッシュに書き出す"
---

# /asana-sync - Asanaタスク同期

Asana MCPを使ってタスクを取得し、ローカルJSONキャッシュに書き出します。
wez-sidebarやcronからの定期実行で使用。

## 実行手順

### 1. タスク取得

Asana MCP `asana_get_tasks` を使い、レボリューションズプロジェクトのタスクを取得してください。

```
project: 1210220399981871
opt_fields: name,due_on,assignee.name,completed,notes,memberships.section.name
limit: 100
```

### 2. JSON整形・書き出し

取得したタスクを以下の形式に整形し、`~/.config/ambient-task-agent/tasks-cache.json` に書き出してください。
ディレクトリが無ければ作成してください。

```json
{
  "synced_at": "2026-02-06T10:00:00+09:00",
  "project": {
    "gid": "1210220399981871",
    "name": "レボリューションズ"
  },
  "tasks": [
    {
      "gid": "1234567890",
      "name": "タスク名",
      "assignee": "田澤雅義",
      "due_on": "2026-02-28",
      "completed": false,
      "section": "セクション名",
      "notes_preview": "最初の100文字..."
    }
  ],
  "summary": {
    "total": 10,
    "incomplete": 8,
    "my_tasks": 5,
    "overdue": 1
  }
}
```

### 3. サマリー出力

同期結果をコンソールに表示してください：

```
Asana同期完了: 全X件 (未完了: Y件, 自分: Z件, 期限超過: W件)
```

### 4. 期限超過タスクがあれば警告

`due_on` が今日より前で `completed: false` のタスクがあれば警告表示してください。

## 注意

- completedタスクは含めるが、最新20件のみ
- `notes_preview` はnotesの先頭100文字（なければ空文字）
- assigneeがnullの場合は "未割当" とする
