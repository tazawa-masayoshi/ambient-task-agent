# ambient-task-agent コンテキスト

## 概要

Asanaをバックエンドとしたアンビエントタスクエージェント。
Claude Codeのhookイベントやskillをトリガーにタスク状態を自動管理し、
wez-sidebarなどのUIからタスク一覧を参照できるようにする。

## アーキテクチャ

```
Claude Code sessions
  ├─ Asana MCP (直接操作: タスク作成・更新・検索)
  ├─ skill (/asana-sync等) → Asana API
  └─ hooks (SessionStart, Stop) → ambient-task-agent
                                      ↓
                                Asana REST API
                                      ↓
                              JSON (stdout / file)
                                      ↓
                              wez-sidebar (TUI表示)
```

## Asana情報

- **Asana MCP**: `asana@claude-plugins-official` インストール・認証済み
- **利用可能なMCPツール**:
  - `asana_search_tasks` — タスク検索
  - `asana_get_task` — タスク詳細取得
  - `asana_create_task` — タスク作成
  - `asana_update_task` — タスク更新
  - `asana_get_projects` — プロジェクト一覧
  - `asana_get_tasks` — プロジェクト内タスク一覧
  - その他多数

### ワークスペース・プロジェクト情報

> 注意: 初回セットアップ時にMCPで確認が必要
> `asana_list_workspaces` → `asana_get_projects`

- **ワークスペースID**: 未確認
- **タスク管理プロジェクトID**: 未確認（既存のものを使うか新規作成）

## kintone情報（参考・旧計画）

- **タスクアプリID**: 3667
- **ドメイン**: dmm-amu.cybozu.com
- **APIトークン**: 未発行
- kintoneは使わない方針に変更。Asanaに一本化。

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

## 実装すべき機能

### Phase 1: 基本機能
- [ ] Asana REST APIクライアント（またはMCP経由）
- [ ] タスク取得 (`fetch`)
- [ ] タスク更新 (`update-status`)
- [ ] 優先度スコア計算
- [ ] JSON出力（stdoutまたはファイル）

### Phase 2: skill/hook連携
- [ ] `/asana-sync` skill作成
- [ ] SessionStartフックでタスク同期
- [ ] ディレクトリ↔Asanaタスク紐付け管理
- [ ] CLAUDE.mdへのタスク情報反映

### Phase 3: wez-sidebar連携
- [ ] tasks-cache.json出力
- [ ] wez-sidebarがcacheを読んで表示
- [ ] wez-sidebarのkintone依存コードを削除・置換
- [ ] TUIからのステータス変更をAsana APIに反映

### Phase 4: アンビエント機能
- [ ] イベントに応じたタスク自動更新
  - セッション開始時: 関連タスクをin_progressに
  - Stop時: 進捗メモをAsanaコメントに記録
- [ ] コンテキスト認識: cwdからタスクを自動推定

## 参考: アンビエントエージェントの設計思想

- **イベントドリブン**: Claude Codeのhookイベントをトリガーに動作
- **バックグラウンド**: ユーザーの明示的な指示なしに自動処理
- **ヒューマンインザループ**: 重要な変更はwez-sidebar TUIで確認・承認
- **コンテキスト認識**: セッションのcwd、作業内容からタスクを推定
- **Asana = source of truth**: ローカルはキャッシュ、Asanaが正
