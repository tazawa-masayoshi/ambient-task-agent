# ambient-task-agent コンテキスト

## 概要

Asanaをバックエンドとしたアンビエントタスクエージェント。
Claude Codeのhookイベントやskillをトリガーにタスク状態を自動管理し、
wez-sidebarなどのUIからタスク一覧を参照できるようにする。

## アーキテクチャ

```
Claude Code sessions
  ├─ Asana MCP (直接操作: タスク作成・更新)
  ├─ skill (/asana-sync等) → Asana MCP
  └─ hooks (SessionStart, Stop) → ambient-task-agent (Rust CLI)
                                      ↓
                                Asana REST API (PAT認証)
                                      ↓
                              JSON (stdout / file)
                                      ↓
                              wez-sidebar (TUI表示)
```

## Asana情報

- **Asana MCP**: `asana@claude-plugins-official` インストール・認証済み
- **ワークスペース**: `kcgrp.jp` (GID: `31207243604083`)
- **プロジェクト**:
  - `レボリューションズ` (GID: `1210220399981871`) — メイン開発プロジェクト
  - `ぱちタウンチャットボット` (GID: `1209044193035773`)
- **ユーザー**: 田澤雅義 (GID: `1207904792356101`)

### 利用可能なMCPツール

- `asana_typeahead_search` — クイック検索（プレミアム不要）
- `asana_get_task` — タスク詳細取得
- `asana_create_task` — タスク作成
- `asana_update_task` — タスク更新
- `asana_delete_task` — タスク削除
- `asana_get_tasks` — プロジェクト/セクション内タスク一覧
- `asana_get_projects` — プロジェクト一覧
- `asana_get_project_sections` — セクション一覧
- `asana_create_task_story` — タスクにコメント追加

### 制限事項

- `asana_search_tasks` はプレミアムプラン限定（利用不可）
- タスク検索は `asana_get_tasks`（プロジェクト単位）+ `asana_typeahead_search` で代替

## Slack情報

- **Bot Token**: `.env` の `SLACK_BOT_TOKEN`
- **テストチャンネル**: `.env` の `SLACK_TEST_CHANNEL` (C09F1T4C6F3)
- **送信テスト**: 確認済み

## 連携方式

### 1. ディレクトリ ↔ Asanaタスクの紐付け

各プロジェクトディレクトリにAsanaタスク/プロジェクトIDを関連付ける。
保存場所の候補:
- `~/.config/ambient-task-agent/mappings.json`
- 各プロジェクトの `.claude/` 内
- CLAUDE.md内にメタデータとして記載

```json
{
  "/Users/.../amu-tazawa-scripts/slack_task_runner": {
    "asana_project_id": "...",
    "asana_section_id": "..."
  }
}
```

### 2. Claude Code skill

```bash
# タスク同期: Asanaから現在のプロジェクトのタスクを取得
/asana-sync

# タスク完了報告
/asana-done
```

### 3. wez-sidebar連携

ambient-task-agentがAsanaからタスクを取得し、
ローカルJSONファイルに書き出す → wez-sidebarが読む

```
~/.config/ambient-task-agent/tasks-cache.json
```

## Rust CLI (ambient-task-agent) の役割

Asana REST API (PAT認証) を使い、以下を担当:
- タスク取得 → JSON出力 (wez-sidebar向け)
- hookから呼ばれるバックグラウンド同期
- Slack通知

Claude Codeセッション内でのタスク操作はAsana MCPを使う。

## 実装すべき機能

### Phase 1: 基本機能 ← NOW
- [x] Asana MCP動作確認
- [x] Slack送信確認
- [ ] Asana REST APIクライアント（PAT認証）
- [ ] タスク取得 (`fetch`) — プロジェクトID指定
- [ ] JSON出力（stdoutまたはファイル）
- [ ] 優先度スコア計算

### Phase 2: skill/hook連携
- [ ] `/asana-sync` skill作成
- [ ] SessionStartフックでタスク同期
- [ ] ディレクトリ↔Asanaタスク紐付け管理
- [ ] CLAUDE.mdへのタスク情報反映

### Phase 3: wez-sidebar連携
- [ ] tasks-cache.json出力
- [ ] wez-sidebarがcacheを読んで表示
- [ ] TUIからのステータス変更をAsana APIに反映

### Phase 4: アンビエント機能
- [ ] イベントに応じたタスク自動更新
  - セッション開始時: 関連タスクをin_progressに
  - Stop時: 進捗メモをAsanaコメントに記録
- [ ] コンテキスト認識: cwdからタスクを自動推定
- [ ] Agent Loop (OpenAI API) によるLLM判断ループ

## 参考: アンビエントエージェントの設計思想

- **イベントドリブン**: Claude Codeのhookイベントをトリガーに動作
- **バックグラウンド**: ユーザーの明示的な指示なしに自動処理
- **ヒューマンインザループ**: 重要な変更はwez-sidebar TUIで確認・承認
- **コンテキスト認識**: セッションのcwd、作業内容からタスクを推定
- **Asana = source of truth**: ローカルはキャッシュ、Asanaが正
